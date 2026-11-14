#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.12"
# dependencies = ["pyarrow"]
#
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""Prints the TLC's New York taxi trips of 2024 as SQL for psql."""

import argparse
import pathlib
import sys
import urllib.request

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

# The TLC's times are New York wall clock with no offset.
SCHEMA = """\\set ON_ERROR_STOP on
SET timezone = 'America/New_York';
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
COPY taxi_zones FROM stdin WITH (FORMAT csv, HEADER);
"""
TRIPS = (
    f"COPY yellow_trips ({', '.join(COLUMNS.values())}) FROM stdin WITH (FORMAT csv);\n"
)
END = b"\\.\n"


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--months", type=int, default=1, help="from January")
    parser.add_argument(
        "--data", type=pathlib.Path, default=pathlib.Path("data"), help="downloads"
    )
    args = parser.parse_args()
    args.data.mkdir(parents=True, exist_ok=True)
    zones = download(args.data, f"{TLC}/misc/taxi_zone_lookup.csv")
    out = sys.stdout.buffer
    out.write(SCHEMA.encode())
    out.write(zones.read_bytes())
    out.write(END)
    options = pyarrow.csv.WriteOptions(include_header=False)
    columns = list(COLUMNS)
    for month in range(1, args.months + 1):
        name = f"yellow_tripdata_2024-{month:02}.parquet"
        trips = pyarrow.parquet.ParquetFile(
            download(args.data, f"{TLC}/trip-data/{name}")
        )
        out.write(TRIPS.encode())
        # iter_batches keeps the file's column order.
        for batch in trips.iter_batches(columns=columns):
            pyarrow.csv.write_csv(batch.select(columns), out, options)
        out.write(END)
    out.flush()


def download(data: pathlib.Path, url: str) -> pathlib.Path:
    path = data / url.rsplit("/", 1)[1]
    if not path.exists():
        print(f"fetching {url}", file=sys.stderr)
        part = path.with_suffix(".part")
        urllib.request.urlretrieve(url, part)
        part.rename(path)
    return path


if __name__ == "__main__":
    main()
