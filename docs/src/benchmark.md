# Benchmark

Straum's benchmark sends Straum and PostgreSQL the same asks about
six months of New York taxi trips, checks that the answers are equal, and
times both. [Memory](#memory) at the end is what two real tables
measured.

The data is the TLC yellow trips of January to June 2024, 20,332,093
trips, and the 265 rows of `taxi_zone_lookup.csv`.

## Results

Seven fixed asks over the benchmark's two views, each sent to PostgreSQL as SQL
over the raw tables and to Straum as an ask over HTTP. One run on an
Apple M2 Pro with 16 GB, on 2026-11-13: PostgreSQL 18.6 in Docker
Desktop with 10 CPUs, and Straum's release build on the same machine.
Each time is the median over at least a second, from one Python client:
asyncpg for PostgreSQL, aiohttp and orjson for Straum. Rows are the
answer's.

| Ask | Rows | PostgreSQL | Straum | Faster |
| --- | ---: | ---: | ---: | ---: |
| Trips | 1 | 218.2 ms | 126 µs | 1,735x |
| Card trips | 1 | 384.4 ms | 234 µs | 1,644x |
| Trips by pickup borough | 8 | 1,060.0 ms | 240 µs | 4,421x |
| Fares by payment type | 6 | 784.0 ms | 326 µs | 2,408x |
| Manhattan pickups, one week | 1 | 48.1 ms | 177 µs | 271x |
| Trips per hour, one day | 24 | 16.9 ms | 140 µs | 121x |
| Tips by dropoff borough | 8 | 1,708.2 ms | 194 µs | 8,784x |

Straum was ready 17.8 s after it started and held 296 MiB, the 265 zone
rows' projection included. In PostgreSQL the table and its indexes
take 2,474 MiB.

- Reads run with PostgreSQL's defaults, two parallel workers per query,
  except `shared_buffers`: 2 GB. The rest of the table comes from the
  OS cache. The other settings in `bench/taxi.py` only speed the load.
  `yellow_trips` has an index on `pickup_at`, which the week and day
  asks use, and the gap is smallest there, where PostgreSQL reads least.
- PostgreSQL's time follows the rows it reads: every trip, but for
  the week and day asks, which read a week's or a day's. Straum's hardly
  changes from ask to ask: most of it is the HTTP round trip, reading
  the query and writing JSON.
- Straum answers only what the views keep: these keys, on hour edges.
  PostgreSQL answers any query over the raw rows. Straum keeps its
  cells current as trips change; a summary table in PostgreSQL would
  have to be rebuilt or maintained.

## Running it yourself

```sh
bench/taxi.py
```

`bench/taxi.py shell` sets up the same, then leaves you in
[`straum shell`](running.md#straum-shell) at the trips in place of
running the asks. Leaving the shell removes PostgreSQL.

It needs Docker, a Rust toolchain and [uv](https://docs.astral.sh/uv/),
which fetches the script's Python packages. The first run downloads the
six months into `bench/data/`, which git ignores. Then it builds
Straum, starts a throwaway `postgres:18`, loads the months eight at a
time on their own connections, runs the SQL `straum plan` prints and
starts `straum run`. Each ask goes to PostgreSQL as SQL over the raw
tables and to Straum as an [ask](asks.md) over HTTP, and a run stops at
the first answer that differs. A run takes about a minute once the data
is there.

`bench/taxi.py transactions` does the same setup, then one transaction of
1,000 to 1,000,000 changed rows while a client asks Straum back to back,
under [Transactions](#transactions).

## The views

`bench/trips.sql` opens with the [quick start](quick-start.md)'s view, what a
dashboard over these trips would declare: trips, fares and the average
fare per hour, payment type and pickup zone, with day and week buckets
for long ranges.

```sql
CREATE VIEW trips WITH (buckets = '1 hour, 1 day, 7 days') AS
SELECT date_bin('1 hour', trips.pickup_at, timestamptz 'epoch') AS bucket,
       trips.payment_type AS payment_type,
       trips.pu_location_id AS pu_location_id,
       pickup.borough AS borough,
       count(*) AS trips,
       sum(trips.fare_amount) AS fare,
       avg(trips.fare_amount) AS avg_fare
FROM public.yellow_trips AS trips
LEFT OUTER JOIN public.taxi_zones AS pickup ON pickup.location_id = trips.pu_location_id
GROUP BY 1, 2, 3, 4;
```

`dropoffs` is the other end of the same trips: tips, the average
distance and fares per hour and dropoff zone.

```sql
CREATE VIEW dropoffs WITH (buckets = '1 hour, 1 day, 7 days') AS
SELECT date_bin('1 hour', dropoffs.pickup_at, timestamptz 'epoch') AS bucket,
       dropoffs.do_location_id AS do_location_id,
       dropoff.borough AS borough,
       count(*) AS trips,
       sum(dropoffs.tip_amount) AS tips,
       avg(dropoffs.trip_distance) AS avg_distance,
       sum(dropoffs.fare_amount) AS fare
FROM public.yellow_trips AS dropoffs
LEFT OUTER JOIN public.taxi_zones AS dropoff ON dropoff.location_id = dropoffs.do_location_id
GROUP BY 1, 2, 3;
```

Over the 20,332,093 trips, `trips` keeps 1,157,581 cells at its hour
width and `dropoffs` 713,457. Neither keeps
[bound cells](https://github.com/janbjorge/straum/blob/main/docs/adr/0042-bound-cells-are-kept-only-where-a-join-adds-a-key.md):
each joins on one of its own keys, so they would be as many again.

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

-- Tips by dropoff borough
SELECT z.borough, count(*), sum(t.tip_amount), avg(t.trip_distance),
       sum(t.fare_amount)
  FROM yellow_trips t
  LEFT JOIN taxi_zones z ON z.location_id = t.do_location_id
 GROUP BY z.borough;
```

Straum gets the same asks over the views, with every value a
parameter. Where the SQL has no range, the ask's is 2000 to 2030, which
holds every trip:

```sql
-- Trips
SELECT trips FROM trips WHERE bucket >= $1 AND bucket < $2

-- Card trips
SELECT trips FROM trips
 WHERE payment_type = $1 AND bucket >= $2 AND bucket < $3

-- Trips by pickup borough
SELECT borough, trips FROM trips
 WHERE bucket >= $1 AND bucket < $2
 GROUP BY borough

-- Fares by payment type
SELECT payment_type, trips, fare, avg_fare FROM trips
 WHERE bucket >= $1 AND bucket < $2
 GROUP BY payment_type

-- Manhattan pickups, one week
SELECT trips FROM trips
 WHERE borough = $1 AND bucket >= $2 AND bucket < $3

-- Trips per hour, one day
SELECT bucket, trips FROM trips
 WHERE bucket >= $1 AND bucket < $2
 GROUP BY bucket

-- Tips by dropoff borough
SELECT borough, trips, tips, avg_distance, fare FROM dropoffs
 WHERE bucket >= $1 AND bucket < $2
 GROUP BY borough
```

## Transactions

Straum holds a transaction until its commit and applies it whole under
its write lock, so its rows wait in memory and every ask waits for the
apply. `bench/taxi.py transactions` updates the fare of the first N trips,
which both views follow, in one transaction while a client asks for one
day of hour buckets as fast
as the answers come. It measures three things: the time from the commit
until Straum has applied it, the longest ask meanwhile, which is the
lock hold as a client feels it, and how much Straum's resident set grew.
One run on 2026-11-13, the machine, build and PostgreSQL of the
[Results](#results); Straum held 234 MiB after the load and an idle ask
took 136 µs:

| Rows changed | Commit to applied | Longest ask meanwhile | Memory grown |
| ---: | ---: | ---: | ---: |
| 1,000 | 219.4 ms | 0.8 ms | 2 MiB |
| 10,000 | 258.7 ms | 5.2 ms | 8 MiB |
| 100,000 | 625.0 ms | 57.6 ms | 62 MiB |
| 1,000,000 | 4.8 s | 617.8 ms | 601 MiB |

Most of the time goes to PostgreSQL decoding the transaction and Straum
reading it, outside the lock; the lock itself is held for about 0.6 ms
per 1,000 rows. Memory is what grows: the rows of a transaction wait in
Straum until its commit, about 630 bytes each, so a million-row
transaction holds two and a half times what both views do. That sets the
default of `--max-transaction-rows` at 100,000: a transaction at the limit
holds the lock for 58 ms, and anything larger is cheaper to
[start over](how-it-works.md#following), which takes the rows
through the bulk `COPY` instead.

## Memory

Memory follows cells, as [How Straum works](how-it-works.md#cells)
says. The rollup key, the columns in `GROUP BY`, decides how many
there are. Two runs on 2026-11-11 measure it: the taxi month under a
wide key, and a warehouse's tables under a narrow one whose dims carry
the ids. The resident set came to 177 and 104 bytes a cell, measures
and projections included, before bound cells.

### The taxi month under a wide key

```sh
cargo test -p straum-pg --release taxi_month -- --ignored --nocapture
```

It needs the data `bench/taxi.py` fetches. It loads the month with
`crates/straum-notation/tests/taxi.sql`, a wider view than the
benchmark's: 15 minute buckets with day and week widths, keyed by
payment type, vendor, pickup zone, dropoff zone and the pickup zone's
borough, zone and service zone, with two filtered counts. The test
compares asks with the view body in PostgreSQL six times: after the
load, after 200 streamed commits, after an update, after a zone is
renamed, after a zone moves to a new key, and after a trip in the
renamed zone changes. Nothing starts over. Release build, Docker on
the same laptop:

| Step or ask | Groups | Straum | PostgreSQL |
| --- | ---: | ---: | ---: |
| Load | | 2.0 s | |
| 200 streamed commits | | 5.1 s | |
| A zone renamed, one commit | | 2.4 ms | |
| A zone moved to a new key, one commit | | 2.1 ms | |
| A trip in the renamed zone, one commit | | 2.4 ms | |
| Every key | 2,322,783 to 2,329,715 | 2.7 to 3.2 s | 6.3 to 6.4 s |
| By pickup borough | 8 | 11.3 to 16.3 ms | 4.1 to 4.3 s |

The ask over every key reads every 15 minute cell, maps each through
the zones' projection and answers nearly as many groups, so it is only
2.0 to 2.4 times faster than the view in PostgreSQL. Cells per width,
each a `count(DISTINCT ...)` of that key and the bucket over the month:

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

Tasks keyed by organization, whether the task is live, its group's
category and its storage location's type, over hour buckets with a
day width beside them. The category and the type are columns of two
dims, so the cells are keyed by the group and location ids and Straum
holds each dim's projection beside them: its key and the one column
the view reads. The tables are a copy of a production database and
are not in the repository: 13,116,878 tasks, 5,690,651 task groups
and 851,964 storage locations, 2,645 MiB of tasks in PostgreSQL. The
machine and build of the [results](#results).

| Rollup | Cells |
| --- | ---: |
| 1 hour | 11,932,307 |
| 1 day | 11,930,678 |

A group holds one task or two, and every task has its own hour, so
both widths hold about a cell a task; the day width collapses nothing,
since the group id is in its key. Keyed by the category and the type
instead, as a view that never followed a dim change could be, the
widths would hold 311,969 and 66,107 cells.

Straum was ready 10 s after it started and held 2,378 MiB: the cells
and the two projections, 6,542,615 dim rows. A run right before held
1,998 MiB; 88 to 104 bytes a cell, projections included.

Every ask equalled PostgreSQL after the load and after each streamed
change. Each change is one transaction; the time runs from the commit
until Straum had applied it:

| Change | Rows | Applied |
| --- | ---: | ---: |
| New tasks for one organization, completed now | 1,000 | 99 ms |
| Open tasks completed, one in fifty | 19,024 | 5.9 s |
| Completed tasks of one day cancelled, one in twenty | 1,873 | 111 ms |
| Tasks of one day deleted, one in thirty | 7,426 | 95 ms |
| Task groups of one day recategorised | 3,365 | 208 ms |
| Storage locations retyped, one in ten | 85,196 | 1.1 s |
| Task groups deleted, one in a hundred | 56,875 | 3.8 s |

The last three change dims. Each is applied to the projection, no cell
moves, and the next ask reads the new category or type; Straum never
started over.

The asks, PostgreSQL run twice on the loaded tables and Straum the better
of two:

| Ask | Rows | PostgreSQL, first | PostgreSQL, second | Straum |
| --- | ---: | ---: | ---: | ---: |
| Picks by organization, one day | 44 | 33 ms | 15 ms | 0.9 ms |
| Picks by organization and category, one week | 110 | 1,448 ms | 751 ms | 89.2 ms |
| Live picks by location type, one day | 3 | 127 ms | 100 ms | 6.7 ms |
| Picks per hour for one organization, one day | 16 | 14 ms | 5 ms | 0.6 ms |

PostgreSQL has an index on `completed` here, and these are the plain
queries the asks mirror, not a dashboard's own. Straum reads the cells
of one width in the range, about a cell a task, and maps each through
the projections, so an ask over a week costs more than one over a day.
