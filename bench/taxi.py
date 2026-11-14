#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = [
#     "aiohttp", "asyncpg", "orjson", "psutil", "pyarrow", "tabulate",
#     "testcontainers",
# ]
#
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""Straum and PostgreSQL 18 on six months of New York taxi trips.

Every mode loads the trips into a throwaway PostgreSQL and starts
`straum run` on bench/trips.sql. Then:

asks: each ask goes to PostgreSQL as the SQL anyone would write over the raw
tables and to Straum as JSON over HTTP. The answers must be equal. Then each
side is timed from this one client, over its own protocol, with a warm
cache. The table goes to stdout.

transactions: Straum holds a transaction until its commit and applies it
whole under its write lock, so every ask waits for it, and the rows wait in
memory. This changes N rows in one transaction for several N while asking
Straum back to back, and reports the time from the commit until Straum had
applied it, the longest ask meanwhile, which is the lock hold as a client
feels it, and how much Straum's resident set grew. --max-transaction-rows
takes its default from this table.

shell: leaves you in `straum shell` at the trips.

Needs Docker, cargo and uv. Downloads go to bench/data, which git ignores.
"""

import argparse
import asyncio
import contextlib
import datetime
import decimal
import os
import pathlib
import secrets
import socket
import statistics
import subprocess
import sys
import time
from collections import Counter

import aiohttp
import asyncpg
import orjson
import psutil
import tabulate
from testcontainers.community.postgres import PostgresContainer

ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = ROOT / "bench" / "trips.sql"
MONTHS = 6
DATA = ROOT / "bench" / "data"
NEW_YORK = {"timezone": "America/New_York"}

# A Straum ask always names its range; 2000 to 2030 holds every trip, so it
# matches SQL without one.
ALL_TIME = ["2000-01-01T00:00:00Z", "2030-01-01T00:00:00Z"]
ONE_DAY = ["2024-01-17T00:00:00-05:00", "2024-01-18T00:00:00-05:00"]

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
        choices=["asks", "transactions", "shell"],
        default="asks",
        help="asks: time the asks on both sides (default); "
        "transactions: what one large transaction costs Straum; "
        "shell: open straum shell at the trips",
    )
    args = parser.parse_args()
    straum = build()
    plan = subprocess.check_output([straum, "plan", "--spec", SPEC], text=True)
    # Docker publishes the port on every interface while this runs.
    password = secrets.token_hex(16)
    postgres = container(password)
    log("starting PostgreSQL")
    with postgres:
        return asyncio.run(run(postgres, straum, plan, password, args.mode))


async def run(
    postgres: PostgresContainer, straum: str, plan: str, password: str, mode: str
) -> int:
    url = postgres.get_connection_url()
    log(f"loading {MONTHS} months")
    load(postgres)
    pg = await asyncpg.connect(url, server_settings=NEW_YORK)
    try:
        # The asks over a week or a day read this index.
        await pg.execute("CREATE INDEX ON yellow_trips (pickup_at)")
        await pg.execute("VACUUM ANALYZE")
        # The plan writes a statement a line, and ALTER SYSTEM can't share a
        # query string with others.
        for line in plan.splitlines():
            if line and not line.startswith("--"):
                await pg.execute(line)
        await pg.execute(f"ALTER ROLE straum PASSWORD '{password}'")
        host, port = postgres.get_container_host_ip(), postgres.get_exposed_port(5432)
        dsn = f"postgres://straum:{password}@{host}:{port}/postgres?sslmode=disable"
        match mode:
            case "shell":
                return await shell(straum, dsn)
            case "transactions":
                return await transactions(pg, straum, dsn)
            case _:
                return await asks(pg, straum, dsn)
    finally:
        await pg.close()


async def shell(straum: str, dsn: str) -> int:
    async with serve(straum, dsn) as (_, loaded, _, url):
        log(f"straum ready in {loaded:.1f} s")
        process = await asyncio.create_subprocess_exec(straum, "shell", "--url", url)
        return await process.wait()


async def asks(pg: asyncpg.Connection, straum: str, dsn: str) -> int:
    version = (await pg.fetchval("SHOW server_version")).split()[0]
    trips, size = await pg.fetchrow(
        "SELECT count(*), pg_total_relation_size('yellow_trips') FROM yellow_trips"
    )
    async with serve(straum, dsn) as (http, loaded, process, _):
        rss = process.memory_info().rss
        log("asking")
        results = [await compare(pg, http, *each) for each in ASKS]

    print(f"PostgreSQL {version}: {trips:,} trips, {size / 2**20:,.0f} MiB.")
    print(f"Straum: ready in {loaded:.1f} s, {rss / 2**20:,.0f} MiB resident.")
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


async def transactions(pg: asyncpg.Connection, straum: str, dsn: str) -> int:
    # The cap is out of the way: this measures what it would prevent.
    cap = str(10 * max(SIZES))
    async with serve(straum, dsn, "--max-transaction-rows", cap) as (
        http,
        loaded,
        process,
        _,
    ):
        rss = process.memory_info().rss
        log(f"straum ready in {loaded:.1f} s; asking while idle")
        idle = statistics.median(await ask_until(http, lambda: True, 1.0))
        rows = []
        for n in SIZES:
            log(f"updating {n:,} rows in one transaction")
            applied, stall, peak = await transaction(pg, http, process, n)
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
        f"Straum held {rss / 2**20:,.0f} MiB after the load."
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


def load(postgres: PostgresContainer) -> None:
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
    psql = ["psql", "-U", "postgres", "-q"]
    container = postgres.get_wrapped_container().id
    subprocess.run(
        ["docker", "exec", "-i", container, *psql], stdin=loader.stdout, check=True
    )
    loader.stdout.close()
    if loader.wait():
        raise SystemExit("load_taxi_trips.py failed")


def container(password: str) -> PostgresContainer:
    # Reads see the defaults but for shared_buffers, sized as a server
    # would be. The rest only speed the load: no checkpoint midway, no page
    # images or flush per commit, which a throwaway server never needs, and
    # memory for the index builds.
    settings = {
        "wal_level": "logical",
        "shared_buffers": "2GB",
        "max_wal_size": "16GB",
        "checkpoint_timeout": "1h",
        "full_page_writes": "off",
        "synchronous_commit": "off",
        "maintenance_work_mem": "1GB",
        "max_parallel_maintenance_workers": "4",
    }
    return (
        PostgresContainer(
            "postgres:18",
            username="postgres",
            password=password,
            dbname="postgres",
            driver=None,
        )
        .with_command(
            " ".join(f"-c {name}={value}" for name, value in settings.items())
        )
        .with_kwargs(shm_size="1g")
    )


def build() -> str:
    log("building straum")
    messages = subprocess.run(
        ["cargo", "build", "--release", "--locked", "-p", "straum"]
        + ["--message-format=json-render-diagnostics"],
        cwd=ROOT,
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
async def serve(straum: str, dsn: str, *flags: str):
    """`straum run` with `flags`, once ready: a session to ask it, how long
    it took to load, its process, and its URL."""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    started = time.perf_counter()
    server = await asyncio.create_subprocess_exec(
        straum,
        "run",
        "--spec",
        SPEC,
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


async def ask(http: aiohttp.ClientSession, body: bytes) -> list:
    async with http.post("/ask", data=body) as response:
        answer = orjson.loads(await response.read())
    if response.status != 200:
        raise SystemExit(f"taxi: straum answered {response.status}: {answer}")
    return answer["rows"]


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
    expected = await pg.fetch(sql)
    got = await ask(http, body)
    if Counter(map(plain, got)) != Counter(map(plain, expected)):
        raise SystemExit(f"taxi: {name}: Straum answered {got}, PostgreSQL {expected}")
    pg_time = await median(lambda: pg.fetch(sql))
    return name, len(got), pg_time, await median(lambda: ask(http, body))


def plain(row) -> tuple:
    """The row in one form for both sides. orjson reads Straum's exact
    decimals as floats, so both sides compare as floats to four places,
    which Straum's avg always has; its buckets come as RFC 3339 text."""
    values = []
    for value in row.values():
        match value:
            case decimal.Decimal():
                value = float(value)
            case datetime.datetime():
                value = value.astimezone(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")
        if isinstance(value, float):
            value = round(value, 4)
        values.append(value)
    return tuple(values)


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
    async with http.get("/status") as response:
        text = await response.text()
    for line in text.splitlines():
        if line.startswith("applied:"):
            position = line.split(":", 1)[1].strip().split(",")[0]
            return lsn(position) if "/" in position else 0
    return 0


def lsn(text: str) -> int:
    high, low = text.split("/")
    return (int(high, 16) << 32) | int(low, 16)


def duration(seconds: float) -> str:
    if seconds >= 1e-3:
        return f"{seconds * 1e3:.1f} ms"
    return f"{seconds * 1e6:.0f} µs"


def log(message: str) -> None:
    print(f"taxi: {message}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    sys.exit(main())
