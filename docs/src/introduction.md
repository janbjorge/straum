# Straum

Straum keeps aggregates of PostgreSQL tables in memory and answers
asks about them over HTTP in microseconds.

A dashboard runs the same `GROUP BY` over the same tables all day, and
PostgreSQL reads the rows again each time. Straum reads them once,
keeps each group as a cell in memory, and follows every commit through
logical replication. You declare what to keep as a view, in a small
subset of PostgreSQL's `CREATE VIEW`, here over New York taxi trips and
their pickup zones:

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

An ask is a `SELECT` over that view, with every value as a parameter.
This one asks for card trips, payment type 1, by pickup borough on one
day in New York:

```sh
curl --json '{
  "query": "SELECT borough, trips, fare FROM trips WHERE payment_type = $1 AND bucket >= $2 AND bucket < $3 GROUP BY borough",
  "parameters": [1, "2024-01-17T00:00:00-05:00", "2024-01-18T00:00:00-05:00"]
}' http://localhost:8080/ask
```

```json
{
  "as_of": "2026-11-13T07:52:38.699696541Z",
  "rows": [
    {"borough": "Bronx", "trips": 180, "fare": 6422.60},
    {"borough": "Brooklyn", "trips": 551, "fare": 16799.70},
    {"borough": "EWR", "trips": 9, "fare": 1000.50},
    {"borough": "Manhattan", "trips": 77355, "fare": 1169745.83},
    {"borough": "N/A", "trips": 36, "fare": 5089.44},
    {"borough": "Queens", "trips": 6785, "fare": 361261.11},
    {"borough": "Staten Island", "trips": 1, "fare": 100.00},
    {"borough": "Unknown", "trips": 415, "fare": 8095.09}
  ]
}
```

Every commit made before `as_of` is in the answer, and an ask never
touches PostgreSQL. On six months of New York taxi trips, the
[benchmark](benchmark.md) answers in 125 to 324 µs where PostgreSQL
takes 28.1 to 1,750.8 ms, from 65 MiB of memory.

![Straum between PostgreSQL and the apps that ask it](diagrams/overview.svg)

*A table owner runs the SQL `straum plan` prints, once. `straum run`
loads the tables and follows them. Apps ask over HTTP.*

## What it is good for

A question asked often, about tables that keep changing, whose answer
is counts, sums, averages, minima or maxima by a few keys over time:

- Dashboard tiles and charts: trips per hour, fares by borough, the
  same `GROUP BY` on every page load.
- Numbers an app shows its own users, such as usage this month or
  orders today. Make the tenant a key and each ask filters on it.
- A summary you maintain by hand today: a materialized view refreshed
  on a schedule, or a rollup table kept by triggers. Straum follows
  inserts, updates and deletes itself, and every answer is exact.

## What it is not

Straum is not a SQL engine, a replica, a cache or a durable store. It
answers only what its spec declares, so it has no ad hoc queries, no
rows and [no median](aggregates.md#median-and-percentiles). It keeps
nothing on disk: every start loads every table again, and a crash
leaves no slot behind holding WAL. PostgreSQL stays the source of
truth.

Straum supports PostgreSQL 16 and newer as shipped, with no extension
and no superuser, so managed services work. The name is Nynorsk for a
stream or current: Straum's input is PostgreSQL's replication stream.

> [!WARNING]
> Straum is not stable yet. The spec, the asks and the flags may change.

The [quick start](quick-start.md) runs this example in Docker in four
steps. After it, a [spec](spec.md) and an [ask](asks.md) are the two
things you write.
