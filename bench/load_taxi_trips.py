#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["asyncpg", "pyarrow"]
#
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""Loads New York taxi trips from the TLC's Parquet files into PostgreSQL.

Fetches the zones and the first N months of 2024 into --data, makes
taxi_zones and yellow_trips, and streams the trips in with COPY, a
connection per month. Nothing is written between Parquet and PostgreSQL.
"""

import argparse
import asyncio
import io
import os
import pathlib
import sys
import urllib.request
from collections.abc import AsyncIterator, Iterator

import asyncpg
import pyarrow
import pyarrow.csv
import pyarrow.parquet

TLC = "https://d37ci6vzurychx.cloudfront.net"
DATA = pathlib.Path(__file__).resolve().parent / "data"

# TLC name -> yellow_trips column.
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

# Money and distance are numeric, so sums are exact. The TLC's times are
# New York wall clock with no offset, so every session reads them there.
NEW_YORK = {"timezone": "America/New_York"}
SCHEMA = """
CREATE TABLE taxi_zones (
    location_id int PRIMARY KEY,
    borough text,
    zone text,
    service_zone text
);
CREATE TABLE yellow_trips (
    id bigint GENERATED ALWAYS AS IDENTITY,
    vendor_id int,
    pickup_at timestamptz,
    pu_location_id int,
    do_location_id int,
    payment_type int,
    fare_amount numeric(12, 2),
    tip_amount numeric(12, 2),
    trip_distance numeric(12, 2)
);
"""
# Built once the rows are in, which is faster than row by row.
INDEXES = [
    "ALTER TABLE yellow_trips ADD PRIMARY KEY (id)",
    "CREATE INDEX ON yellow_trips (pickup_at)",
]
BATCH_ROWS = 65_536


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("dsn", help="the PostgreSQL to load, as a URL")
    parser.add_argument(
        "--months",
        type=int,
        default=1,
        choices=range(1, 13),
        help="months of 2024 from January (default 1)",
    )
    parser.add_argument(
        "--data",
        type=pathlib.Path,
        default=DATA,
        help=f"where the downloads go (default {DATA})",
    )
    args = parser.parse_args()
    months = [f"2024-{month:02}" for month in range(1, args.months + 1)]
    asyncio.run(load(args.dsn, months, args.data))
    return 0


async def load(dsn: str, months: list[str], data: pathlib.Path) -> None:
    zones, trips = fetch(months, data)
    async with asyncpg.create_pool(
        dsn, min_size=1, max_size=len(trips), server_settings=NEW_YORK
    ) as pool:
        await pool.execute(SCHEMA)
        await pool.copy_to_table("taxi_zones", source=zones, format="csv", header=True)
        log(f"loading {len(trips)} months, one connection each")
        await asyncio.gather(*(copy_trips(pool, path) for path in trips))
        log("indexing")
        for index in INDEXES:
            await pool.execute(index)
        log("vacuuming")
        await pool.execute("VACUUM ANALYZE")


async def copy_trips(pool: asyncpg.Pool, path: pathlib.Path) -> None:
    await pool.copy_to_table(
        "yellow_trips",
        source=csv_chunks(path),
        columns=list(COLUMNS.values()),
        format="csv",
    )


async def csv_chunks(path: pathlib.Path) -> AsyncIterator[bytes]:
    """The file's trips as CSV, a batch at a time. Reading and encoding run
    off the event loop, so the months encode in parallel."""
    batches = pyarrow.parquet.ParquetFile(path).iter_batches(
        batch_size=BATCH_ROWS, columns=list(COLUMNS)
    )
    while (chunk := await asyncio.to_thread(next_chunk, batches)) is not None:
        yield chunk


def next_chunk(batches: Iterator[pyarrow.RecordBatch]) -> bytes | None:
    batch = next(batches, None)
    if batch is None:
        return None
    sink = io.BytesIO()
    options = pyarrow.csv.WriteOptions(include_header=False)
    # The file's column order, not COLUMNS', comes out of iter_batches.
    pyarrow.csv.write_csv(batch.select(list(COLUMNS)), sink, options)
    return sink.getvalue()


def fetch(
    months: list[str], data: pathlib.Path
) -> tuple[pathlib.Path, list[pathlib.Path]]:
    data.mkdir(parents=True, exist_ok=True)
    zones = data / "taxi_zone_lookup.csv"
    download(zones, f"{TLC}/misc/taxi_zone_lookup.csv")
    trips = [data / f"yellow_tripdata_{month}.parquet" for month in months]
    for path in trips:
        download(path, f"{TLC}/trip-data/{path.name}")
    return zones, trips


def download(path: pathlib.Path, url: str) -> None:
    if path.exists():
        return
    log(f"fetching {url}")
    # Per process, so two runs fetching at once never take each other's file.
    part = path.with_name(f"{path.name}.{os.getpid()}.part")
    urllib.request.urlretrieve(url, part)
    part.rename(path)


def log(message: str) -> None:
    print(f"load_taxi_trips: {message}", file=sys.stderr, flush=True)


if __name__ == "__main__":
    sys.exit(main())
