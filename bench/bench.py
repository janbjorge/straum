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
"""Straum against PostgreSQL 18 on a month of New York taxi trips.

Each ask goes to both: to PostgreSQL as the SQL anyone would write over the
raw tables, and to `straum run` on bench/trips.sql as JSON over HTTP. The
answers must be equal. Then each side is timed from this one client, over
its own protocol, with a warm cache. The table goes to stdout.

Needs Docker, cargo and uv. Downloads go to bench/data, which git ignores.
"""

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
import urllib.request
from collections import Counter

import aiohttp
import asyncpg
import orjson
import psutil
import pyarrow.csv
import pyarrow.parquet
import tabulate
from testcontainers.community.postgres import PostgresContainer

ROOT = pathlib.Path(__file__).resolve().parents[1]
DATA = ROOT / "bench" / "data"
SPEC = ROOT / "bench" / "trips.sql"
MONTH = "2024-01"
TLC = "https://d37ci6vzurychx.cloudfront.net"

# TLC name -> yellow_trips column. straum-pg's taxi_month test reads the CSV
# too.
COLUMNS = {
    "VendorID": "vendor_id",
    "tpep_pickup_datetime": "pickup_at",
    "PULocationID": "pu_location_id",
    "DOLocationID": "do_location_id",
    "payment_type": "payment_type",
    "fare_amount": "fare_amount",
    "tip_amount": "tip_amount",
    "trip_distance": "trip_distance",
}

# Money and distance are numeric, so sums are exact on both sides. The
# session runs in New York time, the TLC's wall clock.
LOAD = f"""
CREATE TABLE taxi_zones (
    location_id int PRIMARY KEY,
    borough text,
    zone text,
    service_zone text
);
CREATE TABLE yellow_trips (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    vendor_id int,
    pickup_at timestamptz,
    pu_location_id int,
    do_location_id int,
    payment_type int,
    fare_amount numeric(12, 2),
    tip_amount numeric(12, 2),
    trip_distance numeric(12, 2)
);
COPY taxi_zones FROM '/data/taxi_zone_lookup.csv' WITH (format csv, header);
COPY yellow_trips ({", ".join(COLUMNS.values())})
    FROM '/data/trips_{MONTH}.csv' WITH (format csv, header);
CREATE INDEX ON yellow_trips (pickup_at);
"""

# A Straum ask always names its range; 2000 to 2030 holds every trip, so it
# matches SQL without one.
ALL_TIME = ["2000-01-01T00:00:00Z", "2030-01-01T00:00:00Z"]

# Each ask as PostgreSQL SQL over the raw tables, then as a Straum SQL ask
# over the view, whose values must be parameters.
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
        """SELECT pickup.borough, trips FROM trips
            WHERE bucket >= $1 AND bucket < $2
            GROUP BY pickup.borough""",
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
            WHERE pickup.borough = $1 AND bucket >= $2 AND bucket < $3""",
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
        ["2024-01-17T00:00:00-05:00", "2024-01-18T00:00:00-05:00"],
    ),
]


def main() -> int:
    fetch()
    straum = build()
    plan = subprocess.check_output([straum, "plan", "--spec", SPEC], text=True)
    # Docker publishes the port on every interface while the bench runs.
    password = secrets.token_hex(16)
    # Defaults but for shared_buffers: at 128MB the 248MB table is read
    # through the OS cache on every scan, which no sized server does.
    postgres = (
        PostgresContainer(
            "postgres:18",
            username="postgres",
            password=password,
            dbname="postgres",
            driver=None,
        )
        .with_command("-c wal_level=logical -c shared_buffers=2GB")
        .with_volume_mapping(DATA, "/data")
        .with_kwargs(shm_size="1g")
    )
    log("starting PostgreSQL")
    with postgres:
        return asyncio.run(bench(postgres, straum, plan, password))


async def bench(
    postgres: PostgresContainer, straum: str, plan: str, password: str
) -> int:
    pg = await asyncpg.connect(
        postgres.get_connection_url(),
        server_settings={"timezone": "America/New_York"},
    )
    log("loading the month")
    await pg.execute(LOAD)
    await pg.execute("VACUUM ANALYZE")
    # The plan writes a statement a line, and ALTER SYSTEM can't share a
    # query string with others.
    for line in plan.splitlines():
        if line and not line.startswith("--"):
            await pg.execute(line)
    await pg.execute(f"ALTER ROLE straum PASSWORD '{password}'")
    version = (await pg.fetchval("SHOW server_version")).split()[0]
    trips, size = await pg.fetchrow(
        "SELECT count(*), pg_total_relation_size('yellow_trips') FROM yellow_trips"
    )
    host, port = postgres.get_container_host_ip(), postgres.get_exposed_port(5432)
    dsn = f"postgres://straum:{password}@{host}:{port}/postgres?sslmode=disable"
    async with serve(straum, dsn) as (http, loaded, rss):
        log("asking")
        results = [await compare(pg, http, *each) for each in ASKS]
    await pg.close()

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
    # Pipe keeps the alignment colons that GitHub's format drops.
    print(
        tabulate.tabulate(
            table,
            headers=["Ask", "Rows", "PostgreSQL", "Straum", "Faster"],
            tablefmt="pipe",
            colalign=["left", "right", "right", "right", "right"],
            disable_numparse=True,
        )
    )
    return 0


def fetch() -> None:
    DATA.mkdir(exist_ok=True)
    parquet = DATA / f"yellow_tripdata_{MONTH}.parquet"
    for path, url in [
        (parquet, f"{TLC}/trip-data/{parquet.name}"),
        (DATA / "taxi_zone_lookup.csv", f"{TLC}/misc/taxi_zone_lookup.csv"),
    ]:
        if not path.exists():
            log(f"fetching {url}")
            part = path.with_suffix(".part")
            urllib.request.urlretrieve(url, part)
            part.rename(path)
    csv = DATA / f"trips_{MONTH}.csv"
    if not csv.exists():
        log(f"writing {csv.name}")
        table = pyarrow.parquet.read_table(parquet, columns=list(COLUMNS))
        table = table.select(list(COLUMNS)).rename_columns(list(COLUMNS.values()))
        part = csv.with_suffix(".part")
        pyarrow.csv.write_csv(table, part)
        part.rename(csv)


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
    it took to load, and its resident set in bytes."""
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
            yield http, loaded, psutil.Process(server.pid).memory_info().rss
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
    raise SystemExit("bench: straum did not get ready")


async def ask(http: aiohttp.ClientSession, body: bytes) -> list:
    async with http.post("/ask", data=body) as response:
        answer = orjson.loads(await response.read())
    if response.status != 200:
        raise SystemExit(f"bench: straum answered {response.status}: {answer}")
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
        raise SystemExit(f"bench: {name}: Straum answered {got}, PostgreSQL {expected}")
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


def duration(seconds: float) -> str:
    if seconds >= 1e-3:
        return f"{seconds * 1e3:.1f} ms"
    return f"{seconds * 1e6:.0f} µs"


def log(message: str) -> None:
    print(f"bench: {message}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    sys.exit(main())
