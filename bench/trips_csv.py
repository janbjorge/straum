"""Write the columns taxi.sql reads from a TLC yellow trips file as CSV.

Usage: trips_csv.py IN.parquet OUT.csv
"""

import sys

import pyarrow.csv
import pyarrow.parquet

# TLC name -> load.sql name, in load.sql column order.
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

src, dst = sys.argv[1:]
table = pyarrow.parquet.read_table(src, columns=list(COLUMNS)).select(list(COLUMNS))
pyarrow.csv.write_csv(table.rename_columns(list(COLUMNS.values())), dst)
