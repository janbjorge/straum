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

capture: what a statement writes, skips, and survives (renames, drops,
retypes, TRUNCATE, NaN, json columns).
lock: the second writer of a row gets the later id. Planted: CACHE 20.
deferred: a deferred unique key lets a new key in before the old one out.
drain: a low id that commits late is not lost. Planted: a range delete.
order: concurrent writers on hot rows, drained while they run, applied as
a multiset in outbox order; an out row never applies before its in row,
and the result equals the table. Planted: the id sequence with CACHE 20.
horizon: poll cost while a snapshot is held open, and after.

Then the taxi workload. January's trips are loaded; the app then writes
February's real trips in small batches from 8 workers, with tip
corrections and vendor changes (a column the spec never reads) mixed in,
then a week's fare repricing, a day's payment fix, a delete of negative
fares and a zone's borough renamed. It runs once without the triggers
and once with them and a consumer that applies the outbox to count and
fare per hour and payment type, which must equal PostgreSQL's at the
end. Each phase reports its time, statement latency, the write-ahead
log (WAL) written to the app's tables and to the outbox, and the
outbox's backlog and size.

Not here yet, though ADR-0018 decides them: the sign, its count of skipped
writes, the start, the row cap and whole transactions. Rows go as JSON.

Needs Docker and uv. Downloads go to bench/data, which git ignores.
"""

import asyncio
import collections
import datetime
import decimal
import json
import pathlib
import random
import secrets
import statistics
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
CREATE TABLE straum.outbox (
  id       bigint GENERATED ALWAYS AS IDENTITY (CACHE {cache} NO CYCLE) PRIMARY KEY,
  relid    oid      NOT NULL,
  kind     smallint NOT NULL,
  types    text[],
  old_rows json,
  new_rows json
) WITH (
  fillfactor = 100,
  vacuum_truncate = false,
  autovacuum_vacuum_scale_factor = 0,
  autovacuum_vacuum_threshold = 1000,
  autovacuum_vacuum_cost_delay = 0,
  toast.autovacuum_vacuum_scale_factor = 0,
  toast.autovacuum_vacuum_threshold = 1000,
  toast.autovacuum_vacuum_cost_delay = 0
);
ALTER TABLE straum.outbox ALTER old_rows SET COMPRESSION lz4,
                          ALTER new_rows SET COMPRESSION lz4;

CREATE FUNCTION straum.capture() RETURNS trigger LANGUAGE plpgsql AS $f$
DECLARE
  cols text;
  kinds text[];
  diff text;
BEGIN
  IF TG_OP = 'TRUNCATE' THEN
    INSERT INTO straum.outbox (relid, kind) VALUES (TG_RELID, 2);
    RETURN NULL;
  END IF;
  SELECT string_agg(quote_ident(attname), ', ' ORDER BY attnum),
         array_agg(format_type(atttypid, atttypmod) ORDER BY attnum)
    INTO cols, kinds
    FROM pg_attribute
   WHERE attrelid = TG_RELID
     AND attnum = ANY (TG_ARGV[0]::int2[])
     AND NOT attisdropped;
  IF TG_OP = 'INSERT' THEN
    diff := format('SELECT 1 AS side, json_build_array(%s)::text AS r FROM new_rows', cols);
  ELSIF TG_OP = 'DELETE' THEN
    diff := format('SELECT 0 AS side, json_build_array(%s)::text AS r FROM old_rows', cols);
  ELSE
    diff := format(
      'SELECT 0 AS side, r FROM (SELECT json_build_array(%1$s)::text r FROM old_rows'
      ' EXCEPT ALL SELECT json_build_array(%1$s)::text FROM new_rows) a'
      ' UNION ALL '
      'SELECT 1 AS side, r FROM (SELECT json_build_array(%1$s)::text r FROM new_rows'
      ' EXCEPT ALL SELECT json_build_array(%1$s)::text FROM old_rows) b', cols);
  END IF;
  EXECUTE format(
    'INSERT INTO straum.outbox (relid, kind, types, old_rows, new_rows)'
    ' SELECT $1, 1, $2,'
    '  CASE WHEN side = 0 THEN json_agg(r::json) END,'
    '  CASE WHEN side = 1 THEN json_agg(r::json) END'
    ' FROM (SELECT side, r, (row_number() OVER (PARTITION BY side) - 1) / %s AS chunk'
    '       FROM (%s) d) c'
    ' GROUP BY side, chunk ORDER BY side, chunk', {chunk}, diff)
    USING TG_RELID, kinds;
  RETURN NULL;
END
$f$;
"""

