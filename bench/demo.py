#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = ["aiohttp", "asyncpg", "orjson", "testcontainers"]
#
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""January of New York taxi trips, on the published Straum image.

Loads the month into a throwaway PostgreSQL, starts `straum run` from
the image, and prints six beats: the load, one ask timed against
PostgreSQL, an insert, its delete, a borough change, and the
replication slot gone after Straum stops. The times are this
machine's, over January, not the six-month run on the performance
page. Straum reads logical decoding. Needs Docker and uv. Downloads
go to bench/data, which git ignores.
"""

import argparse
import asyncio
import os
import pathlib
import secrets
import statistics
import subprocess
import sys
import time
from collections.abc import Awaitable, Callable, Mapping, Sequence
from datetime import datetime
from typing import IO, Any
from zoneinfo import ZoneInfo

import aiohttp
import asyncpg
import orjson
from testcontainers.community.postgres import PostgresContainer
from testcontainers.core.network import Network

type Rows = Sequence[Mapping[str, Any]]
type Counts = dict[str | None, int]

ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = ROOT / "bench" / "demo.sql"
DATA = ROOT / "bench" / "data"
IMAGE = "ghcr.io/janbjorge/straum:0.8.0"
CONTAINER = "straum-demo"
ALIAS = "straum-pg"
SPEC_IN_IMAGE = f"{SPEC}:/spec/trips.sql:ro"
# Hour edges of the view's date_bin, New York wall time. The January
# file holds a few trips outside the month; both sides use this range.
NEW_YORK_TZ = ZoneInfo("America/New_York")
MONTH = (
    datetime(2024, 1, 1, tzinfo=NEW_YORK_TZ),
    datetime(2024, 2, 1, tzinfo=NEW_YORK_TZ),
)
NEW_YORK = {"timezone": "America/New_York"}
NEWARK = 1
JFK = 132
LAGUARDIA = 138
READY_S = 180
APPLY_S = 30
SLOT_S = 15

BY_BOROUGH_SQL = """
    SELECT z.borough, count(*) AS trips
      FROM yellow_trips t
      LEFT JOIN taxi_zones z ON z.location_id = t.pu_location_id
     WHERE t.pickup_at >= $1::timestamptz AND t.pickup_at < $2::timestamptz
     GROUP BY z.borough
"""
AIRPORT_TRIPS_SQL = f"""
    SELECT count(*) FROM yellow_trips
     WHERE pu_location_id IN ({JFK}, {LAGUARDIA})
       AND pickup_at >= $1::timestamptz AND pickup_at < $2::timestamptz
"""
BY_BOROUGH = """
    SELECT borough, trips FROM trips
     WHERE bucket >= $1 AND bucket < $2
     GROUP BY borough
"""
ONE_BOROUGH = """
    SELECT trips FROM trips
     WHERE borough = $1 AND bucket >= $2 AND bucket < $3
