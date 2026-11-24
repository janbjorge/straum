#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = [
#     "aiohttp", "asyncpg", "orjson", "psutil", "pyarrow", "tabulate",
# ]
#
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""Straum and PostgreSQL on six months of New York taxi trips, at the
major STRAUM_TEST_POSTGRES names, 18 by default.

Every mode loads the trips into a throwaway PostgreSQL and starts
`straum run` on bench/trips.sql. Then:

asks: each ask goes to PostgreSQL as the SQL anyone would write over the raw
tables and to Straum as JSON over HTTP. The answers must be equal, decimals
exactly, PostgreSQL's avg rounded half to even at Straum's places. Then each
side is timed from this one client, over its own protocol, with a warm
cache. The table goes to stdout.

transactions: Straum holds a transaction until its commit and applies it
whole under its write lock, so every ask waits for it, and the rows wait in
memory. This changes N rows in one transaction for several N while asking
Straum back to back, and reports the time from the commit until Straum had
applied it, the longest ask meanwhile, which is the lock hold as a client
feels it, and how much Straum's resident set grew. After each N it checks
the fares by payment type against PostgreSQL and fails if an answer
differs or Straum stops answering. --max-transaction-rows takes its
default from this table.

materialized: builds a materialized view per view of bench/trips.sql, the
same keys at its narrowest bucket (bench/materialized.py), and times its
build, a refresh and a concurrent refresh, and its size. Then it asks as
asks does, and asks the materialized views too where one can answer;
their answers must equal Straum's as well. A median, percentile, mode or
distinct count has no such form: its row says it needs the raw rows.

load: starts Straum while a writer changes trips, and samples each
second, until Straum is ready: the WAL PostgreSQL keeps for Straum's slot,
Straum's memory and its /readyz text. Then it stops the writer, waits for
Straum to apply the last change, and checks the fares by payment type
against PostgreSQL. Straum reads the WAL while the tables load and holds
the commits itself, so the WAL kept should stay small however long the
load takes. Straum's log goes to stderr.

retention: runs Straum on bench/trips.sql as is, then with widths kept
for a time (bench/retention.py), and reports the memory each run holds.
Asks it can answer must equal PostgreSQL's; asks of dropped widths must
be refused.

shell: leaves you in `straum shell` at the trips.

--wal physical has Straum read the physical WAL instead of logical decoding.

