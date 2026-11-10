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
month into `bench/data/`, which git ignores. Then it builds Straum,
starts a throwaway `postgres:18`, loads the tables, runs the SQL `straum
plan` prints and starts `straum run`. Each ask goes to PostgreSQL as SQL
over the raw tables and to Straum as an [ask](asks.md) over HTTP, and a
run stops at the first answer that differs. A run takes under half a
minute once the data is there.

`bench/transaction.py` does the same setup, then one transaction of
1,000 to 1,000,000 changed rows while a client asks Straum back to back,
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

The view keeps 173,660 hour cells for the 2,964,624 trips, 17.1 trips
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
parameter. Where the SQL has no range, the ask's is 2000 to 2030, which
holds every trip:

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

## Results

One run on an Apple M2 Pro with 16 GB, on 2026-11-10: PostgreSQL 18.6 in
Docker Desktop with 10 CPUs, and Straum's release build on the same
machine. Each time is the median over at least a second, from one Python
client: asyncpg for PostgreSQL, aiohttp and orjson for Straum. Rows are
the answer's.

| Ask | Rows | PostgreSQL | Straum | Faster |
| --- | ---: | ---: | ---: | ---: |
| Trips | 1 | 37.7 ms | 148 µs | 254x |
| Card trips | 1 | 64.6 ms | 162 µs | 400x |
| Trips by pickup borough | 8 | 171.2 ms | 172 µs | 997x |
| Fares by payment type | 5 | 133.3 ms | 190 µs | 702x |
| Manhattan pickups, one week | 1 | 75.1 ms | 173 µs | 434x |
| Trips per hour, one day | 24 | 22.1 ms | 181 µs | 122x |

Straum was ready 1.4 s after it started and held 40 MiB. In PostgreSQL
the table and its indexes take 363 MiB. A run right before stayed
within 7% of these times.

- PostgreSQL runs with its defaults, two parallel workers per query,
  except `shared_buffers`: 2 GB, so the table stays in its cache.
  `yellow_trips` has an index on `pickup_at`, which the week and day
  asks use, and the gap is smallest there, where PostgreSQL reads least.
- PostgreSQL's time follows the rows it reads: every trip for the first
  four asks, a week's or a day's for the last two. Straum's hardly
  changes from ask to ask: most of it is the HTTP round trip, reading
  the query and writing JSON.
- Straum answers only what the view keeps: these keys, on hour edges.
  PostgreSQL answers any query over the raw rows. Straum keeps its
  cells current as trips change; a summary table in PostgreSQL would
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
transaction holds seven times the whole taxi view. That sets the
default of `--max-transaction-rows` at 100,000: a transaction at the cap
holds the lock for 20 ms and 29 MiB, and anything larger is cheaper as
a [reload](how-it-works.md#applying-a-commit), which takes the rows in
through the bulk `COPY` instead.

## Cells

Memory follows cells, and the key decides how many cells there are.
Measured on the taxi month under a wide key, and on a warehouse's
tables under a narrow one.

### The taxi month under a wide key

```sh
cargo test -p straum-pg --release taxi_month -- --ignored --nocapture
```

It needs the data `bench/bench.py` fetches. It loads the month with
`crates/straum-notation/tests/taxi.sql`, a wider view than the
benchmark's: 15 minute buckets with day and week widths, keyed by payment
type, vendor, pickup zone, dropoff zone and the pickup zone's borough,
zone and service zone, with two filtered counts. It has Straum load and
follow it, and compares asks with the view body in PostgreSQL after the
load, after 200 streamed commits, after an update, and after two zones
change and a trip in one of them makes Straum start over. One run on
2026-11-10, release build, Docker on the same laptop:

| Step or ask | Groups | Straum | PostgreSQL |
| --- | ---: | ---: | ---: |
| Load | | 2.1 s | |
| 200 streamed commits | | 4.1 s | |
| Start over after the zones changed | | 2.6 s | |
| Every key | 2,322,783 to 2,329,698 | 1.7 to 1.8 s | 4.6 to 4.8 s |
| By pickup borough | 8 | 4.5 to 5.2 ms | |

The ask over every key reads every 15 minute cell and answers nearly as
many groups, so it is only 2.6 to 2.8 times faster than the view in
PostgreSQL. Cells per width, each a `count(DISTINCT ...)` of that key
and the bucket over the month:

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
collapses 5.2 times.

### A warehouse's tables

The same lever on a warehouse shape: tasks keyed by organization,
whether the task is live, its group's category and its storage
location's type, over hour buckets with a day width beside them. The
category and the type are columns of two dims, read from the group
and location tables as each task comes in. The tables are a copy of a
production database and are not in the repository: 13,116,878 tasks,
5,690,651 task groups and 851,964 storage locations, 2,645 MiB of tasks
in PostgreSQL. One run on 2026-11-10, the machine and build of the
[Results](#results).

| Rollup | Cells |
| --- | ---: |
| 1 hour | 311,969 |
| 1 day | 66,107 |

Keyed by task group and storage location instead, the view would hold
about a cell a task, 12 million a width: a group holds one task or two,
and every task has its own hour. The dim columns' few values collapse the
day width to 66 thousand cells.

Straum was ready 19 s after it started and held 84 MiB: the cells, and
nothing else. Holding the 6.5 million group and location rows for
lookups of its own, as it did before joins moved to PostgreSQL, it held
897 MiB and was ready in 8 s.

Every ask equalled PostgreSQL after the load and after each streamed
change. Each change is one transaction, applied once its dim columns are
looked up; the time runs from the commit until Straum had applied it:

| Change | Rows | Applied |
| --- | ---: | ---: |
| New tasks for one organization, completed now | 1,000 | 74 ms |
| Open tasks completed, one in fifty | 19,024 | 3.4 s |
| Completed tasks of one day cancelled, one in twenty | 1,873 | 156 ms |
| Tasks of one day deleted, one in thirty | 7,426 | 275 ms |

An update looks up the old row and the new, so the big one asked
PostgreSQL for 38 thousand group keys and as many location keys. A task
group recategorised is nothing to Straum until a task of it changes;
then Straum starts over, 20 s, and the asks equal PostgreSQL again.

The asks, PostgreSQL run twice on the loaded tables and Straum the better
of two:

| Ask | Rows | PostgreSQL, first | PostgreSQL, second | Straum |
| --- | ---: | ---: | ---: | ---: |
| Picks by organization, one day | 44 | 19 ms | 10 ms | 0.3 ms |
| Picks by organization and category, one week | 110 | 667 ms | 603 ms | 0.9 ms |
| Live picks by location type, one day | 3 | 100 ms | 92 ms | 0.9 ms |
| Picks per hour for one organization, one day | 16 | 7 ms | 5 ms | 0.9 ms |

PostgreSQL has an index on `completed` here, and these are the plain
queries the asks mirror, not a dashboard's own. Straum reads the cells
of one width in the range whatever the question, so its time barely
moves between them.
