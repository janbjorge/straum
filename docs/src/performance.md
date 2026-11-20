# Performance

One benchmark measures how fast Straum answers and how much memory it
takes; [Running it yourself](#running-it-yourself) has the commands. It
sends Straum and PostgreSQL the same asks about six months
of New York taxi trips, checks that the answers are equal, then times
both. [Against a materialized view](#against-a-materialized-view) asks
the same of a materialized view and measures its refresh,
[Transactions](#transactions) what one large transaction costs Straum,
[Physical write-ahead log (WAL)](#physical-wal) how fast Straum reads
the physical WAL, and [Memory](#memory) how many bytes a
cell takes, on the taxi month and on a copy of a warehouse's tables.

The data is the Taxi and Limousine Commission (TLC) yellow trips of
January to June 2024, 20,332,093 trips, and the 265 rows of
`taxi_zone_lookup.csv`. The loader pins each file's SHA-256 and stops if
the TLC republished one, since these counts hold only for those bytes.

## Results

Sixteen fixed asks over the benchmark's two [views](#the-views), each
sent to PostgreSQL as SQL over the raw tables and to Straum as an ask
over HTTP. Rows are the answer's. One run on an Apple M2 Pro with
16 GB, on 2026-11-19: PostgreSQL 18.7 in Docker Desktop with 10 CPUs,
and Straum's release build on the same machine. Each time is the median
over at least a second and five runs, from one Python client: asyncpg
for PostgreSQL, aiohttp and orjson for Straum.

PostgreSQL runs with its defaults, two parallel workers per query,
except `shared_buffers` at 2 GB; the rest of the table comes from the
OS cache. The other settings in `bench/taxi.py` only speed the load.
`yellow_trips` has an index on `pickup_at`, which the week and day asks
use.

| Ask | Rows | PostgreSQL | Straum | Faster |
| --- | ---: | ---: | ---: | ---: |
| Trips | 1 | 214.8 ms | 146 µs | 1,472x |
| Card trips | 1 | 381.8 ms | 223 µs | 1,715x |
| Trips by pickup borough | 8 | 1,058.8 ms | 269 µs | 3,938x |
| Fares by payment type | 6 | 795.3 ms | 340 µs | 2,342x |
| Fare spread by payment type | 6 | 839.9 ms | 437 µs | 1,921x |
| Median fare | 1 | 4,839.8 ms | 25.2 ms | 192x |
| p99 fare by payment type | 6 | 8,670.9 ms | 22.1 ms | 392x |
| Most common fare by payment type | 6 | 9,550.7 ms | 36.2 ms | 264x |
| Distinct fares by payment type | 6 | 10,972.6 ms | 8.2 ms | 1,336x |
| Median fare per hour, one day | 24 | 47.7 ms | 669 µs | 71x |
| Manhattan pickups, one week | 1 | 84.2 ms | 236 µs | 356x |
| Trips per hour, one day | 24 | 16.8 ms | 164 µs | 103x |
| Trips per hour by borough, one month | 4,454 | 964.9 ms | 2.7 ms | 358x |
| Trips per day by borough, one month | 243 | 966.0 ms | 395 µs | 2,444x |
| Fares per week | 31 | 2,154.9 ms | 233 µs | 9,260x |
| Tips by dropoff borough | 8 | 1,746.0 ms | 225 µs | 7,754x |

The target is under 1 ms for every ask ([architecture decision record
ADR-0005](https://github.com/janbjorge/straum/blob/main/docs/adr/0005-asks-are-straum-sql-with-parameters.md)).
The percentile, mode and distinct asks over every trip miss it, as does
the month of hourly rows by borough, which writes 4,454 rows.

PostgreSQL's time follows the rows it reads: every trip, except in the
week and day asks, which read a week's or a day's through the index and
show the smallest gap. Straum's time hardly changes from ask to ask;
most of it is the HTTP round trip, reading the query and writing JSON.

The percentile, mode and distinct asks are the exception. Straum gathers
every fare of the cells in range, so their time follows the fares the
range holds ([Median and percentiles](aggregates.md#median-and-percentiles)).
A group by payment type over six months meets every weekly cell of
every zone, 1,426,949 fares in 6,398 cells for the largest group, and
marks each in a set of bits from the least fare to the greatest. Count,
sum and average of the distinct fares mark them once and share the
tally. Merging them in order on a heap, once per measure, took
423.3 ms; marking them once per measure took 24.4 ms.

Straum was ready 16.3 s after it started and held 305 MiB, the 265 zone
rows' projection included; in PostgreSQL the table and its indexes take
2,475 MiB. Runs the same day show where Straum's memory goes: without
the fare's variance, standard deviation, mode and distinct values it
held 190 MiB; with the first two, 205 MiB; with the mode too, 204 MiB.
The sums of squares take about 15 MiB, and the mode reads the counts
the percentiles keep. The distinct fares took the other 101 MiB, as
`count(DISTINCT c)` kept a count per value of its own. It has read the
percentiles' counts since, and a run on 2026-11-20 held 202 MiB.

On macOS the bench reads what Straum holds from the process's footprint,
which counts the pages the system compressed; on a busy machine one
resident set read 21 MiB. Elsewhere it reads the resident set.

Straum answers only what the views keep: these keys, on hour edges.
PostgreSQL answers any query over the raw rows. The times above are
PostgreSQL reading every trip; a dashboard that can't wait for that
keeps a summary instead. [Against a materialized
view](#against-a-materialized-view) measures that summary: it answers
in milliseconds too, but only as of its last refresh, and it has no
answer for the percentile, mode and distinct asks.

## Against a materialized view

`bench/taxi.py materialized` builds one materialized view per view of
`bench/trips.sql`, with the same keys at the narrowest bucket, an hour
on the epoch grid, and a unique index on them so it can refresh
concurrently. `bench/materialized.py` writes both. The materialized
views keep counts and sums only, since those add across buckets: an
average is a sum over a count, a variance and standard deviation come
from the count, sum and sum of squares. Each ask then goes to
PostgreSQL over the raw tables, to the materialized view with the
ask's own range, and to Straum. The materialized view's answer must
equal Straum's, as the raw table's must, or the run stops.

A median, percentile, mode or distinct count over a range does not
merge from per-bucket rows ([Merging](aggregates.md#merging)), so
those asks have no materialized view form here. Straum answers them
from the count per value each cell keeps; a materialized view could
keep a row per bucket, key and fare to do the same, which this bench
does not build.

One run on 2026-11-20, on the machine and PostgreSQL of the
[Results](#results). Other builds and tests ran on the machine
meanwhile, its load average 6.08 when the run began, so these times
are rough; the raw-table times differ from the Results' for that
reason.

| Ask | Rows | PostgreSQL | Materialized view | Straum |
| --- | ---: | ---: | ---: | ---: |
| Trips | 1 | 225.5 ms | 31.4 ms | 145 µs |
| Card trips | 1 | 413.8 ms | 27.2 ms | 225 µs |
| Trips by pickup borough | 8 | 1,126.3 ms | 52.2 ms | 265 µs |
| Fares by payment type | 6 | 850.4 ms | 61.2 ms | 337 µs |
| Fare spread by payment type | 6 | 953.2 ms | 68.3 ms | 438 µs |
| Median fare | 1 | 5,293.9 ms | needs the raw rows | 25.7 ms |
| p99 fare by payment type | 6 | 9,079.2 ms | needs the raw rows | 21.6 ms |
| Most common fare by payment type | 6 | 9,661.6 ms | needs the raw rows | 36.4 ms |
| Distinct fares by payment type | 6 | 11,200.9 ms | needs the raw rows | 8.6 ms |
| Median fare per hour, one day | 24 | 49.3 ms | needs the raw rows | 639 µs |
| Manhattan pickups, one week | 1 | 75.9 ms | 3.1 ms | 203 µs |
| Trips per hour, one day | 24 | 17.8 ms | 903 µs | 162 µs |
| Trips per hour by borough, one month | 4,454 | 994.8 ms | 30.7 ms | 2.6 ms |
| Trips per day by borough, one month | 243 | 982.6 ms | 27.1 ms | 387 µs |
| Fares per week | 31 | 2,619.9 ms | 61.6 ms | 234 µs |
| Tips by dropoff borough | 8 | 1,814.2 ms | 58.0 ms | 216 µs |

The materialized view reads its rows in the ask's range: every hour
of six months for the asks over all time, a day's or a week's through
its index for the rest. It answers in 903 µs to 68.3 ms, faster than
the raw tables and slower than Straum, which reads cells of the widest
width that fits. This bench builds one materialized view per view. One
per ask, grouped by that ask's keys alone, would hold fewer rows and
answer sooner, and need a refresh of its own.

What keeps it current is the refresh. Each one reads every trip again:

| Materialized view | Rows | Size | Build | Refresh | Refresh concurrently |
| --- | ---: | ---: | ---: | ---: | ---: |
| `trips_hourly` | 1,157,581 | 153 MiB | 9,171.5 ms | 7,855.1 ms | 10,819.7 ms |
| `dropoffs_hourly` | 713,457 | 91 MiB | 6,794.4 ms | 6,934.4 ms | 9,320.0 ms |

Build is the `CREATE` and its index; each refresh time is the median
of three. A plain refresh locks the materialized view, so asks wait
for it; a concurrent one lets them read the old rows and takes longer.
Between refreshes every answer is as old as the last refresh's start,
and the server pays a refresh's read of every trip each time, whether
a trip changed or not. Straum applies a commit in milliseconds
([Transactions](#transactions)), and an ask after it reads the change.

The rows match the cells Straum keeps at its hour width
([The views](#the-views)). The two materialized views took 245 MiB on
disk with their indexes; Straum held 201 MiB in memory in the same
run, both views with every measure, the fares' counts per value
included.

## Running it yourself

```sh
bench/taxi.py
```

It needs Docker, a Rust toolchain and [uv](https://docs.astral.sh/uv/).
The first run downloads the six months into `bench/data/`, which git
ignores. Then it builds Straum, starts a throwaway `postgres:18` (or
the major `STRAUM_TEST_POSTGRES` names), loads the months with
`bench/load_taxi_trips.py`, runs the SQL `straum plan` prints and
starts `straum run`. Last, it sends each ask to both sides and stops at
the first answer that differs.

Answers are equal when every decimal matches to the last digit, with
PostgreSQL's `avg`, variance and standard deviation rounded half to even
at the places Straum gives them. A run takes about 80 seconds once the
data is there.

The script has four modes, each after the same setup. `bench/taxi.py`
or `bench/taxi.py asks` gives the [Results](#results).
`bench/taxi.py materialized` builds and refreshes the materialized
views, then asks all three sides
([Against a materialized view](#against-a-materialized-view)).
`bench/taxi.py transactions` runs one transaction of 1,000 to 1,000,000
changed rows while a client asks Straum back to back
([Transactions](#transactions)). `bench/taxi.py shell` opens
[`straum shell`](cli.md#straum-shell) at the trips in place of the
asks; leaving the shell removes PostgreSQL.

Each mode takes `--wal physical` to have Straum read the
[physical WAL](postgresql.md#physical-wal) in place of logical
decoding. The script then turns `full_page_writes` back on after the
load and takes a checkpoint, since the physical stream needs page
images.

`just taxi [asks|transactions|materialized|shell]` runs the same, with
`wal=physical` for the physical WAL and `postgres=17` for another
major, as in `just postgres=17 wal=physical taxi`.

## The views

`bench/trips.sql` holds two views. `trips` is the [quick start's
view](quick-start.md#2-the-spec) with more measures over the fare:
variance, standard deviation, median, p99, mode, and the count, sum and
average of distinct fares. It is what a dashboard over these trips would
declare. `dropoffs` is the other end of the same trips: tips, the
average distance and fares per hour and dropoff zone.

```sql
CREATE VIEW dropoffs AS
SELECT buckets(dropoffs.pickup_at, '1 hour', '1 day', '7 days') AS bucket,
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
[bound cells](https://github.com/janbjorge/straum/blob/main/docs/adr/0006-refs-are-bound-at-ask-time.md):
each joins on one of its own keys, so bound cells would double them.

## The asks

PostgreSQL gets the query anyone would write over the raw tables, in a
session whose time zone is New York's, as the TLC's times are.

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

-- Fare spread by payment type
SELECT payment_type, var_samp(fare_amount), stddev_samp(fare_amount)
  FROM yellow_trips
 GROUP BY payment_type;

-- Median fare
SELECT percentile_disc(0.5) WITHIN GROUP (ORDER BY fare_amount) FROM yellow_trips;

-- p99 fare by payment type
SELECT payment_type,
       percentile_disc(0.99) WITHIN GROUP (ORDER BY fare_amount)
  FROM yellow_trips
 GROUP BY payment_type;

-- Most common fare by payment type
SELECT payment_type, mode() WITHIN GROUP (ORDER BY fare_amount)
  FROM yellow_trips
 GROUP BY payment_type;

-- Distinct fares by payment type
SELECT payment_type, count(DISTINCT fare_amount), sum(DISTINCT fare_amount),
       avg(DISTINCT fare_amount)
  FROM yellow_trips
 GROUP BY payment_type;

-- Median fare per hour, one day
SELECT date_trunc('hour', pickup_at),
       percentile_disc(0.5) WITHIN GROUP (ORDER BY fare_amount)
  FROM yellow_trips
 WHERE pickup_at >= '2024-01-17' AND pickup_at < '2024-01-18'
 GROUP BY 1;

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

-- Trips per hour by borough, one month
SELECT date_bin('1 hour', t.pickup_at, timestamptz 'epoch'), z.borough, count(*)
  FROM yellow_trips t
  LEFT JOIN taxi_zones z ON z.location_id = t.pu_location_id
 WHERE t.pickup_at >= '2024-01-01T00:00:00Z' AND t.pickup_at < '2024-02-01T00:00:00Z'
 GROUP BY 1, 2;

-- Trips per day by borough, one month
SELECT date_bin('1 day', t.pickup_at, timestamptz 'epoch'), z.borough, count(*)
  FROM yellow_trips t
  LEFT JOIN taxi_zones z ON z.location_id = t.pu_location_id
 WHERE t.pickup_at >= '2024-01-01T00:00:00Z' AND t.pickup_at < '2024-02-01T00:00:00Z'
 GROUP BY 1, 2;

-- Fares per week
SELECT date_bin('7 days', pickup_at, timestamptz 'epoch'), count(*),
       sum(fare_amount)
  FROM yellow_trips
 WHERE pickup_at IS NOT NULL
 GROUP BY 1;

-- Tips by dropoff borough
SELECT z.borough, count(*), sum(t.tip_amount), avg(t.trip_distance),
       sum(t.fare_amount)
  FROM yellow_trips t
  LEFT JOIN taxi_zones z ON z.location_id = t.do_location_id
 GROUP BY z.borough;
```

Straum gets the same asks over the views, every value a parameter;
`bench/taxi.py` holds both. Where the SQL has no range, the ask's is
2000 to 2030, which holds every trip; Fares per week leaves its range
open instead, so it reads every bucket Straum holds. An example:

```sql
-- Trips by pickup borough
SELECT borough, trips FROM trips
 WHERE bucket >= $1 AND bucket < $2
 GROUP BY borough
```

## Transactions

Straum holds a transaction until its commit and applies it whole under
its write lock: its rows wait in memory, and every ask waits for the
apply. `bench/taxi.py transactions` updates the fare of the first N
trips, which both views follow, in one transaction, while a client
asks for one day of hour buckets as fast as the answers come. It
measures the time from the commit until Straum has applied it, the
longest ask meanwhile (the lock hold as a client feels it), and how
much Straum's resident set grew. After each transaction it checks the
fares by payment type against PostgreSQL; an answer that differs ends
the run.

One run on 2026-11-17, on the machine and PostgreSQL of the
[Results](#results). `synchronous_commit` is back on once the trips are
loaded, so a commit counts from its flush, as on a real server. Straum
held 121 MiB after the load, and an idle ask took 204 µs.

| Rows changed | Commit to applied | Longest ask meanwhile | Memory grown |
| ---: | ---: | ---: | ---: |
| 1,000 | 11.7 ms | 1.8 ms | 1 MiB |
| 10,000 | 93.3 ms | 12.4 ms | 3 MiB |
| 100,000 | 827.5 ms | 118.1 ms | 56 MiB |
| 1,000,000 | 8.5 s | 1,238.7 ms | 457 MiB |

Most of the time goes to PostgreSQL decoding the transaction and Straum
reading it, outside the lock. The lock itself is held for about 1.2 ms
per 1,000 rows, half of it moving each changed fare between the
percentiles' value counts. Memory is what grows: the rows of a
transaction wait in Straum until its commit, about 480 bytes each, so a
million-row transaction holds nearly four times what both views do.

That sets the default of `--max-transaction-rows` at 100,000: a
transaction at the limit holds the lock for 118 ms, and anything larger
is cheaper to [stop and load again](how-it-works.md#following), which
takes the rows through the bulk `COPY` instead. The bench raises the
limit out of the way, since it measures what the limit prevents.

## Physical WAL

```sh
cargo test -p straum-pg --release physical_wal_bench -- --ignored --nocapture
```

`just wal-bench` runs the same. It measures how fast Straum reads
[physical WAL](https://github.com/janbjorge/straum/blob/main/docs/adr/0008-straum-reads-physical-wal-where-logical-decoding-is-off.md).

It loads the taxi month with `bench/load_taxi_trips.py`, so it needs
uv, into a PostgreSQL under `wal_level = replica`, takes a checkpoint
and opens a physical replication slot. Four phases each change
1,000,000 trips of `yellow_trips`: `COPY` in transactions of 250,000
rows, then `INSERT`, `UPDATE` and `DELETE` in transactions of 10,000.
The slot streams the WAL, and Straum keeps it in memory. Autovacuum is
off and the checkpoint interval is an hour, so each run writes the same
records. `STRAUM_TEST_POSTGRES` picks the major.

Straum then reads the captured WAL again, with no socket in the way,
three times each way (`STRAUM_BENCH_RUNS`, or `just runs=6 wal-bench`);
the fastest read counts. "Records read" only splits pages into records.
"Pipeline read" goes all the way to the commits for
`crates/straum-notation/tests/taxi.sql`; every row changed must come
out, and nothing may be unfit. "Pass-through read" goes through a
pipeline that reads no table, as the WAL of every other table and
database does. The reader skips those records by their block headers,
without copying or checksumming them, so it outruns splitting every
record.

One run on 2026-11-16, on the machine of the [Results](#results), one
thread, PostgreSQL 18, six reads each way:

| Phase | WAL | Records | Images | PostgreSQL wrote | Records read | Pipeline read | Rows/s | Against PostgreSQL | Pass-through read |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| copy | 128.1 MiB | 1,044,744 | 10 | 1.6 s | 2,989 MiB/s | 525 MiB/s | 4,100,208 | 6.4x | 7,411 MiB/s |
| insert | 173.3 MiB | 2,033,148 | 3 | 1.1 s | 2,100 MiB/s | 538 MiB/s | 3,104,631 | 3.4x | 5,386 MiB/s |
| update | 356.9 MiB | 3,008,774 | 13,447 | 2.3 s | 2,332 MiB/s | 538 MiB/s | 1,507,147 | 3.5x | 6,401 MiB/s |
| delete | 137.7 MiB | 1,010,911 | 10,712 | 480.2 ms | 2,977 MiB/s | 457 MiB/s | 3,321,773 | 1.6x | 5,945 MiB/s |
| all | 795.9 MiB | | | 5.4 s | 2,452 MiB/s | 520 MiB/s | 2,613,459 | 3.5x | 6,200 MiB/s |

Images are full-page images. "Against PostgreSQL" divides the time
PostgreSQL took to write a phase's WAL by the time the pipeline took to
read it. Below 1x, a writer that keeps that pace for long leaves Straum
behind, and once the lag passes `--max-lag`, `/readyz` turns off. The
pipeline held at most 429.5 MiB of rows, most of them the open `COPY`
transactions' rows, which wait for their commit.

Timings drift with the machine, so two tests in
`crates/straum-pg/src/wal/pipeline.rs` hold what this bench gained by
counting allocations instead: a row of a spec table may allocate three
times, for its page's slot list, its bytes and its decoded values, and
a row of another table allocates nothing.

## Memory

Memory follows [cells](how-it-works.md#cells), and the rollup key, the
columns in `GROUP BY`, decides how many there are.

Two runs on 2026-11-11 measure it: the taxi month under a wide key, and
a warehouse's tables under a narrow one whose dims carry the ids. The
resident set came to 177 and 104 bytes a cell, measures and projections
included, before bound cells.

A cell has taken fewer bytes since
([ADR-0003](https://github.com/janbjorge/straum/blob/main/docs/adr/0003-straum-holds-cells-and-nothing-else.md)).
The benchmark's two views held 65 MiB for their 2,078,586 cells, 33
bytes a cell, before `trips` kept its fares for the percentiles; with
them, 113 MiB on 2026-11-15. With the measures the
[Results](#results) add, they held 305 MiB on 2026-11-19, then 202 MiB
on 2026-11-20 once the distinct values read the percentiles' counts.

### The taxi month under a wide key

```sh
cargo test -p straum-pg --release taxi_month -- --ignored --nocapture
```

`just taxi-month` runs the same. It loads the month with
`bench/load_taxi_trips.py`, so it needs uv, and reads it with
`crates/straum-notation/tests/taxi.sql`, a wider view than the
benchmark's: 15 minute buckets with day and week widths, keyed by
payment type, vendor, pickup zone, dropoff zone and the pickup zone's
borough, zone and service zone, with two filtered counts.

The test compares asks with the view body in PostgreSQL six times:
after the load, after 200 streamed commits, after an update, after a
zone is renamed, after a zone moves to a new key, and after a trip in
the renamed zone changes. Nothing stops. Release build, Docker on the
same laptop:

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
2.0 to 2.4 times faster than the view in PostgreSQL.

Cells per width, each a `count(DISTINCT ...)` of that key and the
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
spec collapses little at 15 minutes; without the dropoff zone it
collapses 5.2 times.

### A warehouse's tables

The view keys tasks by organization, whether the task is live, its
group's category and its storage location's type, over hour buckets
with a day width beside them. The category and the type are columns of
two dims, so the cells are keyed by the group and location ids, and
Straum holds each dim's projection beside them: its key and the one
column the view reads.

The tables are a copy of a production database and are not in the
repository: 13,116,878 tasks, 5,690,651 task groups and 851,964 storage
locations, 2,645 MiB of tasks in PostgreSQL. The machine and build are
those of the [Results](#results).

| Rollup | Cells |
| --- | ---: |
| 1 hour | 11,932,307 |
| 1 day | 11,930,678 |

A group holds one task or two, and every task has its own hour, so both
widths hold about a cell a task. The day width collapses nothing, since
the group id is in its key. Keyed by the category and the type instead,
as a view that never followed a dim change could be, the widths would
hold 311,969 and 66,107 cells.

Straum was ready 10 s after it started and held 2,378 MiB: the cells
and the two projections, 6,542,615 dim rows. A run right before held
1,998 MiB. That is 88 to 104 bytes a cell, projections included.

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

The last three change dims: each is applied to the projection, no cell
moves, and the next ask reads the new category or type. Straum never
stopped.

The asks, with PostgreSQL run twice on the loaded tables and Straum the
better of two:

| Ask | Rows | PostgreSQL, first | PostgreSQL, second | Straum |
| --- | ---: | ---: | ---: | ---: |
| Picks by organization, one day | 44 | 33 ms | 15 ms | 0.9 ms |
| Picks by organization and category, one week | 110 | 1,448 ms | 751 ms | 89.2 ms |
| Live picks by location type, one day | 3 | 127 ms | 100 ms | 6.7 ms |
| Picks per hour for one organization, one day | 16 | 14 ms | 5 ms | 0.6 ms |

PostgreSQL has an index on `completed` here, and the SQL is the plain
queries the asks mirror, not a dashboard's own. Straum reads the cells
of one width in the range, about a cell a task, and maps each through
the projections, so an ask over a week costs more than one over a day.

Every rule of a [spec](spec.md), every [ask and endpoint](asks.md) and
every [command and flag](cli.md) are in the reference.