Needs Docker, cargo and uv. Downloads go to bench/data, which git ignores.
"""

import argparse
import asyncio
import contextlib
import decimal
import json
import os
import pathlib
import secrets
import socket
import statistics
import subprocess
import sys
import tempfile
import time
from collections.abc import AsyncIterator

import aiohttp
import asyncpg
import orjson
import psutil
import retention
import tabulate
from answers import alike, duration
from materialized import (
    MATERIALIZED_ASKS,
    MATERIALIZED_VIEWS,
    MaterializedView,
    answer_cell,
    create_statements,
)
from stack import (
    POSTGRES,
    POSTGRES_IMAGE,
    POSTGRES_PORT,
    Stack,
    build_environment,
    git_version,
    postgres_service,
)

ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = ROOT / "bench" / "trips.sql"
MONTHS = 6
DATA = ROOT / "bench" / "data"
NEW_YORK = {"timezone": "America/New_York"}

# The asks name a range from 2000 to 2030, which holds every trip, so each
# matches SQL without one; an ask may leave either end open too.
ALL_TIME = ["2000-01-01T00:00:00Z", "2030-01-01T00:00:00Z"]
ONE_DAY = ["2024-01-17T00:00:00-05:00", "2024-01-18T00:00:00-05:00"]
# January 2024 in UTC days, on the edges of the view's day buckets.
ONE_MONTH = ["2024-01-01T00:00:00Z", "2024-02-01T00:00:00Z"]

# Each ask as PostgreSQL SQL over the raw tables, then as a Straum SQL ask
# over one of the views, whose values must be parameters.
ASKS = [
    (
        "Trips",
        "SELECT count(*) FROM yellow_trips",
        "SELECT trips FROM trips WHERE bucket >= $1 AND bucket < $2",
        ALL_TIME,
    ),
    (
        "Card trips",
        "SELECT count(*) FROM yellow_trips WHERE payment_type = 1",
        """SELECT trips FROM trips
            WHERE payment_type = $1 AND bucket >= $2 AND bucket < $3""",
        [1, *ALL_TIME],
    ),
    (
        "Trips by pickup borough",
        """SELECT z.borough, count(*)
             FROM yellow_trips t
             LEFT JOIN taxi_zones z ON z.location_id = t.pu_location_id
            GROUP BY z.borough""",
        """SELECT borough, trips FROM trips
            WHERE bucket >= $1 AND bucket < $2
            GROUP BY borough""",
        ALL_TIME,
    ),
    (
        "Fares by payment type",
        """SELECT payment_type, count(*), sum(fare_amount), avg(fare_amount)
             FROM yellow_trips
            GROUP BY payment_type""",
        """SELECT payment_type, trips, fare, avg_fare FROM trips
            WHERE bucket >= $1 AND bucket < $2
            GROUP BY payment_type""",
        ALL_TIME,
    ),
    (
        "Fare spread by payment type",
        """SELECT payment_type, var_samp(fare_amount), stddev_samp(fare_amount)
             FROM yellow_trips
            GROUP BY payment_type""",
        """SELECT payment_type, fare_variance, fare_deviation FROM trips
            WHERE bucket >= $1 AND bucket < $2
            GROUP BY payment_type""",
        ALL_TIME,
    ),
    (
        "Median fare",
        "SELECT percentile_disc(0.5) WITHIN GROUP (ORDER BY fare_amount) FROM yellow_trips",
        "SELECT median_fare FROM trips WHERE bucket >= $1 AND bucket < $2",
        ALL_TIME,
    ),
    (
        "p99 fare by payment type",
        """SELECT payment_type,
                  percentile_disc(0.99) WITHIN GROUP (ORDER BY fare_amount)
             FROM yellow_trips
            GROUP BY payment_type""",
        """SELECT payment_type, p99_fare FROM trips
            WHERE bucket >= $1 AND bucket < $2
            GROUP BY payment_type""",
        ALL_TIME,
    ),
    (
        "Most common fare by payment type",
        """SELECT payment_type, mode() WITHIN GROUP (ORDER BY fare_amount)
             FROM yellow_trips
            GROUP BY payment_type""",
        """SELECT payment_type, common_fare FROM trips
            WHERE bucket >= $1 AND bucket < $2
            GROUP BY payment_type""",
        ALL_TIME,
    ),
    (
        "Distinct fares by payment type",
        """SELECT payment_type, count(DISTINCT fare_amount), sum(DISTINCT fare_amount),
                  avg(DISTINCT fare_amount)
             FROM yellow_trips
            GROUP BY payment_type""",
        """SELECT payment_type, distinct_fares, distinct_fare_sum, distinct_fare_avg
             FROM trips
            WHERE bucket >= $1 AND bucket < $2
            GROUP BY payment_type""",
        ALL_TIME,
    ),
    (
        "Median fare per hour, one day",
        """SELECT date_trunc('hour', pickup_at),
                  percentile_disc(0.5) WITHIN GROUP (ORDER BY fare_amount)
             FROM yellow_trips
            WHERE pickup_at >= '2024-01-17' AND pickup_at < '2024-01-18'
            GROUP BY 1""",
        """SELECT bucket, median_fare FROM trips
            WHERE bucket >= $1 AND bucket < $2
            GROUP BY bucket""",
        ONE_DAY,
    ),
    (
        "Manhattan pickups, one week",
        """SELECT count(*)
             FROM yellow_trips t
             JOIN taxi_zones z ON z.location_id = t.pu_location_id
            WHERE z.borough = 'Manhattan'
              AND t.pickup_at >= '2024-01-08' AND t.pickup_at < '2024-01-15'""",
        """SELECT trips FROM trips
            WHERE borough = $1 AND bucket >= $2 AND bucket < $3""",
        ["Manhattan", "2024-01-08T00:00:00-05:00", "2024-01-15T00:00:00-05:00"],
    ),
    (
        "Trips per hour, one day",
        """SELECT date_trunc('hour', pickup_at), count(*)
             FROM yellow_trips
            WHERE pickup_at >= '2024-01-17' AND pickup_at < '2024-01-18'
            GROUP BY 1""",
        """SELECT bucket, trips FROM trips
            WHERE bucket >= $1 AND bucket < $2
            GROUP BY bucket""",
        ONE_DAY,
    ),
    # One month by borough, rows an hour then a day apart: the daily rows
    # read the view's day buckets, one stored bucket a row. Days are UTC
    # days, on the epoch grid, as date_bin with the epoch origin bins them.
    (
        "Trips per hour by borough, one month",
        """SELECT date_bin('1 hour', t.pickup_at, timestamptz 'epoch'), z.borough, count(*)
             FROM yellow_trips t
             LEFT JOIN taxi_zones z ON z.location_id = t.pu_location_id
            WHERE t.pickup_at >= '2024-01-01T00:00:00Z' AND t.pickup_at < '2024-02-01T00:00:00Z'
            GROUP BY 1, 2""",
        """SELECT bucket, borough, trips FROM trips
            WHERE bucket >= $1 AND bucket < $2
            GROUP BY bucket, borough""",
        ONE_MONTH,
    ),
    (
        "Trips per day by borough, one month",
        """SELECT date_bin('1 day', t.pickup_at, timestamptz 'epoch'), z.borough, count(*)
             FROM yellow_trips t
             LEFT JOIN taxi_zones z ON z.location_id = t.pu_location_id
            WHERE t.pickup_at >= '2024-01-01T00:00:00Z' AND t.pickup_at < '2024-02-01T00:00:00Z'
            GROUP BY 1, 2""",
        """SELECT buckets(bucket, '1 day'), borough, trips FROM trips
            WHERE bucket >= $1 AND bucket < $2
            GROUP BY buckets(bucket, '1 day'), borough""",
        ONE_MONTH,
    ),
    # 7-day rows start on Thursdays, the epoch's weekday. Open ends read
    # every bucket held, so a stray old trip makes a row of its own.
    (
        "Fares per week",
        """SELECT date_bin('7 days', pickup_at, timestamptz 'epoch'), count(*),
                  sum(fare_amount)
             FROM yellow_trips
            WHERE pickup_at IS NOT NULL
            GROUP BY 1""",
        """SELECT buckets(bucket, '7 days'), trips, fare FROM trips
            GROUP BY buckets(bucket, '7 days')""",
        [],
    ),
    (
        "Tips by dropoff borough",
        """SELECT z.borough, count(*), sum(t.tip_amount), avg(t.trip_distance),
                  sum(t.fare_amount)
             FROM yellow_trips t
             LEFT JOIN taxi_zones z ON z.location_id = t.do_location_id
            GROUP BY z.borough""",
        """SELECT borough, trips, tips, avg_distance, fare FROM dropoffs
            WHERE bucket >= $1 AND bucket < $2
            GROUP BY borough""",
        ALL_TIME,
    ),
]

SIZES = [1_000, 10_000, 100_000, 1_000_000]
# The ask the transactions mode checks after each size: the update moves
# every fare it touches.
FARES_SQL, FARES_ASK = next(
    (sql, orjson.dumps({"query": query, "parameters": parameters}))
    for name, sql, query, parameters in ASKS
    if name == "Fares by payment type"
)
# One day of hour buckets: the ask a dashboard refreshes.
DASHBOARD_ASK = orjson.dumps(
    {
        "query": """SELECT bucket, trips FROM trips
                     WHERE bucket >= $1 AND bucket < $2 GROUP BY bucket""",
        "parameters": ONE_DAY,
    }
)


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__.splitlines()[0],
        epilog="Needs Docker, cargo and uv. Downloads go to bench/data.",
    )
    parser.add_argument(
        "mode",
        nargs="?",
        choices=["asks", "transactions", "materialized", "load", "retention", "shell"],
        default="asks",
        help="asks: time the asks on both sides (default); "
        "transactions: what one large transaction costs Straum; "
        "materialized: the asks over materialized views too, and their upkeep; "
        "load: the WAL kept and memory held while Straum loads under writes; "
        "retention: the memory widths kept for a time save; "
        "shell: open straum shell at the trips",
    )
    parser.add_argument(
        "--wal",
        choices=["logical", "physical"],
        default="logical",
        help="the WAL Straum reads (default logical)",
    )
    args = parser.parse_args()
    straum = build()
    # The plan reads its role from STRAUM_DSN, else PGUSER, else the OS
    # user; the bench's role is straum, whatever the caller exported.
    environment = {
        name: value
        for name, value in os.environ.items()
        if name != "STRAUM_DSN" and not name.startswith("PG")
    }
    # The plan takes only the role from these; no host is reached.
    environment["PGUSER"] = "straum"
    environment["PGHOST"] = "localhost"
    plan = subprocess.check_output(
        [straum, "plan", SPEC, "--wal", args.wal], text=True, env=environment
    )
    password = secrets.token_hex(16)
    with Stack("straum-taxi", {POSTGRES: postgres(password, args.wal)}) as stack:
        log(f"starting PostgreSQL in stack {stack.name}")
        stack.up(POSTGRES)
        return asyncio.run(run(stack, straum, plan, password, args.mode, args.wal))


async def run(
    stack: Stack,
    straum: str,
    plan: str,
    password: str,
    mode: str,
    wal: str,
) -> int:
    port = stack.port(POSTGRES, POSTGRES_PORT)
    url = f"postgres://postgres:{password}@127.0.0.1:{port}/postgres"
    async with connected(url) as pg:
        # Set here, not on the command line, which ALTER SYSTEM can't
        # override when --wal physical turns page images back on and when
        # the load is done and commits flush again.
        await pg.execute("ALTER SYSTEM SET full_page_writes = off")
        await pg.execute("ALTER SYSTEM SET synchronous_commit = off")
        await pg.execute("SELECT pg_reload_conf()")
        log(f"loading {MONTHS} months")
        load(stack)
        # The asks over a week or a day read this index.
        await pg.execute("CREATE INDEX ON yellow_trips (pickup_at)")
        await pg.execute("VACUUM ANALYZE")
        # A commit is acknowledged once flushed, as on a real server, so
        # "Commit to applied" doesn't count the flush the client never
        # waited for.
        await pg.execute("ALTER SYSTEM RESET synchronous_commit")
        await pg.execute("SELECT pg_reload_conf()")
        if wal == "physical":
            # The physical stream rebuilds rows from page images, which the
            # load skipped; after this checkpoint each page's next change
            # brings it whole.
            await pg.execute("ALTER SYSTEM RESET full_page_writes")
            await pg.execute("SELECT pg_reload_conf()")
            await pg.execute("CHECKPOINT")
        # The plan writes a statement a line, and ALTER SYSTEM can't share a
        # query string with others.
        for line in plan.splitlines():
            if line and not line.startswith("--"):
                await pg.execute(line)
        await pg.execute(f"ALTER ROLE straum PASSWORD '{password}'")
        dsn = f"postgres://straum:{password}@127.0.0.1:{port}/postgres?sslmode=disable"
        flags = ["--wal", wal]
        match mode:
            case "shell":
                return await shell(straum, dsn, flags)
            case "transactions":
                return await transactions(pg, straum, dsn, flags)
            case "materialized":
                return await materialized(pg, straum, dsn, flags)
            case "load":
                return await load_under_writes(pg, url, straum, dsn, flags)
            case "retention":
                return await retention_runs(pg, straum, dsn, flags)
            case _:
                return await asks(pg, straum, dsn, flags)


@contextlib.asynccontextmanager
async def connected(url: str) -> AsyncIterator[asyncpg.Connection]:
    """A connection to PostgreSQL at `url`, in New York's zone, closed on
    the way out."""
    pg = await asyncpg.connect(url, server_settings=NEW_YORK)
    try:
        yield pg
    finally:
        await pg.close()


async def shell(straum: str, dsn: str, flags: list[str]) -> int:
    async with serve(straum, dsn, *flags) as (_, loaded, _, url):
        log(f"straum ready in {loaded:.1f} s")
        process = await asyncio.create_subprocess_exec(straum, "shell", "--url", url)
        return await process.wait()


async def asks(pg: asyncpg.Connection, straum: str, dsn: str, flags: list[str]) -> int:
    version = (await pg.fetchval("SHOW server_version")).split()[0]
    trips, size = await pg.fetchrow(
        "SELECT count(*), pg_total_relation_size('yellow_trips') FROM yellow_trips"
    )
    async with serve(straum, dsn, *flags) as (http, loaded, process, _):
        rss = held(process)
        log("asking")
        results = [await compare(pg, http, *each) for each in ASKS]

    print(f"PostgreSQL {version}: {trips:,} trips, {size / 2**20:,.0f} MiB.")
    print(f"Straum: ready in {loaded:.1f} s, holding {rss / 2**20:,.0f} MiB.")
    print()
    table = [
        (
            name,
            rows,
            duration(pg_time),
            duration(straum_time),
            f"{pg_time / straum_time:,.0f}x",
        )
        for name, rows, pg_time, straum_time in results
    ]
    print_table(table, ["Ask", "Rows", "PostgreSQL", "Straum", "Faster"], "lrrrr")
    return 0


async def transactions(
    pg: asyncpg.Connection, straum: str, dsn: str, flags: list[str]
) -> int:
    # The cap is out of the way: this measures what it would prevent.
    cap = str(10 * max(SIZES))
    async with serve(straum, dsn, *flags, "--max-transaction-rows", cap) as (
        http,
        loaded,
        process,
        _,
    ):
        footprint = held(process)
        rss = process.memory_info().rss
        log(f"straum ready in {loaded:.1f} s; asking while idle")
        idle = statistics.median(await ask_until(http, lambda: True, 1.0))
        rows = []
        for n in SIZES:
            log(f"updating {n:,} rows in one transaction")
            applied, stall, peak = await transaction(pg, http, process, n)
            await check(pg, http, f"fares after {n:,} rows", FARES_SQL, FARES_ASK)
            grew = max(0, peak - rss)
            rows.append(
                (
                    f"{n:,}",
                    duration(applied),
                    duration(stall),
                    f"{grew / 2**20:,.0f} MiB",
                )
            )
    print(
        f"An idle ask takes {duration(idle)}; "
        f"Straum held {footprint / 2**20:,.0f} MiB after the load."
    )
    print()
    headers = [
        "Rows changed",
        "Commit to applied",
        "Longest ask meanwhile",
        "Memory grown",
    ]
    print_table(rows, headers, "rrrr")
    return 0


async def materialized(
    pg: asyncpg.Connection, straum: str, dsn: str, flags: list[str]
) -> int:
    version = (await pg.fetchval("SHOW server_version")).split()[0]
    trips, size = await pg.fetchrow(
        "SELECT count(*), pg_total_relation_size('yellow_trips') FROM yellow_trips"
    )
    upkeep = []
    for view in MATERIALIZED_VIEWS:
        log(f"building and refreshing {view.name}")
        upkeep.append(await keep(pg, view))
    async with serve(straum, dsn, *flags) as (http, loaded, process, _):
        rss = held(process)
        log("asking")
        results = [await compare_materialized(pg, http, *each) for each in ASKS]

    views_size = sum(each[2] for each in upkeep)
    print(f"PostgreSQL {version}: {trips:,} trips, {size / 2**20:,.0f} MiB.")
    print(
        f"Materialized views: {views_size / 2**20:,.0f} MiB together. "
        f"Straum: ready in {loaded:.1f} s, holding {rss / 2**20:,.0f} MiB."
    )
    print()
    table = [
        (
            name,
            rows,
            duration(pg_time),
            answer_cell(materialized_time),
            duration(straum_time),
        )
        for name, rows, pg_time, materialized_time, straum_time in results
    ]
    headers = ["Ask", "Rows", "PostgreSQL", "Materialized view", "Straum"]
    print_table(table, headers, "lrrrr")
    print()
    table = [
        (
            name,
            f"{rows:,}",
            f"{bytes_on_disk / 2**20:,.0f} MiB",
            duration(built),
            duration(refreshed),
            duration(concurrently),
        )
        for name, rows, bytes_on_disk, built, refreshed, concurrently in upkeep
    ]
    headers = [
        "Materialized view",
        "Rows",
        "Size",
        "Build",
        "Refresh",
        "Refresh concurrently",
    ]
    print_table(table, headers, "lrrrrr")
    return 0


async def retention_runs(
    pg: asyncpg.Connection, straum: str, dsn: str, flags: list[str]
) -> int:
    newest, now = await pg.fetchrow(retention.EDGES_SQL)
    weekly = next(each for each in ASKS if each[0] == "Fares per week")
    rows = []
    with tempfile.TemporaryDirectory() as directory:
        for run in retention.runs(SPEC.read_text(), newest, now, weekly):
            spec = pathlib.Path(directory) / f"{run.name}.sql"
            spec.write_text(run.spec)
            log(f"run {run.name}, kept for a time: {run.kept}")
            async with serve(straum, dsn, *flags, spec=spec) as (http, loaded, process, _):
                memory = held(process)
                async with http.get("/views") as response:
                    cells = retention.cells(await response.text())
                times = [(await compare(pg, http, *each))[3] for each in run.asks]
                for name, query in run.refused:
                    await retention.refused(http, f"{name}, run {run.name}", query)
            rows.append(retention.Measured(run, loaded, memory, cells, times))
    print(retention.note(newest, now))
    print()
    print_table(retention.table(rows), retention.HEADERS, "llrrrrrrr")
    return 0


# Trips one write changes; well under --max-transaction-rows.
WRITE_ROWS = 1_000


async def load_under_writes(
    pg: asyncpg.Connection, url: str, straum: str, dsn: str, flags: list[str]
) -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    stopped = asyncio.Event()
    async with connected(url) as writer_pg:
        writer = asyncio.create_task(write_until(writer_pg, stopped))
        started = time.perf_counter()
        server = await asyncio.create_subprocess_exec(
            straum,
            "run",
            SPEC,
            "--listen",
            f"127.0.0.1:{port}",
            *flags,
            env=os.environ | {"STRAUM_DSN": dsn},
        )
        samples = []
        try:
            async with aiohttp.ClientSession(
                f"http://127.0.0.1:{port}",
                headers={"Content-Type": "application/json"},
            ) as http:
                process = psutil.Process(server.pid)
                while True:
                    if server.returncode is not None:
                        raise SystemExit("taxi: straum stopped while loading")
                    ready, why = await readiness(http)
                    # What the server keeps, and what Straum has reported.
                    kept, behind = await pg.fetchrow(
                        "SELECT"
                        " max(pg_wal_lsn_diff(pg_current_wal_lsn(), restart_lsn)),"
                        " max(pg_wal_lsn_diff(pg_current_wal_lsn(),"
                        " coalesce(confirmed_flush_lsn, restart_lsn)))"
                        " FROM pg_replication_slots WHERE slot_name LIKE 'straum%'"
                    )
                    seconds = time.perf_counter() - started
                    samples.append((seconds, kept, behind, held(process), why))
                    if ready:
                        break
                    await asyncio.sleep(1)
                stopped.set()
                writes, before_last = await writer
                log(f"{writes:,} writes during the load; waiting for the last")
                # The last write is the next commit after `before_last`.
                while await applied(http) <= before_last:
                    await asyncio.sleep(0.05)
                await check(pg, http, "fares after the load", FARES_SQL, FARES_ASK)
        finally:
            stopped.set()
            with contextlib.suppress(ProcessLookupError):
                server.terminate()
            await server.wait()
    table = [
        (
            f"{seconds:.0f}",
            mebibytes(kept),
            mebibytes(behind),
            f"{memory / 2**20:,.0f} MiB",
            why,
        )
        for seconds, kept, behind, memory, why in samples
    ]
    headers = [
        "Second",
        "WAL kept for the slot",
        "Not yet reported",
        "Straum memory",
        "/readyz",
    ]
    print_table(table, headers, "rrrrl")
    return 0


def mebibytes(count: int | None) -> str:
    return "no slot" if count is None else f"{count / 2**20:,.1f} MiB"


async def write_until(
    pg: asyncpg.Connection, stopped: asyncio.Event
) -> tuple[int, int]:
    """Changes WRITE_ROWS trips a transaction, round the table, until
    `stopped`: how many transactions, and the WAL position before the
    last."""
    trips = await pg.fetchval("SELECT max(id) FROM yellow_trips")
    writes = 0
    first = 1
    before = 0
    while not stopped.is_set():
        before = lsn(await pg.fetchval("SELECT pg_current_wal_lsn()::text"))
        await pg.execute(
            "UPDATE yellow_trips SET fare_amount = fare_amount + 1"
            f" WHERE id >= {first} AND id < {first + WRITE_ROWS}"
        )
        writes += 1
        first = first + WRITE_ROWS if first + WRITE_ROWS <= trips else 1
    return writes, before


async def readiness(http: aiohttp.ClientSession) -> tuple[bool, str]:
    """Whether Straum is ready, and its /readyz text; not listening yet
    reads as not ready."""
    try:
        async with http.get("/readyz") as response:
            return response.status == 200, (await response.text()).strip()
    except aiohttp.ClientError:
        return False, "not listening"


# Refreshes of each kind per materialized view; the median counts. Each
# reads every trip, so a few, not the second that median() runs.
REFRESH_RUNS = 3


async def keep(
    pg: asyncpg.Connection, view: MaterializedView
) -> tuple[str, int, int, float, float, float]:
    """Builds `view`, then refreshes it each way: its rows, its bytes
    with its index, and the seconds of the build and of each refresh."""
    create, index = create_statements(view)
    start = time.perf_counter()
    await pg.execute(create)
    await pg.execute(index)
    built = time.perf_counter() - start
    await pg.execute(f"VACUUM ANALYZE {view.name}")
    refreshes = {}
    for how in ["", "CONCURRENTLY "]:
        times = []
        for _ in range(REFRESH_RUNS):
            start = time.perf_counter()
            await pg.execute(f"REFRESH MATERIALIZED VIEW {how}{view.name}")
            times.append(time.perf_counter() - start)
        refreshes[how] = statistics.median(times)
    # The answers are timed after a vacuum, as after an autovacuum.
    await pg.execute(f"VACUUM ANALYZE {view.name}")
    rows, bytes_on_disk = await pg.fetchrow(
        f"SELECT count(*), pg_total_relation_size('{view.name}') FROM {view.name}"
    )
    return (
        view.name,
        rows,
        bytes_on_disk,
        built,
        refreshes[""],
        refreshes["CONCURRENTLY "],
    )


async def compare_materialized(
    pg: asyncpg.Connection,
    http: aiohttp.ClientSession,
    name: str,
    sql: str,
    query: str,
    parameters: list,
) -> tuple[str, int, float, float | None, float]:
    """As `compare`, and the ask over the materialized views where one
    answers it: it must answer as Straum does, then it is timed."""
    name, rows, pg_time, straum_time = await compare(
        pg, http, name, sql, query, parameters
    )
    materialized_sql = MATERIALIZED_ASKS[name]
    if materialized_sql is None:
        return name, rows, pg_time, None, straum_time
    body = orjson.dumps({"query": query, "parameters": parameters})
    await check(pg, http, f"{name}, materialized view", materialized_sql, body)
    materialized_time = await median(lambda: pg.fetch(materialized_sql))
    return name, rows, pg_time, materialized_time, straum_time


def print_table(rows: list, headers: list[str], align: str) -> None:
    # Pipe keeps the alignment colons that GitHub's format drops.
    print(
        tabulate.tabulate(
            rows,
            headers=headers,
            tablefmt="pipe",
            colalign=["left" if side == "l" else "right" for side in align],
            disable_numparse=True,
        )
    )


def load(stack: Stack) -> None:
    loader = subprocess.Popen(
        [
            ROOT / "bench" / "load_taxi_trips.py",
            "--months",
            str(MONTHS),
            "--data",
            DATA,
        ],
        stdout=subprocess.PIPE,
    )
    psql = stack.exec_args(POSTGRES, "psql", "-U", "postgres", "-q")
    subprocess.run(psql, stdin=loader.stdout, check=True)
    loader.stdout.close()
    if loader.wait():
        raise SystemExit("load_taxi_trips.py failed")


def postgres(password: str, wal: str) -> dict:
    # Reads see the defaults but for shared_buffers, sized as a server
    # would be. The rest only speed the load: no checkpoint midway and
    # memory for the index builds. run() turns page images and the flush
    # per commit off for the load, and the flush back on after it. --wal
    # physical is for a server at replica, the default, so it runs on one.
    settings = {
        "wal_level": "replica" if wal == "physical" else "logical",
        "shared_buffers": "2GB",
        "max_wal_size": "16GB",
        "checkpoint_timeout": "1h",
        "maintenance_work_mem": "1GB",
        "max_parallel_maintenance_workers": "4",
    }
    return postgres_service(POSTGRES_IMAGE, password, settings, shm_size="1g")


def build() -> str:
    log("building straum")
    messages = subprocess.run(
        ["cargo", "build", "--release", "--locked", "-p", "straum"]
        + ["--message-format=json-render-diagnostics"],
        cwd=ROOT,
        env=build_environment(os.environ, git_version()),
        check=True,
        stdout=subprocess.PIPE,
        text=True,
    ).stdout
    return next(
        message["executable"]
        for message in map(orjson.loads, messages.splitlines())
        if message.get("executable")
    )


@contextlib.asynccontextmanager
async def serve(straum: str, dsn: str, *flags: str, spec: pathlib.Path = SPEC):
    """`straum run` on `spec` with `flags`, once ready: a session to ask
    it, how long it took to load, its process, and its URL."""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    started = time.perf_counter()
    server = await asyncio.create_subprocess_exec(
        straum,
        "run",
        spec,
        "--listen",
        f"127.0.0.1:{port}",
        *flags,
        env=os.environ | {"STRAUM_DSN": dsn},
    )
    try:
        async with aiohttp.ClientSession(
            f"http://127.0.0.1:{port}", headers={"Content-Type": "application/json"}
        ) as http:
            await wait_ready(server, http)
            loaded = time.perf_counter() - started
            yield http, loaded, psutil.Process(server.pid), f"http://127.0.0.1:{port}"
    finally:
        with contextlib.suppress(ProcessLookupError):
            server.terminate()
        await server.wait()


async def wait_ready(
    server: asyncio.subprocess.Process, http: aiohttp.ClientSession
) -> None:
    deadline = time.monotonic() + 120
    while server.returncode is None and time.monotonic() < deadline:
        with contextlib.suppress(aiohttp.ClientError):
            async with http.get("/readyz") as response:
                if response.status == 200:
                    return
        await asyncio.sleep(0.05)
    raise SystemExit("taxi: straum did not get ready")


async def ask(http: aiohttp.ClientSession, body: bytes, decode=orjson.loads) -> list:
    async with http.post("/ask", data=body) as response:
        reply = await response.read()
    # A refusal may be no JSON, as from a proxy: its words, not a decode
    # error.
    if response.status != 200:
        said = reply.decode(errors="replace")
        raise SystemExit(f"taxi: straum answered {response.status}: {said}")
    return decode(reply)["rows"]


def exact(reply: bytes) -> dict:
    """The reply with its decimals as decimals: orjson reads them as
    floats, which round."""
    return json.loads(reply, parse_float=decimal.Decimal)


async def compare(
    pg: asyncpg.Connection,
    http: aiohttp.ClientSession,
    name: str,
    sql: str,
    query: str,
    parameters: list,
) -> tuple[str, int, float, float]:
    """Checks that both sides answer alike, then times each."""
    body = orjson.dumps({"query": query, "parameters": parameters})
    rows = await check(pg, http, name, sql, body)
    pg_time = await median(lambda: pg.fetch(sql))
    return name, rows, pg_time, await median(lambda: ask(http, body))


async def check(
    pg: asyncpg.Connection,
    http: aiohttp.ClientSession,
    name: str,
    sql: str,
    body: bytes,
) -> int:
    """Fails unless both sides answer alike; the rows they answered."""
    expected = [list(row.values()) for row in await pg.fetch(sql)]
    got = [list(row.values()) for row in await ask(http, body, exact)]
    if not alike(got, expected):
        raise SystemExit(f"taxi: {name}: Straum answered {got}, PostgreSQL {expected}")
    return len(got)


async def median(run) -> float:
    """Median seconds of `run` over at least a second and five runs."""
    times = []
    end = time.perf_counter() + 1
    while len(times) < 5 or time.perf_counter() < end:
        start = time.perf_counter()
        await run()
        times.append(time.perf_counter() - start)
    return statistics.median(times)


async def ask_until(http: aiohttp.ClientSession, done, at_least: float) -> list[float]:
    """Asks back to back until `done()` holds and `at_least` seconds have
    passed; each ask's seconds."""
    times = []
    end = time.perf_counter() + at_least
    while not done() or time.perf_counter() < end:
        start = time.perf_counter()
        async with http.post("/ask", data=DASHBOARD_ASK) as response:
            await response.read()
            if response.status != 200:
                raise SystemExit(f"taxi: straum answered {response.status}")
        times.append(time.perf_counter() - start)
    return times


