#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = ["asyncpg", "pyarrow", "tabulate"]
#
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""The trigger outbox of ADR-0018 against a throwaway PostgreSQL, at the
major STRAUM_TEST_POSTGRES names, 18 by default. One run, no flags.

First the checks of what the design claims, some with a planted break.

capture: a statement's whole rows with their types, through renamed,
dropped, retyped and added columns; and what writes nothing but counts
as skipped: a TRUNCATE, a statement over the row cap, an expired sign.
lock: the second writer of a row gets the later id. Planted: CACHE 20.
deferred: a deferred unique key lets a new key in before the old one out.
drain: a low id that commits late is not lost. Planted: a range delete.
order: concurrent writers on hot rows, drained while they run, applied as
a multiset in outbox order; an out row never applies before its in row,
and the result equals the table. Planted: the id sequence with CACHE 20.
horizon: poll cost while a snapshot is held open, and after.

Then the taxi workload. January's trips are loaded; the app then writes
February's real trips in small batches from 8 workers, with tip
corrections and vendor changes mixed in, then a week's fare repricing, a
day's payment fix, a delete of negative fares and a zone's borough
renamed. It runs once without the triggers and once with them and a
consumer that keeps a hash of every row the outbox says the tables hold,
which must equal PostgreSQL's. A statement the triggers skip costs the
consumer a reload, as it would Straum. Each phase reports its time,
statement latency, the write-ahead log (WAL) written to the app's tables
and to the outbox, and the outbox's backlog and size.

Not here yet: a start while writers run, and whole transactions only.

Needs Docker and uv. Downloads go to bench/data, which git ignores.
"""

import asyncio
import collections
import decimal
import hashlib
import json
import pathlib
import random
import secrets
import statistics
import struct
import subprocess
import sys
import time
import zoneinfo

import asyncpg
import pyarrow.parquet
import tabulate
from load_taxi_trips import COLUMNS, TLC, download
from stack import POSTGRES, POSTGRES_IMAGE, POSTGRES_PORT, Stack, postgres_service

OUTBOX = """
CREATE SCHEMA straum;
CREATE SEQUENCE straum.sign;
CREATE SEQUENCE straum.skipped;
CREATE TABLE straum.outbox (
  id       bigint GENERATED ALWAYS AS IDENTITY (CACHE {cache}) PRIMARY KEY,
  relid    oid     NOT NULL,
  incoming boolean NOT NULL,
  rows     bytea   NOT NULL
);

CREATE FUNCTION straum.capture() RETURNS trigger LANGUAGE plpgsql
SECURITY DEFINER SET search_path = pg_catalog, pg_temp SET client_encoding = 'UTF8'
AS $f$
DECLARE
  changed bigint;
BEGIN
  IF TG_OP = 'TRUNCATE' THEN
    PERFORM nextval('straum.skipped');
    RETURN NULL;
  ELSIF TG_OP = 'DELETE' THEN
    SELECT count(*) INTO changed FROM (SELECT FROM old_rows LIMIT {cap} + 1) c;
  ELSE
    SELECT count(*) INTO changed FROM (SELECT FROM new_rows LIMIT {cap} + 1) c;
  END IF;
  IF changed = 0 THEN
    RETURN NULL;
  END IF;
  IF changed > {cap}
     OR (SELECT last_value FROM straum.sign) < extract(epoch FROM clock_timestamp()) THEN
    PERFORM nextval('straum.skipped');
    RETURN NULL;
  END IF;
  IF TG_OP <> 'INSERT' THEN
    INSERT INTO straum.outbox (relid, incoming, rows)
    SELECT TG_RELID, false, string_agg(record_send(o), ''::bytea) FROM old_rows o;
  END IF;
  IF TG_OP <> 'DELETE' THEN
    INSERT INTO straum.outbox (relid, incoming, rows)
    SELECT TG_RELID, true, string_agg(record_send(n), ''::bytea) FROM new_rows n;
  END IF;
  RETURN NULL;
END
$f$;
"""

TRIGGERS = """
CREATE TRIGGER {t}_ins AFTER INSERT ON {t} REFERENCING NEW TABLE AS new_rows
  FOR EACH STATEMENT EXECUTE FUNCTION straum.capture();
CREATE TRIGGER {t}_upd AFTER UPDATE ON {t}
  REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows
  FOR EACH STATEMENT EXECUTE FUNCTION straum.capture();
CREATE TRIGGER {t}_del AFTER DELETE ON {t} REFERENCING OLD TABLE AS old_rows
  FOR EACH STATEMENT EXECUTE FUNCTION straum.capture();
CREATE TRIGGER {t}_trunc AFTER TRUNCATE ON {t}
  FOR EACH STATEMENT EXECUTE FUNCTION straum.capture();
"""

DRAIN = """
DELETE FROM straum.outbox
 WHERE id IN (SELECT id FROM straum.outbox ORDER BY id LIMIT {limit})
