# Feeding Straum from Parquet

`straum run --feed SOCKET` takes its tables from a feed in place of
PostgreSQL: a Unix socket a second program, a feeder, connects to and
gives the rows through. `straum-parquet` is the feeder in the
repository; it reads Parquet files.

A feed is for benchmarks and for trying a spec: Straum is ready in
seconds, with no PostgreSQL to set up. It carries loads only. Nothing
changes after the last table, so a fed Straum answers the same until
it stops. To follow tables as they change, [set up
PostgreSQL](postgresql.md).

## Running it

```sh
straum run --spec trips.sql --feed /tmp/straum.feed &
straum-parquet --manifest data/taxi.manifest --feed /tmp/straum.feed
```

```text
straum: waiting for a feeder on /tmp/straum.feed
straum: listening on 0.0.0.0:8080
straum-parquet: fed 265 rows of public.taxi_zones
straum-parquet: fed 2964624 rows of public.yellow_trips
straum-parquet: Straum has every table
straum: loaded every table from the feed
```

Straum asks the feeder for each table of its [spec](spec.md), dims
first, and only for the columns the spec reads. A table two views read
is fed once for each. `straum-parquet` exits with 0 once Straum has
every table, and [asks](asks.md), `straum status`, `straum views` and
`straum shell` work as they do over PostgreSQL.

The releases and the image hold `straum` alone. Build the feeder from
the repository:

```sh
cargo build --release -p straum-parquet
```

`bench/taxi.py --feed` does all of this for the
[benchmark](benchmark.md#without-postgresql)'s taxi trips.

## The manifest

The manifest says which files hold which table and how their columns
read. A line each; `#` starts a comment:

```text
# The taxi trips, as the TLC publishes them.
table public.taxi_zones
file taxi_zones.parquet
column location_id int from LocationID
column borough text from Borough

table public.yellow_trips
file yellow_tripdata_2024-01.parquet
file yellow_tripdata_2024-02.parquet
column pickup_at timestamptz from tpep_pickup_datetime in America/New_York
column pu_location_id int from PULocationID
column payment_type int from payment_type
column fare_amount numeric(12,2) from fare_amount
```

| Line | Meaning |
| --- | --- |
| `table SCHEMA.TABLE` | Opens a table, named as the spec names it. |
| `file PATH` | A file of the table, found from the manifest's directory. One or more; their rows are fed in this order. |
| `column NAME TYPE from FILE_COLUMN` | A column of the table: its name in the spec, its type, and the file's own name for it, which may not hold a space. |
| `... in ZONE` | After a `timestamptz` column: the IANA name of the zone whose clock the file's column shows. |

The manifest lists every column Straum may ask for; a file may hold
more. The types are the ones Straum [reads from
PostgreSQL](postgresql.md), each with one spelling: `int`,
`numeric(p,s)` with `p` at most 18 and no space, `bool`, `text`,
`timestamptz` and `timestamp`.

## How a column reads

A Parquet file says how a value is stored, not what PostgreSQL would
hold. `straum-parquet` reads each value to what Straum would get had
the file been copied into a table of the manifest's types:

| Type | Stored as | Read |
| --- | --- | --- |
| `int` | `INT32`, `INT64` | As it is. |
| `numeric(p,s)` | `INT32`, `INT64` | The integer at scale `s`. |
| `numeric(p,s)` | `DOUBLE` | The shortest decimal that is the double, rounded to scale `s`, half away from zero, as PostgreSQL rounds a numeric on the way in: 1.005 is 1.01. `NaN` and the infinities stop the feed. |
| `bool` | `BOOLEAN` | As it is. |
| `text` | `BYTE_ARRAY` | Its UTF-8. |
| `timestamptz` | `INT64`, a timestamp in UTC | The instant, rounded down to the second. |
| `timestamptz` ... `in ZONE` | `INT64`, a timestamp with no zone | The instant the zone's clock showed it. |
| `timestamp` | `INT64`, a timestamp with no zone | Its [wall clock](spec.md), rounded down to the second. |

A timestamp with no zone is a clock reading, as the TLC's pickup times
are, and a `timestamptz` is an instant, so the manifest must say whose
clock it is. An hour the clock skipped reads as if it had not been set
forward yet, and an hour it showed twice is its second showing. That
is PostgreSQL's reading of both, so buckets hold the trips they hold
when PostgreSQL loads the same times.

A null is a null. A column inside a group or a list is no column of a
table. Files compressed with Snappy or Zstandard read, and
uncompressed ones.

A column that can't read as its type, or a value that is no value of
it, stops the feed, and both ends say the same:

```text
straum-parquet: data/trips.parquet: row 17: can't read NaN as a numeric of scale 2 in column fare_amount
```

```text
$ curl localhost:8080/readyz
not ready: the feeder: data/trips.parquet: row 17: can't read NaN as a numeric of scale 2 in column fare_amount
```

## What a feed is not

- **Not followed.** No change comes after the load. `straum status`
  shows `applied: 0/0` and no lag, and stays so.
- **Not guarded.** The socket has no authentication. Whoever may
  connect to it gives Straum every row it holds, so Straum makes its
  file its owner's alone, and a feed is never TCP.
- **Not restarted.** After a failure Straum waits for a feeder to
  connect again; run `straum-parquet` once more. `/readyz` says why
  until then.
- **Not checked like PostgreSQL.** The [setup
  check](postgresql.md#what-straum-checks-at-start) reads PostgreSQL's
  catalog. Of a feed Straum checks that the time column has the type
  of its `date_bin` origin; any other column of the wrong type stops
  the load at its first row.

A socket file left by a Straum that has stopped is replaced at the
next start. A path that holds anything else is left alone, and Straum
stops.

## Another feeder

A feeder is any program that speaks the feed's frames; `straum-feed`
has both ends in Rust (`Feed`, `Feeder`). A frame is a tag byte, the
payload's length as four bytes, and the payload, at most 64 MiB.
Numbers are big-endian, and a text is its length as four bytes, then
its UTF-8.

| Tag | From | Frame | Payload |
| ---: | --- | --- | --- |
| 1 | Feeder | Hello, once, first | The bytes `straum feed 1`. |
| 2 | Straum | Load | Three or more texts: the schema, the table, then each column asked for. |
| 3 | Feeder | Types | A byte per column asked for: 0 integer, 1 numeric and its scale as four bytes, 2 boolean, 3 text, 4 `timestamptz`, 5 `timestamp`. |
| 4 | Feeder | Rows | Whole rows end to end, a value per column asked for: byte 0 null, 1 false, 2 true, 3 and an integer of eight bytes, 4 and a text. |
| 5 | Feeder | End | Nothing: the table has no more rows. |
| 6 | Straum | Done | Nothing: Straum has every table, and closes. |
| 7 | Either | Failed | Why, as UTF-8. The sender closes. |

The feeder answers each Load with Types, any number of Rows, and End.
A numeric is its integer in units of `10^-scale`, and a time is Unix
seconds. About 8,000 rows a frame load fastest: Straum starts a thread
per wider bucket width for each frame it adds.
