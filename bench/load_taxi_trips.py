#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = ["pyarrow"]
#
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""Prints the TLC's New York taxi trips of 2024 as SQL for psql."""

import argparse
import hashlib
import pathlib
import sys
import urllib.request

import pyarrow.csv
import pyarrow.parquet

TLC = "https://d37ci6vzurychx.cloudfront.net"

# Each file's SHA-256 as fetched on 2026-11-17: the trip counts the docs
# give hold only for these bytes, and the TLC may republish a month.
SHA256 = {
    "taxi_zone_lookup.csv": "1a99e105092230f8620f301edcca7f80d3080642ff404d28ed957d3fa222c8ed",
    "yellow_tripdata_2024-01.parquet": "c4d59da7bbc8abaeeeb1727947ee93d9891a71acb42854bd80db1571b2030510",
    "yellow_tripdata_2024-02.parquet": "c76c43c18c6c6664080dd920baab4928988d5786a6b65980792ca7cd796f9f20",
    "yellow_tripdata_2024-03.parquet": "2d4cdc8fb96726cdd3803b13b02d2e61e71d45720aff0ebc693a8bdd1f249823",
    "yellow_tripdata_2024-04.parquet": "ce37f736c7cf9e92164c850ef2b502a8f7bb590fc1804b6edbcb80293da01b89",
    "yellow_tripdata_2024-05.parquet": "1974230be2b5a3ea92e47b5eb77f32dbf1de896d81ad8df4ab6faba52749ad23",
    "yellow_tripdata_2024-06.parquet": "677cf14c8347f745b583f012fbdba072334c6c4efa17bfd6a64369f2ba30329c",
}

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
        check(part, path.name)
        part.rename(path)
    check(path, path.name)
    return path


def check(path: pathlib.Path, name: str) -> None:
    """Fails unless `path` holds the bytes pinned for `name`; a month
    with no pin, past June, is taken as it comes."""
    wanted = SHA256.get(name)
    if wanted is None:
        return
    with path.open("rb") as file:
        got = hashlib.file_digest(file, "sha256").hexdigest()
    if got != wanted:
        raise SystemExit(
            f"{name}: SHA-256 {got}, not the pinned {wanted}; "
            "the TLC changed the file, so the counts in the docs no longer hold"
        )


if __name__ == "__main__":
    main()
