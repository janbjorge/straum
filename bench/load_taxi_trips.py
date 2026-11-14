#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["asyncpg", "pyarrow"]
#
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""Loads the TLC's New York taxi trips of 2024 into PostgreSQL."""

import argparse
import asyncio
import io
import pathlib
import urllib.request

import asyncpg
import pyarrow.csv
import pyarrow.parquet

TLC = "https://d37ci6vzurychx.cloudfront.net"

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

SCHEMA = """
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
"""

# The TLC's times are New York wall clock with no offset.
NEW_YORK = {"timezone": "America/New_York"}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("dsn", help="PostgreSQL URL")
    parser.add_argument("--months", type=int, default=1, help="from January")
    parser.add_argument(
        "--data", type=pathlib.Path, default=pathlib.Path("data"), help="downloads"
    )
    args = parser.parse_args()
    asyncio.run(load(args.dsn, args.months, args.data))


async def load(dsn: str, months: int, data: pathlib.Path) -> None:
    data.mkdir(parents=True, exist_ok=True)
    zones = download(data, f"{TLC}/misc/taxi_zone_lookup.csv")
    pg = await asyncpg.connect(dsn, server_settings=NEW_YORK)
    try:
        await pg.execute(SCHEMA)
        await pg.copy_to_table("taxi_zones", source=zones, format="csv", header=True)
        for month in range(1, months + 1):
            name = f"yellow_tripdata_2024-{month:02}.parquet"
            trips = download(data, f"{TLC}/trip-data/{name}")
            await pg.copy_to_table(
                "yellow_trips",
                source=csv(trips),
                columns=list(COLUMNS.values()),
                format="csv",
            )
    finally:
        await pg.close()


async def csv(path: pathlib.Path):
    batches = pyarrow.parquet.ParquetFile(path).iter_batches(columns=list(COLUMNS))
    for batch in batches:
        sink = io.BytesIO()
        options = pyarrow.csv.WriteOptions(include_header=False)
        # iter_batches keeps the file's column order.
        pyarrow.csv.write_csv(batch.select(list(COLUMNS)), sink, options)
        yield sink.getvalue()


def download(data: pathlib.Path, url: str) -> pathlib.Path:
    path = data / url.rsplit("/", 1)[1]
    if not path.exists():
        print(f"fetching {url}")
        part = path.with_suffix(".part")
        urllib.request.urlretrieve(url, part)
        part.rename(path)
    return path


if __name__ == "__main__":
    main()