RETURNING id, relid, incoming, rows
"""
# The sign as seconds since 1970: one far ahead, and one long past.
FRESH = "SELECT setval('straum.sign', 4102444800)"
EXPIRED = "SELECT setval('straum.sign', 1)"
SKIPPED = "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM straum.skipped"
# A count and a 128-bit sum of md5s over a table's rows as the triggers
# send them: equal for two tables holding the same rows.
TALLY = """
SELECT count(*)::int AS rows,
       coalesce(sum(('x' || substr(h, 1, 16))::bit(64)::bigint::numeric), 0) AS high,
       coalesce(sum(('x' || substr(h, 17, 16))::bit(64)::bigint::numeric), 0) AS low
  FROM (SELECT md5(record_send(t)) AS h FROM {table} t) hashed
"""

HOT_KEYS = 20
# The most rows a statement may change and still be sent, as
# --max-transaction-rows is.
ROW_CAP = 100_000
INT4, TEXT, VARCHAR = 23, 25, 1043
RESULTS: list[tuple[str, str, str]] = []


def record(check: str, passed: bool, detail: str) -> None:
    RESULTS.append((check, "PASS" if passed else "FAIL", detail))
    print(f"{check}: {'PASS' if passed else 'FAIL'} {detail}", flush=True)


def records(rows: bytes) -> list[bytes]:
    """The rows of an outbox value, each as `record_send` wrote it: a
    column count, then for each column a type OID, a length and that many
    bytes; a NULL has length -1 and none."""
    found = []
    at = 0
    while at < len(rows):
        start = at
        (columns,) = struct.unpack_from(">i", rows, at)
        at += 4
        for _ in range(columns):
            (length,) = struct.unpack_from(">i", rows, at + 4)
            at += 8 + max(length, 0)
        found.append(rows[start:at])
    return found


def fields(row: bytes) -> list[tuple[int, bytes | None]]:
    """A row's columns: each type OID and its bytes, None for NULL."""
    (columns,) = struct.unpack_from(">i", row, 0)
    at = 4
    found = []
    for _ in range(columns):
        oid, length = struct.unpack_from(">Ii", row, at)
        at += 8
        found.append((oid, None if length < 0 else row[at : at + length]))
        at += max(length, 0)
    return found


def number(value: bytes | None) -> int | None:
    return None if value is None else int.from_bytes(value, "big", signed=True)


def numbers(outbox_row: asyncpg.Record, column: int) -> list[int | None]:
    """One integer column of every row in an outbox row."""
    return [number(fields(row)[column][1]) for row in records(outbox_row["rows"])]


async def fresh(dsn: str, cache: int) -> asyncpg.Connection:
    """A database with only the outbox, its function and a fresh sign."""
    admin = await asyncpg.connect(dsn)
    await admin.execute("DROP DATABASE IF EXISTS t WITH (FORCE)")
    await admin.execute("CREATE DATABASE t")
    await admin.close()
    conn = await asyncpg.connect(db(dsn))
    await conn.execute(OUTBOX.format(cache=cache, cap=ROW_CAP))
    await conn.execute(FRESH)
    return conn


def db(dsn: str) -> str:
    return dsn.rsplit("/", 1)[0] + "/t"


async def drain(conn: asyncpg.Connection, limit: int = 200) -> list[asyncpg.Record]:
    got = await conn.fetch(DRAIN.format(limit=limit))
    return sorted(got, key=lambda record: record["id"])


