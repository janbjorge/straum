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
"""What one transaction costs Straum, on the taxi month.

Straum holds a transaction until its commit and applies it whole under its
write lock, so every ask waits for it, and the rows wait in memory. This
script loads the month as bench.py does, then changes N rows in one
transaction for several N while asking Straum every few milliseconds. For
each N it reports the time from the commit until Straum had applied it, the
longest ask seen meanwhile, which is the lock hold as a client feels it, and
how much Straum's resident set grew. --max-transaction-rows takes its
default from this table.

Needs Docker, cargo and uv, and the data bench.py fetches.
"""

import asyncio
import secrets
import statistics
import subprocess
import time

import aiohttp
import asyncpg
import orjson
import psutil
import tabulate
from testcontainers.community.postgres import PostgresContainer

from bench import DATA, LOAD, SPEC, build, duration, fetch, log, serve

SIZES = [1_000, 10_000, 100_000, 1_000_000]

# One day of hour buckets: the ask a dashboard refreshes.
ASK = orjson.dumps(
    {
        "query": """SELECT bucket, trips FROM trips
                     WHERE bucket >= $1 AND bucket < $2 GROUP BY bucket""",
        "parameters": ["2024-01-17T00:00:00-05:00", "2024-01-18T00:00:00-05:00"],
    }
)


def main() -> int:
    fetch()
    straum = build()
    plan = subprocess.check_output([straum, "plan", "--spec", SPEC], text=True)
    password = secrets.token_hex(16)
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
        return asyncio.run(measure(postgres, straum, plan, password))


async def measure(
    postgres: PostgresContainer, straum: str, plan: str, password: str
) -> int:
    pg = await asyncpg.connect(
        postgres.get_connection_url(),
        server_settings={"timezone": "America/New_York"},
    )
    log("loading the month")
    await pg.execute(LOAD)
    await pg.execute("VACUUM ANALYZE")
    for line in plan.splitlines():
        if line and not line.startswith("--"):
            await pg.execute(line)
    await pg.execute(f"ALTER ROLE straum PASSWORD '{password}'")
    host, port = postgres.get_container_host_ip(), postgres.get_exposed_port(5432)
    dsn = f"postgres://straum:{password}@{host}:{port}/postgres?sslmode=disable"
    # The cap is out of the way: this measures what it would prevent.
    cap = str(10 * max(SIZES))
    async with serve(straum, dsn, "--max-transaction-rows", cap) as (http, loaded, rss):
        log(f"straum ready in {loaded:.1f} s; asking while idle")
        idle = statistics.median(await asks(http, lambda: True, 1.0))
        process = straum_process()
        rows = []
        for n in SIZES:
            log(f"updating {n:,} rows in one transaction")
            applied, stall, peak = await transaction(pg, http, process, n)
            grew = max(0, peak - rss)
            rows.append((f"{n:,}", duration(applied), duration(stall), f"{grew / 2**20:,.0f} MiB"))
    await pg.close()
    print(f"An idle ask takes {duration(idle)}; Straum held {rss / 2**20:,.0f} MiB after the load.")
    print()
    print(
        tabulate.tabulate(
            rows,
            headers=["Rows changed", "Commit to applied", "Longest ask meanwhile", "Memory grown"],
            tablefmt="pipe",
            colalign=["right", "right", "right", "right"],
            disable_numparse=True,
        )
    )
    return 0


def straum_process() -> psutil.Process:
    """The `straum run` this script started, found among its children."""
    for child in psutil.Process().children(recursive=True):
        if "straum" in child.name():
            return child
    raise SystemExit("transaction: no straum process found")


async def asks(http: aiohttp.ClientSession, done, at_least: float) -> list[float]:
    """Asks back to back until `done()` holds and `at_least` seconds have
    passed; each ask's seconds."""
    times = []
    end = time.perf_counter() + at_least
    while not done() or time.perf_counter() < end:
        start = time.perf_counter()
        async with http.post("/ask", data=ASK) as response:
            await response.read()
            if response.status != 200:
                raise SystemExit(f"transaction: straum answered {response.status}")
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
    asker = asyncio.create_task(asks(http, lambda: finished, 0))
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


if __name__ == "__main__":
    raise SystemExit(main())