TRIGGERS = """
CREATE TRIGGER {t}_ins AFTER INSERT ON {t} REFERENCING NEW TABLE AS new_rows
  FOR EACH STATEMENT EXECUTE FUNCTION straum.capture('{cols}');
CREATE TRIGGER {t}_upd AFTER UPDATE ON {t}
  REFERENCING OLD TABLE AS old_rows NEW TABLE AS new_rows
  FOR EACH STATEMENT EXECUTE FUNCTION straum.capture('{cols}');
CREATE TRIGGER {t}_del AFTER DELETE ON {t} REFERENCING OLD TABLE AS old_rows
  FOR EACH STATEMENT EXECUTE FUNCTION straum.capture('{cols}');
CREATE TRIGGER {t}_trunc AFTER TRUNCATE ON {t}
  FOR EACH STATEMENT EXECUTE FUNCTION straum.capture('{cols}');
"""

DRAIN = """
DELETE FROM straum.outbox
 WHERE id IN (SELECT id FROM straum.outbox ORDER BY id LIMIT {limit})
RETURNING id, relid, kind, types, old_rows::text, new_rows::text
"""

HOT_KEYS = 20
# Rows per outbox row, so no value nears PostgreSQL's 1 GB limit.
CHUNK_ROWS = 10000
RESULTS: list[tuple[str, str, str]] = []


def record(check: str, passed: bool, detail: str) -> None:
    RESULTS.append((check, "PASS" if passed else "FAIL", detail))
    print(f"{check}: {'PASS' if passed else 'FAIL'} {detail}", flush=True)


def rows_of(text: str | None) -> list[tuple]:
    if text is None:
        return []
    parsed = json.loads(text, parse_float=decimal.Decimal, parse_int=decimal.Decimal)
    return [tuple(value for value in row) for row in parsed]


async def fresh(dsn: str, cache: int) -> asyncpg.Connection:
    """A database with only the outbox and its function, at `cache`."""
    admin = await asyncpg.connect(dsn)
    await admin.execute("DROP DATABASE IF EXISTS t WITH (FORCE)")
    await admin.execute("CREATE DATABASE t")
    await admin.close()
    conn = await asyncpg.connect(dsn.rsplit("/", 1)[0] + "/t")
    await conn.execute(OUTBOX.format(cache=cache, chunk=CHUNK_ROWS))
    return conn


def db(dsn: str) -> str:
    return dsn.rsplit("/", 1)[0] + "/t"


async def drain(conn: asyncpg.Connection, limit: int = 200) -> list[asyncpg.Record]:
    got = await conn.fetch(DRAIN.format(limit=limit))
    return sorted(got, key=lambda record: record["id"])


