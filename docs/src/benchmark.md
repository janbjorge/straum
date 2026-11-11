# Benchmark

Straum's benchmark asks Straum and PostgreSQL the same questions about
one month of New York taxi trips, checks that the answers are equal, and
times both.

The data is the TLC yellow trips of January 2024, 2,964,624 trips, and
the 265 rows of `taxi_zone_lookup.csv`.

## Results

Six asks a dashboard would make, each sent to PostgreSQL as SQL over the
raw tables and to Straum as an ask over HTTP. One run on an Apple M2 Pro
with 16 GB, on 2026-11-10: PostgreSQL 18.6 in Docker Desktop with 10
CPUs, and Straum's release build on the same machine. Each time is the
median over at least a second, from one Python client: asyncpg for
PostgreSQL, aiohttp and orjson for Straum. Rows are the answer's.

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

## Running it yourself

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
CREATE VIEW trips WITH (buckets = '1 hour, 1 day, 7 days') AS
SELECT date_bin('1 hour', f.pickup_at, timestamptz 'epoch') AS bucket,
       f.payment_type AS payment_type,
       f.pu_location_id AS pu_location_id,
       pickup.borough AS borough,
       count(*) AS trips,
       sum(f.fare_amount) AS fare,
       avg(f.fare_amount) AS avg_fare
FROM public.yellow_trips AS f
LEFT OUTER JOIN public.taxi_zones AS pickup ON pickup.location_id = f.pu_location_id
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
```

## Transactions

Straum holds a transaction until its commit and applies it whole under
its write lock, so its rows wait in memory and every ask waits for the
apply. `bench/transaction.py` updates the fare of the first N trips in
one transaction while a client asks for one day of hour buckets as fast
as the answers come. It measures three things: the time from the commit
until Straum has applied it, the longest ask meanwhile, which is the
lock hold as a client feels it, and how much Straum's resident set grew.
One run on 2026-11-10, the machine, build and PostgreSQL of the
[Results](#results); Straum held 38 MiB after the load and an idle ask
took 136 µs:

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