async def check_capture(dsn: str) -> None:
    conn = await fresh(dsn, 1)
    await conn.execute(
        "CREATE TABLE items (id int PRIMARY KEY, grp int, amount int,"
        " note text, extra text)"
    )
    await conn.execute(TRIGGERS.format(t="items"))

    async def written(sql: str) -> list[asyncpg.Record]:
        await conn.execute(sql)
        return await drain(conn)

    def shape(got: list[asyncpg.Record]) -> list[tuple[bool, int]]:
        return [(row["incoming"], len(records(row["rows"]))) for row in got]

    got = await written(
        "INSERT INTO items VALUES (1,1,10,'a','x'),(2,1,20,'b','x'),(3,2,NULL,'c','x')"
    )
    types = [oid for oid, _ in fields(records(got[0]["rows"])[0])] if got else None
    amounts = numbers(got[0], 2) if got else None
    record(
        "capture/insert",
        shape(got) == [(True, 3)]
        and types == [INT4, INT4, INT4, TEXT, TEXT]
        and sorted(amounts, key=str) == [10, 20, None],
        f"outbox rows {shape(got)}, types {types}, amounts {amounts}",
    )
    got = await written("DELETE FROM items WHERE false")
    record("capture/zero-rows-writes-nothing", got == [], f"outbox rows {shape(got)}")
    got = await written("UPDATE items SET note = note || '!'")
    record(
        "capture/every-change-is-sent",
        shape(got) == [(False, 3), (True, 3)],
        f"outbox rows {shape(got)}",
    )
    got = await written("UPDATE items SET amount = amount + 1 WHERE id IN (1, 2)")
    out = sorted(numbers(got[0], 2)) if got else None
    into = sorted(numbers(got[-1], 2)) if got else None
    record(
        "capture/update-out-then-in",
        shape(got) == [(False, 2), (True, 2)] and out == [10, 20] and into == [11, 21],
        f"outbox rows {shape(got)}, amounts out {out}, in {into}",
    )
    await conn.execute("ALTER TABLE items RENAME amount TO amt")
    got = await written("UPDATE items SET amt = amt + 1 WHERE id = 1")
    record(
        "capture/column-renamed",
        shape(got) == [(False, 1), (True, 1)],
        f"outbox rows {shape(got)}",
    )
    await conn.execute("ALTER TABLE items DROP COLUMN extra")
    await conn.execute("ALTER TABLE items ALTER note TYPE varchar(20)")
    await conn.execute("ALTER TABLE items ADD COLUMN more int")
    got = await written("UPDATE items SET amt = amt + 1 WHERE id = 1")
    types = [oid for oid, _ in fields(records(got[-1]["rows"])[0])] if got else None
    record(
        "capture/dropped-retyped-added",
        shape(got) == [(False, 1), (True, 1)]
        and types == [INT4, INT4, INT4, VARCHAR, INT4],
        f"outbox rows {shape(got)}, types {types}",
    )
    got = await written(
        "INSERT INTO items (id, grp, amt) VALUES (1, 9, 9) ON CONFLICT (id)"
        " DO UPDATE SET amt = items.amt + 1"
    )
    record(
        "capture/on-conflict-update",
        shape(got) == [(False, 1), (True, 1)],
        f"outbox rows {shape(got)}",
    )

    async def skips(sql: str) -> tuple[list[tuple[bool, int]], int]:
        before = await conn.fetchval(SKIPPED)
        got = await written(sql)
        return shape(got), await conn.fetchval(SKIPPED) - before

    over = f"INSERT INTO items (id, grp, amt) SELECT g, 1, 1 FROM generate_series(10, {10 + ROW_CAP}) g"
    wrote, skipped = await skips(over)
    record(
        "capture/over-the-row-cap-skips",
        wrote == [] and skipped == 1,
        f"{ROW_CAP + 1} rows: outbox rows {wrote}, skipped {skipped}",
    )
    wrote, skipped = await skips("DELETE FROM items WHERE id > 10")
    record(
        "capture/at-the-row-cap-is-sent",
        wrote == [(False, ROW_CAP)] and skipped == 0,
        f"{ROW_CAP} rows: outbox rows {wrote}, skipped {skipped}",
    )
    wrote, skipped = await skips("TRUNCATE items")
    record(
        "capture/truncate-skips",
        wrote == [] and skipped == 1,
        f"outbox rows {wrote}, skipped {skipped}",
    )
    await conn.execute(EXPIRED)
    wrote, skipped = await skips("INSERT INTO items (id, grp, amt) VALUES (1, 1, 1)")
    await conn.execute(FRESH)
    again, none = await skips("UPDATE items SET amt = 2 WHERE id = 1")
    record(
        "capture/expired-sign-skips",
        wrote == [] and skipped == 1 and again == [(False, 1), (True, 1)] and none == 0,
        f"expired: outbox rows {wrote}, skipped {skipped}; fresh again: {again}",
    )
    await conn.close()


async def worker(dsn: str, rng: random.Random, statements: int) -> int:
    conn = await asyncpg.connect(dsn)
    failed = 0
    for _ in range(statements):
        key = rng.randint(1, HOT_KEYS)
        choice = rng.random()
        if choice < 0.25:
            sql = (
                f"INSERT INTO items VALUES ({key}, {rng.randint(1, 3)},"
                f" {rng.randint(1, 9)}, 'n') ON CONFLICT (id)"
                " DO UPDATE SET amount = items.amount + 1"
            )
        elif choice < 0.55:
            sql = f"UPDATE items SET amount = amount + {rng.randint(1, 5)} WHERE id = {key}"
        elif choice < 0.65:
            sql = (
                f"UPDATE items SET amount = amount + 1 WHERE grp = {rng.randint(1, 3)}"
            )
        elif choice < 0.75:
            sql = f"UPDATE items SET note = md5(random()::text) WHERE id = {key}"
        elif choice < 0.9:
            sql = f"DELETE FROM items WHERE id = {key}"
        else:
            other = rng.randint(1, HOT_KEYS)
            sql = f"UPDATE items SET grp = grp % 3 + 1 WHERE id IN ({key}, {other})"
        try:
            async with conn.transaction():
                await conn.execute(sql)
                if rng.random() < 0.3:
                    again = rng.randint(1, HOT_KEYS)
                    await conn.execute(
                        f"UPDATE items SET amount = amount + 1 WHERE id = {again}"
                    )
                await asyncio.sleep(rng.random() * 0.002)
        except (asyncpg.DeadlockDetectedError, asyncpg.UniqueViolationError):
            failed += 1
    await conn.close()
    return failed


