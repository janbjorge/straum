# Benchmark

Straum's benchmark asks two questions of one month of New York taxi
trips: do answers from a rollup equal the same query over the raw rows,
and how much faster are they than PostgreSQL?

The data is the TLC yellow trips of January 2024, 2,964,624 trips, and
the 265 rows of `taxi_zone_lookup.csv`. The spec is
`crates/straum-notation/tests/taxi.sql`:

```sql
CREATE VIEW trips WITH (key = id, buckets = '1 day, 7 days') AS
SELECT date_bin('15 minutes', f.pickup_at, timestamptz 'epoch') AS bucket,
       f.payment_type,
       f.vendor_id,
       f.pu_location_id,
       f.do_location_id,
       pickup.borough,
       pickup.zone,
       pickup.service_zone,
       count(*) AS trips,
       sum(f.fare_amount) AS fare,
       max(f.tip_amount) AS tip_max,
       avg(f.trip_distance) AS distance,
       count(*) FILTER (WHERE f.payment_type = 1) AS card,
       count(*) FILTER (WHERE pickup.borough = 'Manhattan') AS manhattan_pickups
FROM public.yellow_trips f
LEFT JOIN public.taxi_zones pickup ON pickup.location_id = f.pu_location_id
LEFT JOIN public.taxi_zones dropoff ON dropoff.location_id = f.do_location_id
GROUP BY 1, 2, 3, 4, 5, 6, 7, 8;
```

## Answers equal the raw join

```sh
bench/run.sh
```

It needs Docker, `curl` and [uv](https://docs.astral.sh/uv/). The first
run downloads the month into `bench/data/`, which git ignores, and turns
the Parquet file into CSV. Then it starts a throwaway `postgres:18`,
loads the tables (`bench/load.sql`) and runs two scripts. A run takes
under a minute once the data is there.

`bench/rollup.sql` builds the rollup Straum keeps, in SQL: one row per
payment type, vendor, pickup zone, dropoff zone and bucket, at each width
the spec names, 15 minutes, 1 day and 7 days. Zones are not part of the
key; they are mapped after aggregation, as Straum does. It prints how far
rows collapse into cells:

| Rollup | Rows | Trips a row |
| --- | ---: | ---: |
| Raw trips | 2,964,624 | 1.0 |
| 15 minutes | 2,322,783 | 1.3 |
| 1 hour | 1,685,731 | 1.8 |
| 15 minutes, no dropoff zone | 568,612 | 5.2 |
| 1 day | 519,726 | 5.7 |
| 7 days | 199,398 | 14.9 |

Pickup and dropoff zone together are nearly unique per trip, so this spec
collapses little at 15 minutes. Without the dropoff zone it collapses 5.2
times. Memory follows cells, so the key is the lever on memory.

`bench/compare.sql` runs eight asks twice, over the raw join and from the
15 minute rollup, and compares the two with `EXCEPT ALL` both ways. An
ask without a time key runs a third time from the buckets Straum reads
for it: the widest that fit its range, narrower ones at the ends. For
2024-01-03 05:15 to 2024-01-27 13:30 that is 15 minute buckets to
midnight, three weeks (weeks start on a Thursday, at the epoch), two days
and 15 minute buckets again. The asks cover:

- avg from sum and count, and both filtered counts;
- filters on by columns and on zone attributes;
- both refs to the zones table, pickup and dropoff;
- ranges on 15 minute edges inside an hour.

A ref id with no zone row groups under null on both sides, as a left join
does. Every row matches. A run that finds a difference fails with
`rollup-then-map differs from the raw join`.

## Speed

These numbers come from one run on an Apple M2 Pro with 16 GB, on
2026-11-09, with PostgreSQL 18 and Straum on the same machine. The
harness that produced them is not in `bench/` yet.

The view for this run is taxi.sql with an hourly bucket and fewer keys,
payment type and pickup zone, keeping count, sum of fares and max tip,
with `buckets = '1 day, 7 days'`. Its hourly rollup has 173,660 cells.

PostgreSQL ran each query alone (`pgbench`, one client, 10 s) with a warm
cache. To give it the best plan, `yellow_trips` has an index on
`pickup_at` that includes every column the queries read, and a second
table, `roll`, holds the same hourly rollup as Straum, with its own
index. Straum's time is `Store::ask` on the loaded rollup, in process, the
median of 15 runs of 200 asks.

| Ask | Cells read | PostgreSQL, raw | PostgreSQL, `roll` | Straum | vs raw | vs `roll` |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| All fares, total | 4,938 | 66.8 ms | | 5.4 µs | 12,455x | |
| A month by pickup borough | 5,749 | 198.0 ms | 19.2 ms | 32.9 µs | 6,022x | 584x |
| One day by payment type and hour | 5,254 | 11.2 ms | 0.79 ms | 34.9 µs | 320x | 23x |
| One week of Manhattan by pickup zone | 4,565 | 37.8 ms | 5.1 ms | 31.5 µs | 1,198x | 161x |

Over HTTP, one client at a time, the same asks took 0.04 to 0.10 ms at
the median, between 9,722 and 20,293 asks a second. HTTP and JSON cost
more than the ask itself.

How to read the table:

- Straum's cost follows the cells an ask reads, a few nanoseconds each.
  Wider buckets keep that number small for long ranges: a month reads
  four weeks and three days of cells, not 744 hours.
- PostgreSQL's cost follows the rows it reads. Straum is furthest ahead
  where PostgreSQL reads the most rows for the fewest groups.
- Against a rollup table in PostgreSQL the gap is smaller, 23 to 584
  times, since both read rows that are summed already. Straum keeps that rollup current
  as trips change; PostgreSQL would have to rebuild or maintain it.
- An ask that reads many cells for a small answer, such as one day by
  payment type, is the slowest kind. A view without the pickup zone
  would answer it from about 120 cells, one per answer row, instead of
  5,254.

## The whole view, end to end

```sh
cargo test -p straum-pg --release taxi_month -- --ignored --nocapture
```

It needs the data from `bench/run.sh`. It loads the month into
`postgres:18` with the full taxi.sql view, has Straum load and follow it,
and compares asks with the view body in PostgreSQL after the load, after
202 streamed commits and after an update. One run on 2026-11-09, release
build, Docker on the same laptop:

| Step or ask | Groups | Straum | PostgreSQL |
| --- | ---: | ---: | ---: |
| Load | | 2.0 s | |
| 202 streamed commits | | 4.9 s | |
| Every key | 2,322,783 to 2,329,715 | 2.4 to 3.0 s | 6.2 to 6.4 s |
| By pickup borough | 8 | 6.6 to 8.8 ms | 3.8 to 4.0 s |

The ask by borough has no time key, so it reads mostly week and day
buckets and it is 432 to 606
times faster. The ask over every key reads every 15 minute cell and
answers nearly as many groups, so it is only 2.1 to 2.6 times faster.