"""


def main() -> int:
    parser = argparse.ArgumentParser(
        description=__doc__.splitlines()[0],
        epilog="Needs Docker and uv. Downloads go to bench/data.",
    )
    parser.parse_args()
    log(f"pulling {IMAGE}")
    docker("pull", IMAGE, check=True)
    stop_straum()
    password = secrets.token_hex(16)
    log("starting PostgreSQL")
    with Network() as network:
        postgres = container(password, network)
        with postgres:
            return asyncio.run(run(postgres, password, network.name))


async def run(postgres: PostgresContainer, password: str, network: str) -> int:
    started = time.perf_counter()
    pg = await asyncpg.connect(
        postgres.get_connection_url(driver=None), server_settings=NEW_YORK
    )
    try:
        log("loading January")
        load(postgres)
        await pg.execute("ANALYZE")
        await apply_plan(pg, password)
        trips = await pg.fetchval("SELECT count(*) FROM yellow_trips")
        url = start_straum(password, network)
        try:
            async with aiohttp.ClientSession(url) as http:
                await wait_ready(http)
                print(f"ready: {trips:,} trips, {time.perf_counter() - started:.0f} s")
                await time_boroughs(pg, http)
                await insert_and_delete(pg, http)
                await move_airports(pg, http)
        finally:
            stop_straum()
        await slot_gone(pg)
    finally:
        await pg.close()
    return 0


def container(password: str, network: Network) -> PostgresContainer:
    # wal_level on the command line, so the plan's ALTER SYSTEM needs no
    # restart. The rest of the server stays at its defaults.
    return (
        PostgresContainer(
            "postgres:18",
            username="postgres",
            password=password,
            dbname="postgres",
            driver=None,
        )
        .with_command("-c wal_level=logical")
        .with_network(network)
        .with_network_aliases(ALIAS)
    )


def load(postgres: PostgresContainer) -> None:
    loader = subprocess.Popen(
        [ROOT / "bench" / "load_taxi_trips.py", "--months", "1", "--data", DATA],
        stdout=subprocess.PIPE,
    )
    stdout = loader.stdout
    if stdout is None:
        raise SystemExit("demo: loader has no stdout")
    container_id = postgres.get_wrapped_container().id
    if not container_id:
        raise SystemExit("demo: PostgreSQL container has no id")
    with stdout:
        docker(
            "exec",
            "-i",
            container_id,
            "psql",
            "-U",
            "postgres",
            "-q",
            stdin=stdout,
            check=True,
        )
    if loader.wait():
        raise SystemExit("demo: load_taxi_trips.py failed")


async def apply_plan(pg: asyncpg.Connection, password: str) -> None:
    sql = straum_run(password, ("--rm",), ("plan", "/spec/trips.sql"))
    for line in sql.splitlines():
        if line and not line.startswith("--"):
            await pg.execute(line)


def start_straum(password: str, network: str) -> str:
    log("starting Straum")
    straum_run(
        password,
        ("-d", "--name", CONTAINER, "--network", network, "-p", "127.0.0.1::8080"),
        ("run", "/spec/trips.sql"),
    )
    return f"http://127.0.0.1:{published_port()}"


def published_port() -> int:
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        out = docker("port", CONTAINER, "8080", capture=True)
        if out.returncode == 0 and out.stdout.strip():
            return int(out.stdout.strip().rsplit(":", 1)[1])
        time.sleep(0.2)
    raise SystemExit(f"demo: Straum published no port\n{logs()}")


def stop_straum() -> None:
    docker("rm", "-f", CONTAINER, capture=True)


async def wait_ready(http: aiohttp.ClientSession) -> None:
    try:
        async with asyncio.timeout(READY_S):
            while True:
                try:
                    async with http.get("/readyz") as response:
                        if response.status == 200:
                            return
                except aiohttp.ClientError:
                    if not running():
                        raise SystemExit(f"demo: Straum stopped\n{logs()}") from None
                await asyncio.sleep(0.05)
    except TimeoutError:
        raise SystemExit(f"demo: Straum did not get ready\n{logs()}") from None


def running() -> bool:
    out = docker("inspect", "-f", "{{.State.Running}}", CONTAINER, capture=True)
    return out.stdout.strip() == "true"


def logs() -> str:
    out = docker("logs", CONTAINER, capture=True)
    return out.stdout + out.stderr


async def time_boroughs(pg: asyncpg.Connection, http: aiohttp.ClientSession) -> None:
    body = ask_body(BY_BOROUGH, MONTH)

    async def from_postgres() -> None:
        await pg.fetch(BY_BOROUGH_SQL, *MONTH)

    async def from_straum() -> None:
        await ask(http, body)

    expected = tally(await pg.fetch(BY_BOROUGH_SQL, *MONTH))
    got = tally(await ask(http, body))
    if got != expected:
        raise SystemExit(f"demo: boroughs differ: Straum {got}, PostgreSQL {expected}")
    pg_time = await median(from_postgres)
    straum_time = await median(from_straum)
    print(
        "trips by pickup borough, January: "
        f"PostgreSQL {duration(pg_time)}, Straum {duration(straum_time)}"
    )


async def insert_and_delete(
    pg: asyncpg.Connection, http: aiohttp.ClientSession
) -> None:
    borough = await pg.fetchval(
        "SELECT borough FROM taxi_zones WHERE location_id = $1", NEWARK
    )
    if not isinstance(borough, str):
        raise SystemExit(f"demo: zone {NEWARK} borough was {borough!r}")
    body = ask_body(ONE_BOROUGH, [borough, *MONTH])
    before = (await ask(http, body))[0]["trips"]
    trip_id = await pg.fetchval(
        """
        INSERT INTO yellow_trips (
            vendor_id, pickup_at, pu_location_id, do_location_id,
            payment_type, fare_amount, tip_amount, trip_distance
        ) VALUES (2, '2024-01-17 12:00:00-05', $1, $2, 1, 9999.99, 0, 17.50)
        RETURNING id
        """,
        NEWARK,
        JFK,
    )
    await until(http, body, before + 1, borough, "insert")
    print(f"insert: {borough} trips {before:,} -> {before + 1:,}")
    status = await pg.execute("DELETE FROM yellow_trips WHERE id = $1", trip_id)
    if status != "DELETE 1":
        raise SystemExit(f"demo: delete removed {status}, not 1 row")
    await until(http, body, before, borough, "delete")
    print(f"delete: {borough} trips {before + 1:,} -> {before:,}, 1 row removed")


async def move_airports(pg: asyncpg.Connection, http: aiohttp.ClientSession) -> None:
    before = tally(await ask(http, ask_body(BY_BOROUGH, MONTH)))
    moved = await pg.fetchval(AIRPORT_TRIPS_SQL, *MONTH)
    if not isinstance(moved, int) or moved == 0:
        raise SystemExit(f"demo: zones {JFK} and {LAGUARDIA} have no January trips")
    queens = before["Queens"]
    if not isinstance(queens, int) or queens < moved:
        raise SystemExit(f"demo: Queens has {queens!r} trips, zones hold {moved}")
    # JFK and LaGuardia, both in Queens. One update, every past trip moves.
    await pg.execute(
        "UPDATE taxi_zones SET borough = 'Airports' "
        "WHERE location_id IN ($1, $2)",
        JFK,
        LAGUARDIA,
    )
    await until_move(http, moved, queens - moved)
    print(f"borough: Queens {queens:,} -> {queens - moved:,}, Airports {moved:,}")


async def until(
    http: aiohttp.ClientSession,
    body: bytes,
    want: int,
    borough: str,
    what: str,
) -> None:
    seen: object = None
    try:
        async with asyncio.timeout(APPLY_S):
            while True:
                seen = (await ask(http, body))[0]["trips"]
                if seen == want:
                    return
                await asyncio.sleep(0.05)
    except TimeoutError:
        raise SystemExit(
            f"demo: {what}: {borough} trips stayed {seen}, wanted {want}"
        ) from None


async def until_move(http: aiohttp.ClientSession, airports: int, queens: int) -> None:
    seen: Counts = {}
    body = ask_body(BY_BOROUGH, MONTH)
    try:
        async with asyncio.timeout(APPLY_S):
            while True:
                seen = tally(await ask(http, body))
                if seen.get("Airports") == airports and seen.get("Queens") == queens:
                    return
                await asyncio.sleep(0.05)
    except TimeoutError:
        raise SystemExit(
            f"demo: borough move: Airports {seen.get('Airports')}, "
            f"Queens {seen.get('Queens')}; wanted {airports} and {queens}"
        ) from None


async def slot_gone(pg: asyncpg.Connection) -> None:
    held: object = None
    try:
        async with asyncio.timeout(SLOT_S):
            while True:
                held = await pg.fetchval(
                    "SELECT count(*) FROM pg_replication_slots "
                    "WHERE slot_name LIKE 'straum%'"
                )
                if held == 0:
                    print("slot: gone")
                    return
                await asyncio.sleep(0.2)
    except TimeoutError:
        raise SystemExit(f"demo: {held} slots named straum still held") from None


async def ask(http: aiohttp.ClientSession, body: bytes) -> list[dict[str, Any]]:
    async with http.post(
        "/ask", data=body, headers={"Content-Type": "application/json"}
    ) as response:
        answer = orjson.loads(await response.read())
    if response.status != 200:
        raise SystemExit(f"demo: Straum answered {response.status}: {answer}")
    rows = answer["rows"]
    if not isinstance(rows, list):
        raise SystemExit(f"demo: rows was {rows!r}")
    return rows


def ask_body(query: str, parameters: Sequence[object]) -> bytes:
    return orjson.dumps({"query": query, "parameters": parameters})


def tally(rows: Rows) -> Counts:
    return {row["borough"]: row["trips"] for row in rows}


async def median(run: Callable[[], Awaitable[object]]) -> float:
    times: list[float] = []
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


def straum_run(
    password: str, flags: Sequence[str], command: Sequence[str]
) -> str:
    completed = docker(
        "run",
        *flags,
        "-e",
        "STRAUM_DSN",
        "-v",
        SPEC_IN_IMAGE,
        IMAGE,
        *command,
        env=os.environ | {"STRAUM_DSN": dsn(password)},
        capture=True,
        check=True,
    )
    return completed.stdout


def dsn(password: str) -> str:
    return f"postgres://straum:{password}@{ALIAS}:5432/postgres?sslmode=disable"


def docker(
    *args: str,
    check: bool = False,
    capture: bool = False,
    env: Mapping[str, str] | None = None,
    stdin: IO[bytes] | None = None,
) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["docker", *args],
        check=check,
        text=True,
        capture_output=capture,
        env=env,
        stdin=stdin,
    )


def log(message: str) -> None:
    print(f"demo: {message}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    sys.exit(main())