async def transaction(
    pg: asyncpg.Connection, http: aiohttp.ClientSession, process: psutil.Process, n: int
) -> tuple[float, float, int]:
    """Changes `n` rows in one transaction: seconds from its commit until
    Straum had applied it, the longest ask meanwhile, and Straum's largest
    resident set meanwhile, in bytes."""
    before = await pg.fetchval("SELECT pg_current_wal_lsn()::text")
    finished = False
    asker = asyncio.create_task(ask_until(http, lambda: finished, 0))
    peak = 0
    await pg.execute(
        f"UPDATE yellow_trips SET fare_amount = fare_amount + 1 WHERE id <= {n}"
    )
    committed = time.perf_counter()
    # The update is the next commit after `before`, so Straum is past it
    # once its applied position is. The rows wait in Straum meanwhile.
    while await applied(http) <= lsn(before):
        peak = max(peak, process.memory_info().rss)
        await asyncio.sleep(0.002)
    took = time.perf_counter() - committed
    finished = True
    return took, max(await asker), max(peak, process.memory_info().rss)


async def applied(http: aiohttp.ClientSession) -> int:
    position = (await status_field(http, "applied")).split(",")[0]
    return lsn(position) if "/" in position else 0


async def status_field(http: aiohttp.ClientSession, name: str) -> str:
    """The value /status gives `name`, or "" if it gives none."""
    async with http.get("/status") as response:
        text = await response.text()
    for line in text.splitlines():
        if line.startswith(f"{name}:"):
            return line.split(":", 1)[1].strip()
    return ""


def held(process: psutil.Process) -> int:
    """The bytes `process` holds. On macOS the resident set leaves out
    pages the system compressed, which a busy machine does at once, so
    this reads the footprint, which counts them."""
    if sys.platform != "darwin":
        return process.memory_info().rss
    with tempfile.TemporaryDirectory() as directory:
        report = pathlib.Path(directory) / "footprint.json"
        subprocess.run(
            ["/usr/bin/footprint", "-p", str(process.pid), "-j", str(report)],
            check=True,
            capture_output=True,
        )
        summary = orjson.loads(report.read_bytes())
    return summary["processes"][0]["footprint"]


def lsn(text: str) -> int:
    high, low = text.split("/")
    return (int(high, 16) << 32) | int(low, 16)


def log(message: str) -> None:
    print(f"taxi: {message}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    sys.exit(main())