async def order_run(dsn: str, cache: int, seed: int) -> tuple[int, bool, int]:
    """(out rows that found nothing to take, table equals the multiset,
    outbox rows applied)."""
    setup = await fresh(dsn, cache)
    await setup.execute(
        "CREATE TABLE items (id int PRIMARY KEY, grp int, amount numeric, note text)"
    )
    await setup.execute(TRIGGERS.format(t="items"))
    rng = random.Random(seed)
    tasks = [
        asyncio.create_task(worker(db(dsn), random.Random(rng.random()), 400))
        for _ in range(8)
    ]
    consumer = await asyncpg.connect(db(dsn))
    held: collections.Counter = collections.Counter()
    missing = 0
    applied = 0
    while True:
        done = all(task.done() for task in tasks)
        got = await drain(consumer)
        for row in got:
            applied += 1
            for sent in records(row["rows"]):
                if row["incoming"]:
                    held[sent] += 1
                elif held[sent] == 0:
                    missing += 1
                else:
                    held[sent] -= 1
        if done and not got:
            break
        if not got:
            await asyncio.sleep(0.005)
    await asyncio.gather(*tasks)
    table = collections.Counter(
        r["sent"]
        for r in await consumer.fetch("SELECT record_send(i) AS sent FROM items i")
    )
    skipped = await consumer.fetchval(SKIPPED)
    await consumer.close()
    await setup.close()
    return missing, +held == table and skipped == 0, applied


async def check_order(dsn: str, seeds: int) -> None:
    for cache, label in ((1, "order"), (20, "order/planted-cache-20")):
        lines = []
        bad = 0
        for seed in range(seeds):
            missing, equal, applied = await order_run(dsn, cache, seed)
            lines.append(
                f"seed {seed}: {applied} outbox rows, {missing} early outs, equal {equal}"
            )
            bad += missing > 0 or not equal
        expect_fail = cache != 1
        record(label, (bad > 0) == expect_fail, "; ".join(lines))


async def wait_for_lock(conn: asyncpg.Connection, pid: int) -> None:
    for _ in range(500):
        waiting = await conn.fetchval(
            "SELECT wait_event_type = 'Lock' FROM pg_stat_activity WHERE pid = $1", pid
        )
        if waiting:
            return
        await asyncio.sleep(0.01)
    raise SystemExit(f"session {pid} never waited on a lock")


async def check_lock(dsn: str) -> None:
    for cache, label in ((1, "lock"), (20, "lock/planted-cache-20")):
        setup = await fresh(dsn, cache)
        await setup.execute(
            "CREATE TABLE items (id int PRIMARY KEY, grp int, amount int)"
        )
        await setup.execute(TRIGGERS.format(t="items"))
        await setup.execute("INSERT INTO items VALUES (300, 1, 0)")
        first = await asyncpg.connect(db(dsn))
        second = await asyncpg.connect(db(dsn))
        await second.execute("INSERT INTO items VALUES (200, 1, 0)")
        await first.execute("INSERT INTO items VALUES (201, 1, 0)")
        await drain(setup)
        tx = first.transaction()
        await tx.start()
        await first.execute("UPDATE items SET amount = 1 WHERE id = 300")
        later = asyncio.create_task(
            second.execute("UPDATE items SET amount = 2 WHERE id = 300")
        )
        await wait_for_lock(setup, second.get_server_pid())
        await tx.commit()
        await later
        came = [row for row in await drain(setup) if row["incoming"]]
        order = [numbers(row, 2)[0] for row in came]
        ids = [row["id"] for row in came]
        record(
            label,
            (order == [1, 2]) == (cache == 1),
            f"ids {ids}, amounts in outbox order {order}",
        )
        for conn in (first, second, setup):
            await conn.close()


async def check_deferred(dsn: str) -> None:
    setup = await fresh(dsn, 1)
    await setup.execute(
        "CREATE TABLE d (k int, v int, UNIQUE (k) DEFERRABLE INITIALLY DEFERRED)"
    )
    await setup.execute(TRIGGERS.format(t="d"))
    await setup.execute("INSERT INTO d VALUES (1, 10)")
    await drain(setup)
    newer = await asyncpg.connect(db(dsn))
    older = await asyncpg.connect(db(dsn))
    newer_tx = newer.transaction()
    await newer_tx.start()
    await newer.execute("INSERT INTO d VALUES (1, 20)")
    older_tx = older.transaction()
    await older_tx.start()
    await older.execute("DELETE FROM d WHERE k = 1")
    commit = asyncio.create_task(newer_tx.commit())
    await wait_for_lock(setup, newer.get_server_pid())
    await older_tx.commit()
    await commit
    events = [
        ("in" if row["incoming"] else "out", numbers(row, 0), numbers(row, 1))
        for row in await drain(setup)
    ]
    record(
        "deferred/in-before-out",
        [event[0] for event in events] == ["in", "out"],
        f"outbox order {events}; a multiset still ends right, a key match sees k=1 twice",
    )
    for conn in (newer, older, setup):
        await conn.close()


