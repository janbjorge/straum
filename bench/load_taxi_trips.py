#!/usr/bin/env -S uv run --script
# /// script
# requires-python = "==3.14.*"
# dependencies = ["asyncpg", "pyarrow"]
#
# [tool.uv]
# exclude-newer = "2026-11-01T00:00:00Z"
# ///
"""Prints the TLC's New York taxi trips of 2024 as SQL for psql.

With `--setup`, loads the database the PG* environment names and runs
that SQL after. asyncpg reads those variables. Setup tolerates a role
or publication that already exists, so a second run only re-applies it.
"""

import argparse
import asyncio
import hashlib
import io
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
    parser.add_argument(
        "--setup",
        type=pathlib.Path,
        help="load via the PG* environment, then run this SQL",
    )
    args = parser.parse_args()
    args.data.mkdir(parents=True, exist_ok=True)
    if args.setup is not None:
        load_database(args.data, args.months, args.setup)
        return
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


def load_database(data: pathlib.Path, months: int, setup: pathlib.Path) -> None:
    """Loads the trips, then runs `setup`. The database is the PG* environment."""
    asyncio.run(load(data, months, setup))


async def load(data: pathlib.Path, months: int, setup: pathlib.Path) -> None:
    """Loads the trips through asyncpg, then runs `setup`."""
    import asyncpg

    zones = download(data, f"{TLC}/misc/taxi_zone_lookup.csv")
    files = []
    for month in range(1, months + 1):
        name = f"yellow_tripdata_2024-{month:02}.parquet"
        files.append(download(data, f"{TLC}/trip-data/{name}"))
    # The TLC's times are New York wall clock. The session zone makes
    # the timestamptz copy read them that way. Host, user, password,
    # database and sslmode come from PGHOST and the other PG* variables.
    conn = await asyncpg.connect(server_settings={"timezone": "America/New_York"})
    try:
        present = await conn.fetchval("SELECT to_regclass('public.yellow_trips')")
        if present is None:
            print("loading taxi trips", file=sys.stderr)
            async with conn.transaction():
                await copy_trips(conn, zones, files)
        else:
            print("taxi trips already loaded", file=sys.stderr)
        await apply_setup(conn, setup.read_text())
    finally:
        await conn.close()


async def copy_trips(conn, zones: pathlib.Path, files: list[pathlib.Path]) -> None:
    """Creates the two tables and copies `zones` and each month in `files`."""
    for statement in statements(table_sql()):
        await conn.execute(statement)
    await conn.copy_to_table(
        "taxi_zones",
        source=io.BytesIO(zones.read_bytes()),
        schema_name="public",
        format="csv",
        header=True,
    )
    columns = list(COLUMNS.values())
    names = list(COLUMNS)
    options = pyarrow.csv.WriteOptions(include_header=False)
    for path in files:
        buf = io.BytesIO()
        parquet = pyarrow.parquet.ParquetFile(path)
        for batch in parquet.iter_batches(columns=names):
            pyarrow.csv.write_csv(batch.select(names), buf, options)
        buf.seek(0)
        await conn.copy_to_table(
            "yellow_trips",
            source=buf,
            columns=columns,
            schema_name="public",
            format="csv",
        )


def table_sql() -> str:
    """The `CREATE TABLE` statements `SCHEMA` also sends to psql."""
    lines = []
    for line in SCHEMA.splitlines():
        if line.startswith("\\") or line.startswith("COPY ") or line.startswith("SET "):
            continue
        lines.append(line)
    return "\n".join(lines)


async def apply_setup(conn, sql: str) -> None:
    """Runs `sql`. A role or publication that already exists is left as it is."""
    import asyncpg

    for statement in statements(sql):
        try:
            await conn.execute(statement)
        except asyncpg.exceptions.DuplicateObjectError:
            continue
    print("setup applied", file=sys.stderr)


def statements(sql: str) -> list[str]:
    """Statements of `sql`, comments and blank lines dropped."""
    pending: list[str] = []
    found = []
    for line in sql.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("--"):
            continue
        pending.append(stripped)
        if stripped.endswith(";"):
            found.append(" ".join(pending))
            pending = []
    if pending:
        raise SystemExit("SQL ended inside a statement")
    return found


if __name__ == "__main__":
    main()
