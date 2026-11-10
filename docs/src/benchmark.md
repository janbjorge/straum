# Benchmark

Straum's benchmark asks Straum and PostgreSQL the same questions about
one month of New York taxi trips, checks that the answers are equal, and
times both.

The data is the TLC yellow trips of January 2024, 2,964,624 trips, and
the 265 rows of `taxi_zone_lookup.csv`.

## Running it

```sh
bench/bench.py
```

It needs Docker, a Rust toolchain and [uv](https://docs.astral.sh/uv/),
which fetches the script's Python packages. The first run downloads the
month into `bench/data/`, which git ignores, and turns the Parquet file
into CSV. Then it builds Straum, starts a throwaway `postgres:18` with
testcontainers, loads the tables, runs the SQL `straum plan` prints and
starts `straum run`. Each ask goes to PostgreSQL as SQL over the raw
tables and to Straum as an [ask](asks.md) over HTTP, and a run stops at
the first answer that differs. A run takes under half a minute once the
data is there.

```sh
bench/transaction.py
```

The same setup, then one transaction of 1,000 to 1,000,000 changed rows
while a client asks Straum back to back: what a transaction costs Straum,
under [Transactions](#transactions).

## The view

`bench/trips.sql` is what a dashboard over these trips would declare:
trips, fares and the average fare per hour, payment type and pickup
zone, with day and week buckets for long ranges.

```sql
CREATE VIEW trips WITH (buckets = '1 day, 7 days') AS
SELECT date_bin('1 hour', f.pickup_at, timestamptz 'epoch') AS hour,
       f.payment_type,
       f.pu_location_id,
       pickup.borough,
       count(*) AS trips,
       sum(f.fare_amount) AS fare,
       avg(f.fare_amount) AS avg_fare
FROM public.yellow_trips f
LEFT JOIN public.taxi_zones pickup ON pickup.location_id = f.pu_location_id
GROUP BY 1, 2, 3, 4;
```

Its rollup keeps 173,660 hour cells for the 2,964,624 trips, 17.1 trips
a cell, and 25,739 day and week cells.

## The asks

PostgreSQL gets the query anyone would write over the raw tables. The
session's time zone is New York's, as the TLC's times are.

```sql
-- Trips
SELECT count(*) FROM yellow_trips;

-- Card trips
SELECT count(*) FROM yellow_trips WHERE payment_type = 1;

-- Trips by pickup borough
SELECT z.borough, count(*)
  FROM yellow_trips t
  LEFT JOIN taxi_zones z ON z.location_id = t.pu_location_id
 GROUP BY z.borough;

-- Fares by payment type
SELECT payment_type, count(*), sum(fare_amount), avg(fare_amount)
  FROM yellow_trips
 GROUP BY payment_type;

-- Manhattan pickups, one week
SELECT count(*)
  FROM yellow_trips t
  JOIN taxi_zones z ON z.location_id = t.pu_location_id
 WHERE z.borough = 'Manhattan'
   AND t.pickup_at >= '2024-01-08' AND t.pickup_at < '2024-01-15';

-- Trips per hour, one day
SELECT date_trunc('hour', pickup_at), count(*)
  FROM yellow_trips
 WHERE pickup_at >= '2024-01-17' AND pickup_at < '2024-01-18'
 GROUP BY 1;
```

Straum gets the same questions over the view, with every value a
parameter:

```sql
-- Trips
SELECT trips FROM trips WHERE bucket >= $1 AND bucket < $2

-- Card trips
SELECT trips FROM trips
 WHERE payment_type = $1 AND bucket >= $2 AND bucket < $3

-- Trips by pickup borough
SELECT pickup.borough, trips FROM trips
 WHERE bucket >= $1 AND bucket < $2
 GROUP BY pickup.borough

-- Fares by payment type
SELECT payment_type, trips, fare, avg_fare FROM trips
 WHERE bucket >= $1 AND bucket < $2
 GROUP BY payment_type

-- Manhattan pickups, one week
SELECT trips FROM trips
 WHERE pickup.borough = $1 AND bucket >= $2 AND bucket < $3

-- Trips per hour, one day
SELECT bucket, trips FROM trips
 WHERE bucket >= $1 AND bucket < $2
 GROUP BY bucket
```

An ask always names its range. Where the SQL has none, the range is
2000 to 2030, which holds every trip. Trips by pickup borough goes as:

```json
{"query": "SELECT pickup.borough, trips FROM trips WHERE bucket >= $1 AND bucket < $2 GROUP BY pickup.borough",
 "parameters": ["2000-01-01T00:00:00Z", "2030-01-01T00:00:00Z"]}
```

## Results

One run on an Apple M2 Pro with 16 GB, on 2026-11-10: PostgreSQL 18.6 in
Docker Desktop with 10 CPUs, and Straum's release build on the same
machine. Each time is the median over at least a second, from one Python
client: asyncpg for PostgreSQL, aiohttp and orjson for Straum. Rows are
the answer's.

| Ask | Rows | PostgreSQL | Straum | Faster |
| --- | ---: | ---: | ---: | ---: |
| Trips | 1 | 25.8 ms | 114 µs | 227x |
| Card trips | 1 | 46.8 ms | 122 µs | 385x |
| Trips by pickup borough | 8 | 122.5 ms | 139 µs | 883x |
| Fares by payment type | 5 | 98.5 ms | 147 µs | 672x |
| Manhattan pickups, one week | 1 | 58.9 ms | 140 µs | 422x |
| Trips per hour, one day | 24 | 15.7 ms | 137 µs | 115x |

Straum was ready 0.8 s after it started and held 38 MiB. In PostgreSQL
the table and its indexes take 363 MiB. Two more runs right before
stayed within 11% of these times, but for PostgreSQL's week ask, which
took 72.0 ms once.

How to read the table:

- Both sides answer the same client over their own protocol, with a
  warm cache. PostgreSQL runs with its defaults, two parallel workers
  per query, except `shared_buffers`: 2 GB, so the table stays in its
  cache. `yellow_trips` has an index on `pickup_at`, which the week and
  day asks use. More parallel workers would shorten its scans of the
  whole table.
- PostgreSQL's time follows the rows it reads: every trip for the first
  four asks, a week's or a day's for the last two. Straum's hardly
  changes from ask to ask: most of it is the HTTP round trip, reading
  the query and JSON.
- The gap is smallest where PostgreSQL reads least: one day, through
  the index.
- Straum answers only what the view keeps: these keys, on hour edges.
  PostgreSQL answers any query over the raw rows. Straum keeps its
  rollup current as trips change; a rollup table in PostgreSQL would
  have to be rebuilt or maintained.

## Transactions

Straum holds a transaction until its commit and applies it whole under
its write lock, so its rows wait in memory and every ask waits for the
apply. `bench/transaction.py` updates the fare of the first N trips in
one transaction while asking for one day of hour buckets as fast as the
answers come, and measures the time from the commit until Straum has
applied it, the longest ask seen meanwhile, which is the lock hold as a
client feels it, and how much Straum's resident set grew. One run on
2026-11-10, the machine, build and PostgreSQL of the [Results](#results);
Straum held 38 MiB after the load and an idle ask took 136 µs:

| Rows changed | Commit to applied | Longest ask meanwhile | Memory grown |
| ---: | ---: | ---: | ---: |
| 1,000 | 5.1 ms | 331 µs | 1 MiB |
| 10,000 | 43.3 ms | 2.4 ms | 3 MiB |
| 100,000 | 356 ms | 22.5 ms | 29 MiB |
| 1,000,000 | 3.9 s | 231 ms | 262 MiB |

Most of the time goes to PostgreSQL decoding the transaction and Straum
reading it, outside the lock; the lock itself is held for about 0.2 ms
per 1,000 rows. Memory is what grows: the rows of a transaction wait in
Straum until its commit, about 270 bytes each, so a million-row
transaction holds seven times the whole taxi rollup. That sets the
default of `--max-transaction-rows` at 100,000: a transaction at the cap
holds the lock for 20 ms and 29 MiB, and anything larger is cheaper as
a [reload](how-it-works.md#applying-a-commit), which takes the rows in
through the bulk `COPY` instead.

## The whole view, end to end

```sh
cargo test -p straum-pg --release taxi_month -- --ignored --nocapture
```

It needs the data `bench/bench.py` fetches. It loads the month into
`postgres:18` with the full view of
`crates/straum-notation/tests/taxi.sql`, which the [spec](spec.md) page
walks through, has Straum load and follow it, and compares asks with the
view body in PostgreSQL after the load, after 202 streamed commits and
after an update. One run on 2026-11-09, release build, Docker on the
same laptop:

| Step or ask | Groups | Straum | PostgreSQL |
| --- | ---: | ---: | ---: |
| Load | | 2.0 s | |
| 202 streamed commits | | 4.9 s | |
| Every key | 2,322,783 to 2,329,715 | 2.4 to 3.0 s | 6.2 to 6.4 s |
| By pickup borough | 8 | 6.6 to 8.8 ms | |

The ask over every key reads every 15 minute cell and answers nearly as
many groups, so it is only 2.1 to 2.6 times faster than the view in
PostgreSQL. The test's reference for the ask by borough groups that
whole view again, which nobody would write, so the table leaves its time
out; the asks above time a query by borough.

taxi.sql keys trips by payment type, vendor, pickup zone and dropoff
zone. Cells per width, each a `count(DISTINCT ...)` of that key and the
bucket over the month:

| Rollup | Cells | Trips a cell |
| --- | ---: | ---: |
| Raw trips | 2,964,624 | 1.0 |
| 15 minutes | 2,322,783 | 1.3 |
| 1 hour | 1,685,731 | 1.8 |
| 15 minutes, no dropoff zone | 568,612 | 5.2 |
| 1 day | 519,726 | 5.7 |
| 7 days | 199,398 | 14.9 |

Pickup and dropoff zone together are nearly unique per trip, so this
spec collapses little at 15 minutes. Without the dropoff zone it
collapses 5.2 times. Memory follows cells, so the key is the lever on
memory.

## Cells on pio-shaped tables

The same lever on a warehouse shape: tasks keyed by organization, task
group, storage location and whether the task is live, with the group's
category and the location's type as attributes, over hour buckets with a
day width beside them. The tables are generated locally in the
distributions of a production database (13 million tasks, 5.7 million
task groups, 860,000 storage locations, a task group holding one to
three tasks) and are not in the repository; the run below is three
tenths of that size, on 2026-11-10, the machine and build of the
[Results](#results). Both views answered every ask as PostgreSQL did,
before and after streamed changes.

| View | Keys | Cells, 1 hour | Cells, 1 day | Resident |
| --- | --- | ---: | ---: | ---: |
| Late | organization, task group, storage location, live | 3,669,223 | 3,669,206 | 1,478 MiB |
| Bound at ingest | organization, live, category, location type | 932,532 | 127,014 | 498 MiB |

3,932,790 tasks, 1,706,235 task groups, 253,555 storage locations. The
late view keeps 0.93 cells a task, since a task group holds one task or
two and every task has its own hour; at the production size that is
about 12 million cells a width and 5.8 GiB. [Binding the
attributes at ingest](spec.md#binding-attributes-at-ingest) keys the
cells by the category's three values and the location type's four
instead, so the day width collapses 29 times and memory falls to a
third; what remains is the hour cells and the dim rows Straum keeps for
the lookups.

The same view over a copy of the production tables themselves, bound at
ingest only, the same day and machine: 13,116,878 tasks, 5,690,651 task
groups, 851,964 storage locations, 2,645 MiB of tasks in PostgreSQL.
Straum loaded in 8 s and held 897 MiB; every ask equalled PostgreSQL
before and after the streamed changes.

| Rollup | Cells |
| --- | ---: |
| 1 hour | 311,969 |
| 1 day | 66,107 |

The generated tables spread each organization's picks evenly over the
hours and so held ten times the hour cells the real ones do: picks
cluster in working hours. Memory did not fall with the cells, since most
of it is the 6.5 million dim rows Straum keeps for the lookups. The
asks, PostgreSQL run twice on the loaded tables and Straum the better
of two:

| Ask | Rows | PostgreSQL, first | PostgreSQL, second | Straum |
| --- | ---: | ---: | ---: | ---: |
| Picks by organization, one day | 44 | 24 ms | 12 ms | 0.3 ms |
| Picks by organization and category, one week | 110 | 657 ms | 551 ms | 1.1 ms |
| Live picks by location type, one day | 3 | 93 ms | 85 ms | 1.0 ms |
| Picks per hour for one organization, one day | 16 | 7 ms | 4 ms | 0.3 ms |

PostgreSQL has an index on `completed` here, and these are the plain
queries the asks mirror, not the dashboard's own. Straum reads the
cells of one width in the range whatever the question, so its time
barely moves between them.