async def check_drain(dsn: str) -> None:
    setup = await fresh(dsn, 1)
    await setup.execute("CREATE TABLE items (id int PRIMARY KEY, grp int, amount int)")
    await setup.execute(TRIGGERS.format(t="items"))
    late = await asyncpg.connect(db(dsn))
    tx = late.transaction()
    await tx.start()
    await late.execute("INSERT INTO items VALUES (1, 1, 1)")
    await setup.execute("INSERT INTO items VALUES (2, 1, 2)")
    first = await drain(setup)
    await tx.commit()
    second = await drain(setup)
    seen = [row["id"] for row in first] + [row["id"] for row in second]
    record(
        "drain",
        sorted(seen) == [1, 2] and len(first) == 1,
        f"first poll {[r['id'] for r in first]}, second {[r['id'] for r in second]}",
    )

    tx = late.transaction()
    await tx.start()
    await late.execute("INSERT INTO items VALUES (3, 1, 3)")
    await setup.execute("INSERT INTO items VALUES (4, 1, 4)")
    consumer = setup.transaction()
    await consumer.start()
    read = [
        row["id"]
        for row in await setup.fetch("SELECT id FROM straum.outbox ORDER BY id")
    ]
    await tx.commit()
    deleted = [
        r["id"]
        for r in await setup.fetch(
            "DELETE FROM straum.outbox WHERE id <= $1 RETURNING id", max(read)
        )
    ]
    await consumer.commit()
    lost = sorted(set(deleted) - set(read))
    record(
        "drain/planted-range-delete",
        lost != [],
        f"read {read}, deleted {sorted(deleted)}, lost {lost}",
    )
    await late.close()
    await setup.close()


async def churn(dsn: str, seconds: float) -> dict[str, float]:
    producer = await asyncpg.connect(db(dsn))
    consumer = await asyncpg.connect(db(dsn))
    end = time.monotonic() + seconds
    polls: list[float] = []

    async def produce() -> None:
        while time.monotonic() < end:
            await producer.execute(
                "INSERT INTO straum.outbox (relid, incoming, rows) SELECT 1, true,"
                " convert_to(repeat('x', 200), 'UTF8') FROM generate_series(1, 10)"
            )

    async def consume() -> None:
        while time.monotonic() < end:
            started = time.perf_counter()
            await consumer.fetch(DRAIN.format(limit=100))
            polls.append((time.perf_counter() - started) * 1000)

    await asyncio.gather(produce(), consume())
    for _ in range(50):
        await consumer.fetch(DRAIN.format(limit=1000))
    plan = await consumer.fetchval(
        "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON)"
        " SELECT id FROM straum.outbox ORDER BY id LIMIT 100"
    )
    node = json.loads(plan)[0]["Plan"]
    await consumer.execute("SELECT pg_stat_force_next_flush()")
    await asyncio.sleep(1)
    stats = await consumer.fetchrow(
        "SELECT n_dead_tup, pg_relation_size('straum.outbox') AS heap,"
        " pg_relation_size('straum.outbox_pkey') AS idx FROM pg_stat_all_tables"
        " WHERE relid = 'straum.outbox'::regclass"
    )
    await producer.close()
    await consumer.close()
    quantiles = statistics.quantiles(polls, n=100)
    return {
        "polls": len(polls),
        "p50_ms": round(quantiles[49], 3),
        "p99_ms": round(quantiles[98], 3),
        "empty_poll_buffers": node.get("Shared Hit Blocks", 0)
        + node.get("Shared Read Blocks", 0),
        "dead_tuples": stats["n_dead_tup"],
        "heap_kb": stats["heap"] // 1024,
        "index_kb": stats["idx"] // 1024,
    }


async def check_horizon(dsn: str, seconds: float) -> None:
    setup = await fresh(dsn, 1)
    rows = []
    rows.append({"phase": "free", **await churn(dsn, seconds)})
    holder = await asyncpg.connect(db(dsn))
    tx = holder.transaction(isolation="repeatable_read")
    await tx.start()
    await holder.fetchval("SELECT pg_current_xact_id()")
    rows.append({"phase": "snapshot held", **await churn(dsn, seconds)})
    await tx.rollback()
    await holder.close()
    rows.append({"phase": "released", **await churn(dsn, seconds)})
    await setup.execute("VACUUM straum.outbox")
    rows.append({"phase": "after VACUUM", **await churn(dsn, 2)})
    print(tabulate.tabulate(rows, headers="keys"), flush=True)
    held = rows[1]["empty_poll_buffers"]
    free = rows[0]["empty_poll_buffers"]
    record(
        "horizon",
        held > free,
        f"empty poll buffers free {free}, held {held},"
        f" released {rows[2]['empty_poll_buffers']},"
        f" after VACUUM {rows[3]['empty_poll_buffers']}",
    )
    await setup.close()


