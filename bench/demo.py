#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = ["aiohttp", "asyncpg", "orjson"]
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
import pathlib
import secrets
import statistics
import subprocess
import sys
import time
from collections.abc import Awaitable, Callable, Mapping, Sequence
from datetime import datetime
from typing import Any
from zoneinfo import ZoneInfo

import aiohttp
import asyncpg
import orjson
from stack import POSTGRES, POSTGRES_PORT, Stack, postgres_service

type Rows = Sequence[Mapping[str, Any]]
type Counts = dict[str | None, int]

ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = ROOT / "bench" / "demo.sql"
DATA = ROOT / "bench" / "data"
IMAGE = "ghcr.io/janbjorge/straum:0.8.0"
STRAUM = "straum"
STRAUM_PORT = 8080
SPEC_IN_IMAGE = "/spec/trips.sql"
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
    password = secrets.token_hex(16)
    services = {POSTGRES: postgres(password), STRAUM: straum(password)}
    with Stack("straum-demo", services) as stack:
        log(f"pulling {IMAGE}")
        stack.pull(STRAUM)
        log(f"starting PostgreSQL in stack {stack.name}")
        stack.up(POSTGRES)
        return asyncio.run(run(stack, password))


async def run(stack: Stack, password: str) -> int:
    started = time.perf_counter()
    port = stack.port(POSTGRES, POSTGRES_PORT)
    pg = await asyncpg.connect(
        f"postgres://postgres:{password}@127.0.0.1:{port}/postgres",
        server_settings=NEW_YORK,
    )
    try:
        log("loading January")
        load(stack)
        await pg.execute("ANALYZE")
        await apply_plan(pg, stack)
        trips = await pg.fetchval("SELECT count(*) FROM yellow_trips")
        log("starting Straum")
        stack.up(STRAUM)
        url = f"http://127.0.0.1:{stack.port(STRAUM, STRAUM_PORT)}"
        async with aiohttp.ClientSession(url) as http:
            await wait_ready(stack, http)
            print(f"ready: {trips:,} trips, {time.perf_counter() - started:.0f} s")
            await time_boroughs(pg, http)
            await insert_and_delete(pg, http)
            await move_airports(pg, http)
        stack.stop(STRAUM)
        await slot_gone(pg)
    finally:
        await pg.close()
    return 0


def postgres(password: str) -> dict:
    # wal_level on the command line, so the plan's ALTER SYSTEM needs no
    # restart. The rest of the server stays at its defaults.
    return postgres_service("postgres:18", password, {"wal_level": "logical"})


def straum(password: str) -> dict:
    return {
        "image": IMAGE,
        "command": ["run", SPEC_IN_IMAGE],
        "environment": {"STRAUM_DSN": dsn(password)},
        "volumes": [f"{SPEC}:{SPEC_IN_IMAGE}:ro"],
        "ports": [{"target": STRAUM_PORT, "host_ip": "127.0.0.1"}],
    }


def load(stack: Stack) -> None:
    loader = subprocess.Popen(
        [ROOT / "bench" / "load_taxi_trips.py", "--months", "1", "--data", DATA],
        stdout=subprocess.PIPE,
    )
    stdout = loader.stdout
    if stdout is None:
        raise SystemExit("demo: loader has no stdout")
    with stdout:
        psql = stack.exec_args(POSTGRES, "psql", "-U", "postgres", "-q")
        subprocess.run(psql, stdin=stdout, check=True)
    if loader.wait():
        raise SystemExit("demo: load_taxi_trips.py failed")


async def apply_plan(pg: asyncpg.Connection, stack: Stack) -> None:
    sql = stack.run(STRAUM, "plan", SPEC_IN_IMAGE)
    for line in sql.splitlines():
        if line and not line.startswith("--"):
            await pg.execute(line)


async def wait_ready(stack: Stack, http: aiohttp.ClientSession) -> None:
    try:
        async with asyncio.timeout(READY_S):
            while True:
                try:
                    async with http.get("/readyz") as response:
                        if response.status == 200:
                            return
                except aiohttp.ClientError:
                    if not stack.running(STRAUM):
                        logs = stack.logs(STRAUM)
                        raise SystemExit(f"demo: Straum stopped\n{logs}") from None
                await asyncio.sleep(0.05)
    except TimeoutError:
        logs = stack.logs(STRAUM)
        raise SystemExit(f"demo: Straum did not get ready\n{logs}") from None


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
        f"PostgreSQL {duration(pg_time)}, Straum {duration(straum_time)}, "
        f"{pg_time / straum_time:,.0f}x"
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
        "UPDATE taxi_zones SET borough = 'Airports' WHERE location_id IN ($1, $2)",
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
        reply = await response.read()
    # A refusal may be no JSON, as from a proxy: its words, not a decode
    # error.
    if response.status != 200:
        said = reply.decode(errors="replace")
        raise SystemExit(f"demo: Straum answered {response.status}: {said}")
    rows = orjson.loads(reply)["rows"]
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


def dsn(password: str) -> str:
    return f"postgres://straum:{password}@{POSTGRES}:{POSTGRES_PORT}/postgres?sslmode=disable"


def log(message: str) -> None:
    print(f"demo: {message}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    sys.exit(main())