async def check_capture(dsn: str) -> None:
    conn = await fresh(dsn, 1)
    await conn.execute(
        "CREATE TABLE items (id int PRIMARY KEY, grp int, amount numeric,"
        " note text, extra text)"
    )
    await conn.execute(TRIGGERS.format(t="items", cols="{1,2,3}"))

    async def written(sql: str) -> list[asyncpg.Record]:
        await conn.execute(sql)
        return await drain(conn)

    got = await written(
        "INSERT INTO items VALUES (1,1,10,'a','x'),(2,1,20,'b','x'),(3,2,'NaN','c','x')"
    )
    new = rows_of(got[0]["new_rows"]) if got else []
    types = got[0]["types"] if got else None
    nan = new[2][2] if len(new) == 3 else None
    record(
        "capture/insert",
        len(got) == 1 and len(new) == 3 and types == ["integer", "integer", "numeric"],
        f"outbox rows {len(got)}, rows in {len(new)}, types {types}, NaN as {nan!r}",
    )
    got = await written("UPDATE items SET note = note || '!'")
    record("capture/other-column-skips", got == [], f"outbox rows {len(got)}")
    got = await written("DELETE FROM items WHERE false")
    record("capture/zero-rows-skips", got == [], f"outbox rows {len(got)}")
    got = await written("UPDATE items SET amount = amount WHERE id = 1")
    record("capture/same-value-skips", got == [], f"outbox rows {len(got)}")
    got = await written("UPDATE items SET amount = amount + 1 WHERE id IN (1, 2)")
    out = rows_of(got[0]["old_rows"]) if got else []
    into = rows_of(got[-1]["new_rows"]) if got else []
    record(
        "capture/update-out-then-in",
        len(got) == 2
        and got[0]["new_rows"] is None
        and len(out) == 2
        and len(into) == 2,
        f"outbox rows {len(got)}, out {out}, in {into}",
    )
    await conn.execute("ALTER TABLE items RENAME amount TO amt")
    got = await written("UPDATE items SET amt = amt + 1 WHERE id = 1")
    record("capture/spec-column-renamed", len(got) == 2, f"outbox rows {len(got)}")
    await conn.execute("ALTER TABLE items DROP COLUMN extra")
    await conn.execute("ALTER TABLE items ALTER note TYPE varchar(20)")
    got = await written("UPDATE items SET amt = amt + 1 WHERE id = 1")
    record(
        "capture/other-column-dropped-retyped",
        len(got) == 2,
        f"outbox rows {len(got)}",
    )
    await conn.execute("ALTER TABLE items ALTER amt TYPE numeric(12,2)")
    got = await written("UPDATE items SET amt = amt + 1 WHERE id = 1")
    record(
        "capture/spec-column-retyped",
        len(got) == 2 and got[0]["types"][2] == "numeric(12,2)",
        f"types {got[0]['types'] if got else None}",
    )
    got = await written(
        "INSERT INTO items (id, grp, amt) VALUES (1, 9, 9) ON CONFLICT (id)"
        " DO UPDATE SET amt = items.amt + 1"
    )
    record("capture/on-conflict-update", len(got) == 2, f"outbox rows {len(got)}")
    got = await written("TRUNCATE items")
    record(
        "capture/truncate-resets",
        len(got) == 1 and got[0]["kind"] == 2,
        f"kinds {[row['kind'] for row in got]}",
    )
    await conn.execute("CREATE TABLE j (v json)")
    await conn.execute(TRIGGERS.format(t="j", cols="{1}"))
    try:
        await conn.execute("INSERT INTO j VALUES ('{}')")
        await conn.execute("UPDATE j SET v = '[]'")
        record("capture/json-column", True, "update on a json column works")
    except asyncpg.PostgresError as error:
        record("capture/json-column", False, f"update on a json column: {error}")
    await conn.close()


async def worker(dsn: str, rng: random.Random, statements: int) -> int:
    conn = await asyncpg.connect(dsn)
    failed = 0
    for _ in range(statements):
        key = rng.randint(1, HOT_KEYS)
        choice = rng.random()
        if choice < 0.25:
            sql = (
                f"INSERT INTO items VALUES ({key}, {rng.randint(1, 3)}, {rng.randint(1, 9)},"
                f" 'n') ON CONFLICT (id) DO UPDATE SET amount = items.amount + 1"
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
                    await conn.execute(
                        f"UPDATE items SET amount = amount + 1 WHERE id = {rng.randint(1, HOT_KEYS)}"
                    )
                await asyncio.sleep(rng.random() * 0.002)
        except (asyncpg.DeadlockDetectedError, asyncpg.UniqueViolationError):
            failed += 1
    await conn.close()
    return failed


async def order_run(dsn: str, cache: int, seed: int) -> tuple[int, bool, int]:
    """(out rows that found nothing to take, table equals the multiset,
    statements applied)."""
    setup = await fresh(dsn, cache)
    await setup.execute(
        "CREATE TABLE items (id int PRIMARY KEY, grp int, amount numeric, note text)"
    )
    await setup.execute(TRIGGERS.format(t="items", cols="{1,2,3}"))
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
            for gone in rows_of(row["old_rows"]):
                if held[gone] == 0:
                    missing += 1
                else:
                    held[gone] -= 1
            for came in rows_of(row["new_rows"]):
                held[came] += 1
        if done and not got:
            break
        if not got:
            await asyncio.sleep(0.005)
    await asyncio.gather(*tasks)
    table = collections.Counter(
        (decimal.Decimal(r["id"]), decimal.Decimal(r["grp"]), r["amount"])
        for r in await consumer.fetch("SELECT id, grp, amount FROM items")
    )
    await consumer.close()
    await setup.close()
    return missing, +held == table, applied


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
            "CREATE TABLE items (id int PRIMARY KEY, grp int, amount numeric)"
        )
        await setup.execute(TRIGGERS.format(t="items", cols="{1,2,3}"))
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
        got = await drain(setup)
        order = [rows_of(row["new_rows"])[0][2] for row in got if row["new_rows"]]
        ids = [row["id"] for row in got if row["new_rows"]]
        right = order == [1, 2]
        record(
            label, right == (cache == 1), f"ids {ids}, amounts in outbox order {order}"
        )
        for conn in (first, second, setup):
            await conn.close()