DATA = pathlib.Path(__file__).resolve().parent / "data"
NEW_YORK = zoneinfo.ZoneInfo("America/New_York")
# February's trips the app writes; enough for a run of a minute or two.
FEBRUARY_TRIPS = 300_000
WRITERS = 8
TABLES = ["yellow_trips", "taxi_zones"]
INSERT_TRIPS = (
    f"INSERT INTO yellow_trips ({', '.join(COLUMNS.values())})"
    " SELECT * FROM unnest($1::int[], $2::timestamptz[], $3::int[], $4::int[],"
    " $5::int[], $6::numeric[], $7::numeric[], $8::numeric[])"
)
BULK = [
    (
        "week's fares repriced",
        (
            "UPDATE yellow_trips SET fare_amount = round(fare_amount * 1.05, 2)"
            " WHERE pickup_at >= '2024-01-08' AND pickup_at < '2024-01-15'"
        ),
    ),
    (
        "day's payment fixed",
        (
            "UPDATE yellow_trips SET payment_type = 1"
            " WHERE pickup_at >= '2024-01-20' AND pickup_at < '2024-01-21'"
            " AND payment_type = 2"
        ),
    ),
    ("negative fares deleted", "DELETE FROM yellow_trips WHERE fare_amount < 0"),
    (
        "zone renamed",
        "UPDATE taxi_zones SET borough = 'Manhattan!' WHERE location_id = 161",
    ),
]


def load_january(stack: Stack) -> None:
    """January's trips into the database `taxi`, the template each run
    copies."""
    stack_psql = stack.exec_args(
        POSTGRES, "psql", "-U", "postgres", "-q", "-c", "CREATE DATABASE taxi"
    )
    subprocess.run(stack_psql, check=True)
    loader = subprocess.Popen(
        [
            pathlib.Path(__file__).resolve().parent / "load_taxi_trips.py",
            "--months",
            "1",
            "--data",
            DATA,
        ],
        stdout=subprocess.PIPE,
    )
    psql = stack.exec_args(POSTGRES, "psql", "-U", "postgres", "-q", "-d", "taxi")
    subprocess.run(psql, stdin=loader.stdout, check=True, stdout=subprocess.DEVNULL)
    loader.stdout.close()
    if loader.wait():
        raise SystemExit("load_taxi_trips.py failed")


def money(value: float | None) -> decimal.Decimal | None:
    return None if value is None else decimal.Decimal(str(value))


def february() -> list[tuple]:
    """The first FEBRUARY_TRIPS trips of February 2024, as insert values."""
    path = download(DATA, f"{TLC}/trip-data/yellow_tripdata_2024-02.parquet")
    trips = []
    for batch in pyarrow.parquet.ParquetFile(path).iter_batches(columns=list(COLUMNS)):
        for row in batch.to_pylist():
            pickup = row["tpep_pickup_datetime"]
            trips.append(
                (
                    row["VendorID"],
                    None if pickup is None else pickup.replace(tzinfo=NEW_YORK),
                    row["PULocationID"],
                    row["DOLocationID"],
                    row["payment_type"],
                    money(row["fare_amount"]),
                    money(row["tip_amount"]),
                    money(row["trip_distance"]),
                )
            )
            if len(trips) == FEBRUARY_TRIPS:
                return trips
    return trips


def batches(trips: list[tuple], rng: random.Random) -> collections.deque:
    """Trips cut into statements of 1 to 20, as an app flushing meters."""
    cut = collections.deque()
    at = 0
    while at < len(trips):
        size = rng.randint(1, 20)
        cut.append(trips[at : at + size])
        at += size
    return cut


async def tallies(conn: asyncpg.Connection) -> dict[int, list[int]]:
    """Each table's rows, count and hash sums, by the table's OID."""
    found = {}
    for table in TABLES:
        relid = await conn.fetchval("SELECT $1::regclass::oid", table)
        row = await conn.fetchrow(TALLY.format(table=table), timeout=600)
        found[relid] = [row["rows"], int(row["high"]), int(row["low"])]
    return found


def apply(totals: dict[int, list[int]], got: list[asyncpg.Record]) -> None:
    """Takes each drained row out of, or into, its table's tally."""
    for row in got:
        tally = totals[row["relid"]]
        sign = 1 if row["incoming"] else -1
        for sent in records(row["rows"]):
            digest = hashlib.md5(sent).digest()
            tally[0] += sign
            tally[1] += sign * int.from_bytes(digest[:8], "big", signed=True)
            tally[2] += sign * int.from_bytes(digest[8:], "big", signed=True)