async def check_deferred(dsn: str) -> None:
    setup = await fresh(dsn, 1)
    await setup.execute(
        "CREATE TABLE d (k int, v int, UNIQUE (k) DEFERRABLE INITIALLY DEFERRED)"
    )
    await setup.execute(TRIGGERS.format(t="d", cols="{1,2}"))
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
    got = await drain(setup)
    events = [
        ("in", rows_of(r["new_rows"]))
        if r["new_rows"]
        else ("out", rows_of(r["old_rows"]))
        for r in got
    ]
    inverted = [event[0] for event in events] == ["in", "out"]
    record(
        "deferred/in-before-out",
        inverted,
        f"outbox order {events}; a multiset still ends right, a key match sees k=1 twice",
    )
    for conn in (newer, older, setup):
        await conn.close()


async def check_drain(dsn: str) -> None:
    setup = await fresh(dsn, 1)
    await setup.execute(
        "CREATE TABLE items (id int PRIMARY KEY, grp int, amount numeric)"
    )
    await setup.execute(TRIGGERS.format(t="items", cols="{1,2,3}"))
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
                "INSERT INTO straum.outbox (relid, kind, new_rows)"
                " SELECT 1, 1, json_build_array(g, repeat('x', 200)) FROM generate_series(1, 10) g"
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
        "EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON) SELECT id FROM straum.outbox ORDER BY id LIMIT 100"
    )
    node = json.loads(plan)[0]["Plan"]
    await consumer.execute("SELECT pg_stat_force_next_flush()")
    await asyncio.sleep(1)
    stats = await consumer.fetchrow(
        "SELECT n_dead_tup, n_live_tup, pg_relation_size('straum.outbox') AS heap,"
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
    await setup.execute("VACUUM (SKIP_LOCKED, TRUNCATE false) straum.outbox")
    rows.append({"phase": "after VACUUM", **await churn(dsn, 2)})
    print(tabulate.tabulate(rows, headers="keys"), flush=True)
    held = rows[1]["empty_poll_buffers"]
    free = rows[0]["empty_poll_buffers"]
    record(
        "horizon",
        held > free,
        f"empty poll buffers free {free}, held {held}, released {rows[2]['empty_poll_buffers']},"
        f" after VACUUM {rows[3]['empty_poll_buffers']}",
    )
    await setup.close()


DATA = pathlib.Path(__file__).resolve().parent / "data"
NEW_YORK = zoneinfo.ZoneInfo("America/New_York")
# February's trips the app writes; enough for a run of a minute or two.
FEBRUARY_TRIPS = 300_000
WRITERS = 8
# The yellow_trips columns the taxi specs read; vendor_id is not one.
READ_TRIP_COLUMNS = [
    "pickup_at",
    "pu_location_id",
    "do_location_id",
    "payment_type",
    "fare_amount",
    "tip_amount",
    "trip_distance",
]
READ_ZONE_COLUMNS = ["location_id", "borough"]
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


def hour_of(text: str | None) -> datetime.datetime | None:
    if text is None:
        return None
    stamp = datetime.datetime.fromisoformat(text).astimezone(datetime.UTC)
    return stamp.replace(minute=0, second=0, microsecond=0, tzinfo=None)


async def reference_from_sql(conn: asyncpg.Connection) -> dict:
    totals = {}
    for row in await conn.fetch(
        "SELECT date_trunc('hour', pickup_at AT TIME ZONE 'UTC') AS hour, payment_type,"
        " count(*) AS trips, coalesce(sum(fare_amount), 0) AS fare"
        " FROM yellow_trips GROUP BY 1, 2"
    ):
        totals[(row["hour"], row["payment_type"])] = [row["trips"], row["fare"]]
    return totals


def apply_trip(totals: dict, row: tuple, sign: int) -> None:
    pickup, _, _, payment, fare = row[0], row[1], row[2], row[3], row[4]
    key = (hour_of(pickup), None if payment is None else int(payment))
    entry = totals.setdefault(key, [0, decimal.Decimal(0)])
    entry[0] += sign
    entry[1] += sign * (fare or 0)
    if entry[0] == 0:
        del totals[key]


async def consume(
    dsn: str, trips_relid: int, totals: dict, stop: asyncio.Event, state: dict
) -> None:
    conn = await asyncpg.connect(dsn)
    limit = 1000
    while True:
        got = await drain(conn, limit)
        for row in got:
            if row["relid"] != trips_relid:
                continue
            for gone in rows_of(row["old_rows"]):
                apply_trip(totals, gone, -1)
            for came in rows_of(row["new_rows"]):
                apply_trip(totals, came, 1)
        state["drained"] += len(got)
        limit = min(limit * 2, 20000) if len(got) == limit else max(limit // 2, 1000)
        if not got:
            if stop.is_set():
                break
            await asyncio.sleep(0.01)
    await conn.close()


async def watch_backlog(
    conn: asyncpg.Connection, state: dict, stop: asyncio.Event
) -> None:
    while not stop.is_set():
        written = await conn.fetchval(
            "SELECT pg_sequence_last_value('straum.outbox_id_seq')"
        )
        state["backlog_max"] = max(
            state["backlog_max"], (written or 0) - state["drained"]
        )
        await asyncio.sleep(0.5)


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
    while queue:
        batch = queue.popleft()
        columns = [list(column) for column in zip(*batch)]
        work = [(INSERT_TRIPS, columns)]
        if rng.random() < 0.3:
            work.append(
                (
                    "UPDATE yellow_trips SET tip_amount = tip_amount + 1 WHERE id = $1",
                    [rng.randint(1, top_id)],
                )
            )
        if rng.random() < 0.2:
            work.append(
                (
                    "UPDATE yellow_trips SET vendor_id = 2 WHERE id = $1",
                    [rng.randint(1, top_id)],
                )
            )
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
    return [
        r["node"]
        for r in await conn.fetch(
            "SELECT pg_relation_filenode(c.oid) AS node FROM pg_class c"
            " WHERE c.oid = ANY($1::regclass[])"
            " OR c.oid IN (SELECT indexrelid FROM pg_index WHERE indrelid = ANY($1::regclass[]))"
            " OR c.oid IN (SELECT reltoastrelid FROM pg_class WHERE oid = ANY($1::regclass[]))"
            " OR c.oid IN (SELECT indexrelid FROM pg_index WHERE indrelid IN"
            "  (SELECT reltoastrelid FROM pg_class WHERE oid = ANY($1::regclass[])))",
            tables,
        )
        if r["node"]
    ]


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
    app = await relations(conn, ["yellow_trips", "taxi_zones"])
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


async def run_taxi(
    base: str, trips: list[tuple], triggers: bool
) -> tuple[list[dict], bool | None]:
    name = "with_outbox" if triggers else "plain"
    admin = await asyncpg.connect(base + "/postgres")
    await admin.execute(f"DROP DATABASE IF EXISTS {name} WITH (FORCE)")
    await admin.execute(f"CREATE DATABASE {name} TEMPLATE taxi STRATEGY file_copy")
    await admin.close()
    dsn = f"{base}/{name}"
    conn = await asyncpg.connect(dsn)
    await conn.execute("VACUUM ANALYZE")
    await conn.execute("CREATE EXTENSION pg_walinspect")
    if triggers:
        await conn.execute(OUTBOX.format(cache=1, chunk=CHUNK_ROWS))
        for table, columns in (
            ("yellow_trips", READ_TRIP_COLUMNS),
            ("taxi_zones", READ_ZONE_COLUMNS),
        ):
            attnums = await conn.fetchval(
                "SELECT array_agg(attnum ORDER BY attnum)::text FROM pg_attribute"
                " WHERE attrelid = $1::regclass AND attname = ANY($2)",
                table,
                columns,
            )
            await conn.execute(TRIGGERS.format(t=table, cols=attnums))
    totals = await reference_from_sql(conn)
    trips_relid = await conn.fetchval("SELECT 'yellow_trips'::regclass::oid")
    top_id = await conn.fetchval("SELECT max(id) FROM yellow_trips")
    state = {"drained": 0, "backlog_max": 0}
    stop = asyncio.Event()
    background = []
    if triggers:
        background.append(
            asyncio.create_task(consume(dsn, trips_relid, totals, stop, state))
        )
        background.append(
            asyncio.create_task(watch_backlog(await asyncpg.connect(dsn), state, stop))
        )
    phases = []

    def phase(
        label: str,
        seconds: float,
        statements: int,
        latencies: list[float],
        wal: dict,
        size: float,
    ) -> None:
        ordered = sorted(latencies)
        last = len(ordered) - 1
        phases.append(
            {
                "outbox": "yes" if triggers else "no",
                "phase": label,
                "seconds": round(seconds, 2),
                "statements": statements,
                "p50 ms": round(ordered[last // 2], 2),
                "p99 ms": round(ordered[last * 99 // 100], 2),
                **wal,
                "backlog max": state["backlog_max"] if triggers else "",
                "outbox size MB": size if triggers else "",
            }
        )
        print(phases[-1], flush=True)

    queue = batches(trips, random.Random(7))
    latencies: list[float] = []
    start = await lsn(conn)
    started = time.perf_counter()
    counts = await asyncio.gather(
        *[
            write(dsn, queue, top_id, random.Random(seed), latencies)
            for seed in range(WRITERS)
        ]
    )
    phase(
        "February written",
        time.perf_counter() - started,
        sum(counts),
        latencies,
        await wal_since(conn, start, triggers),
        await outbox_size(conn) if triggers else 0,
    )
    for label, sql in BULK:
        state["backlog_max"] = 0
        start = await lsn(conn)
        started = time.perf_counter()
        await conn.execute(sql, timeout=1800)
        took = time.perf_counter() - started
        phase(
            label,
            took,
            1,
            [took * 1000],
            await wal_since(conn, start, triggers),
            await outbox_size(conn) if triggers else 0,
        )
    equal = None
    if triggers:
        start = await lsn(conn)
        started = time.perf_counter()
        stop.set()
        await asyncio.gather(*background)
        phase(
            "outbox drained",
            time.perf_counter() - started,
            0,
            [0.0],
            await wal_since(conn, start, triggers),
            await outbox_size(conn),
        )
        equal = totals == await reference_from_sql(conn)
    await conn.close()
    return phases, equal


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
    plain, _ = await run_taxi(base, trips, triggers=False)
    outbox, equal = await run_taxi(base, trips, triggers=True)
    record(
        "taxi/reference",
        bool(equal),
        "count and fare per hour and payment type, outbox vs PostgreSQL",
    )
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
    # Sized as a small server would be; everything else stays default,
    # checkpoints and autovacuum included, as a production server runs.
    # wal_keep_size only keeps each phase's WAL until it is read back.
    settings = {"shared_buffers": "2GB", "wal_keep_size": "8GB"}
    services = {
        POSTGRES: postgres_service(POSTGRES_IMAGE, password, settings, shm_size="1g")
    }
    with Stack("straum-outbox", services) as stack:
        stack.up(POSTGRES)
        return asyncio.run(run(stack, password))


if __name__ == "__main__":
    sys.exit(main())