async def consume(dsn: str, totals: dict, stop: asyncio.Event, state: dict) -> None:
    conn = await asyncpg.connect(dsn)
    limit = 1000
    while True:
        got = await drain(conn, limit)
        apply(totals, got)
        state["drained"] += len(got)
        limit = min(limit * 2, 20000) if len(got) == limit else max(limit // 2, 1000)
        if not got:
            if stop.is_set():
                break
            await asyncio.sleep(0.01)
    await conn.close()


async def watch_backlog(dsn: str, state: dict, stop: asyncio.Event) -> None:
    conn = await asyncpg.connect(dsn)
    while not stop.is_set():
        written = await conn.fetchval(
            "SELECT pg_sequence_last_value('straum.outbox_id_seq')"
        )
        state["backlog_max"] = max(
            state["backlog_max"], (written or 0) - state["drained"]
        )
        await asyncio.sleep(0.5)
    await conn.close()


async def write(
    dsn: str,
    queue: collections.deque,
    top_id: int,
    rng: random.Random,
    latencies: list[float],
) -> int:
    """One app connection: insert batches until none are left; after some,
    correct a tip or change a vendor, each its own statement."""
    conn = await asyncpg.connect(dsn)
    statements = 0
    tip = "UPDATE yellow_trips SET tip_amount = tip_amount + 1 WHERE id = $1"
    vendor = "UPDATE yellow_trips SET vendor_id = 2 WHERE id = $1"
    while queue:
        batch = queue.popleft()
        work = [(INSERT_TRIPS, [list(column) for column in zip(*batch, strict=True)])]
        if rng.random() < 0.3:
            work.append((tip, [rng.randint(1, top_id)]))
        if rng.random() < 0.2:
            work.append((vendor, [rng.randint(1, top_id)]))
        for sql, params in work:
            started = time.perf_counter()
            await conn.execute(sql, *params)
            latencies.append((time.perf_counter() - started) * 1000)
            statements += 1
    await conn.close()
    return statements


async def lsn(conn: asyncpg.Connection) -> str:
    """Where the WAL is now, just after a checkpoint, so each phase of
    both runs pays the same page images for the pages it first touches."""
    await conn.execute("CHECKPOINT")
    return await conn.fetchval("SELECT pg_current_wal_insert_lsn()::text")


async def relations(conn: asyncpg.Connection, tables: list[str]) -> list[int]:
    """The file nodes of `tables` with their indexes, TOAST tables and
    TOAST indexes, which is where their WAL lands."""
    rows = await conn.fetch(
        "SELECT pg_relation_filenode(c.oid) AS node FROM pg_class c"
        " WHERE c.oid = ANY($1::regclass[])"
        " OR c.oid IN (SELECT indexrelid FROM pg_index WHERE indrelid = ANY($1::regclass[]))"
        " OR c.oid IN (SELECT reltoastrelid FROM pg_class WHERE oid = ANY($1::regclass[]))"
        " OR c.oid IN (SELECT indexrelid FROM pg_index WHERE indrelid IN"
        "  (SELECT reltoastrelid FROM pg_class WHERE oid = ANY($1::regclass[])))",
        tables,
    )
    return [row["node"] for row in rows if row["node"]]


async def wal_since(conn: asyncpg.Connection, start: str, triggers: bool) -> dict:
    """WAL written since `start`, in MB, split into the app's tables, the
    outbox and the rest (commits, catalogs, other tables). A record that
    touches several blocks is shared among them evenly."""
    end = await conn.fetchval("SELECT pg_current_wal_insert_lsn()::text")
    total = await conn.fetchval(
        "SELECT pg_wal_lsn_diff($1::text::pg_lsn, $2::text::pg_lsn)", end, start
    )
    if not total:
        return {"app tables MB": 0.0, "outbox MB written": 0.0, "rest MB": 0.0}
    app = await relations(conn, TABLES)
    outbox = await relations(conn, ["straum.outbox"]) if triggers else []
    rows = await conn.fetch(
        "SELECT relfilenode, sum(record_length::numeric / n) AS bytes FROM"
        " (SELECT relfilenode, record_length, count(*) OVER (PARTITION BY start_lsn) AS n"
        "  FROM pg_get_wal_block_info($1::text::pg_lsn, $2::text::pg_lsn)) b"
        " GROUP BY relfilenode",
        start,
        end,
    )
    split = {"app": 0.0, "outbox": 0.0}
    for row in rows:
        if row["relfilenode"] in app:
            split["app"] += float(row["bytes"])
        elif row["relfilenode"] in outbox:
            split["outbox"] += float(row["bytes"])
    rest = float(total) - split["app"] - split["outbox"]
    return {
        "app tables MB": round(split["app"] / 2**20, 1),
        "outbox MB written": round(split["outbox"] / 2**20, 1),
        "rest MB": round(rest / 2**20, 1),
    }


async def outbox_size(conn: asyncpg.Connection) -> float:
    size = await conn.fetchval("SELECT pg_total_relation_size('straum.outbox')")
    return round(size / 2**20, 1)


async def copy_of_taxi(base: str, name: str, triggers: bool) -> asyncpg.Connection:
    """A copy of the loaded trips, with or without the outbox."""
    admin = await asyncpg.connect(base + "/postgres")
    await admin.execute(f"DROP DATABASE IF EXISTS {name} WITH (FORCE)")
    await admin.execute(f"CREATE DATABASE {name} TEMPLATE taxi STRATEGY file_copy")
    await admin.close()
    conn = await asyncpg.connect(f"{base}/{name}")
    await conn.execute("VACUUM ANALYZE")
    await conn.execute("CREATE EXTENSION pg_walinspect")
    if triggers:
        await conn.execute(OUTBOX.format(cache=1, cap=ROW_CAP))
        await conn.execute(FRESH)
        for table in TABLES:
            await conn.execute(TRIGGERS.format(t=table))
    return conn


async def run_taxi(base: str, trips: list[tuple], triggers: bool) -> list[dict]:
    name = "with_outbox" if triggers else "plain"
    conn = await copy_of_taxi(base, name, triggers)
    dsn = f"{base}/{name}"
    totals = await tallies(conn) if triggers else {}
    top_id = await conn.fetchval("SELECT max(id) FROM yellow_trips")
    state = {"drained": 0, "backlog_max": 0}
    phases = []

    async def phase(
        label: str, took: float, latencies: list[float], start: str
    ) -> dict:
        ordered = sorted(latencies)
        last = len(ordered) - 1
        row = {
            "outbox": "yes" if triggers else "no",
            "phase": label,
            "seconds": round(took, 2),
            "statements": len(ordered),
            "p50 ms": round(ordered[last // 2], 2),
            "p99 ms": round(ordered[last * 99 // 100], 2),
            **await wal_since(conn, start, triggers),
        }
        phases.append(row)
        return row

    stop = asyncio.Event()
    background = []
    if triggers:
        background.append(asyncio.create_task(consume(dsn, totals, stop, state)))
        background.append(asyncio.create_task(watch_backlog(dsn, state, stop)))
    queue = batches(trips, random.Random(7))
    latencies: list[float] = []
    start = await lsn(conn)
    started = time.perf_counter()
    await asyncio.gather(
        *[
            write(dsn, queue, top_id, random.Random(seed), latencies)
            for seed in range(WRITERS)
        ]
    )
    row = await phase(
        "February written", time.perf_counter() - started, latencies, start
    )
    stop.set()
    await asyncio.gather(*background)
    if triggers:
        row |= {"backlog max": state["backlog_max"], "reload": "no"}
        row["outbox size MB"] = await outbox_size(conn)
        record(
            "taxi/february",
            totals == await tallies(conn) and await conn.fetchval(SKIPPED) == 0,
            f"{state['drained']} outbox rows applied; rows and hashes {totals}",
        )
    print(row, flush=True)
    for label, sql in BULK:
        skipped = await conn.fetchval(SKIPPED) if triggers else 0
        start = await lsn(conn)
        started = time.perf_counter()
        await conn.execute(sql, timeout=1800)
        took = time.perf_counter() - started
        row = await phase(label, took, [took * 1000], start)
        if triggers:
            reload = await conn.fetchval(SKIPPED) != skipped
            row["outbox size MB"] = await outbox_size(conn)
            if reload:
                # As a start does: what the load holds goes in its snapshot.
                async with conn.transaction(isolation="repeatable_read"):
                    await conn.execute("DELETE FROM straum.outbox")
                    totals = await tallies(conn)
            else:
                while got := await drain(conn, 1000):
                    apply(totals, got)
            row |= {"backlog max": "", "reload": "yes" if reload else "no"}
        print(row, flush=True)
    if triggers:
        record(
            "taxi/bulk",
            totals == await tallies(conn),
            f"rows and hashes after the bulk statements {totals}",
        )
    await conn.close()
    return phases


async def run(stack: Stack, password: str) -> int:
    port = stack.port(POSTGRES, POSTGRES_PORT)
    base = f"postgresql://postgres:{password}@127.0.0.1:{port}"
    dsn = f"{base}/postgres"
    version = await (await asyncpg.connect(dsn)).fetchval("SHOW server_version")
    print(f"PostgreSQL {version}", flush=True)
    await check_capture(dsn)
    await check_lock(dsn)
    await check_deferred(dsn)
    await check_drain(dsn)
    await check_order(dsn, 3)
    await check_horizon(dsn, 10)
    print("loading January's trips", flush=True)
    load_january(stack)
    trips = february()
    plain = await run_taxi(base, trips, triggers=False)
    outbox = await run_taxi(base, trips, triggers=True)
    print(tabulate.tabulate(plain + outbox, headers="keys"))
    print(
        tabulate.tabulate(
            RESULTS,
            headers=["check", "result", "detail"],
            maxcolwidths=[None, None, 90],
        )
    )
    return 1 if any(result == "FAIL" for _, result, _ in RESULTS) else 0


def main() -> int:
    password = secrets.token_hex(16)
    # Sized as a small server would be, the rest as a server runs by
    # default. wal_keep_size keeps each phase's WAL until it is read back.
    settings = {"shared_buffers": "2GB", "wal_keep_size": "8GB"}
    services = {
        POSTGRES: postgres_service(POSTGRES_IMAGE, password, settings, shm_size="1g")
    }
    with Stack("straum-outbox", services) as stack:
        stack.up(POSTGRES)
        return asyncio.run(run(stack, password))


if __name__ == "__main__":
    sys.exit(main())
