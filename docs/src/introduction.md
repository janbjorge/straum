# Straum

Straum keeps aggregates of PostgreSQL tables in memory and answers
asks about them over HTTP in microseconds.

A dashboard runs the same `GROUP BY` over the same tables all day, and
PostgreSQL reads the rows again each time. Straum reads them once, from
a snapshot, keeps each group as a cell in memory, and follows every
commit through logical replication. Every answer reports how current it
is. You declare what to keep as a view, in a small subset of PostgreSQL's
`CREATE VIEW`, here over New York taxi trips and their pickup zones:

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

Every commit made before `as_of` is in the answer. An ask never touches
PostgreSQL; its cost follows the number of cells it reads. On six months
of New York taxi trips, the [benchmark](benchmark.md) answers in 144 to
349 µs where PostgreSQL takes 18 to 1,072 ms, from 362 MiB of memory.

![Straum between PostgreSQL and the apps that ask it](diagrams/overview.svg)

*A table owner runs the SQL `straum plan` prints, once. `straum run`
loads the tables and follows them, as [How Straum
works](how-it-works.md) describes. Apps ask over HTTP.*

Straum supports PostgreSQL 17 and newer as shipped, with no extension
and no superuser, so managed services work. The name is Nynorsk for a
stream or current: Straum's input is PostgreSQL's replication stream.

> [!WARNING]
> Straum is not stable yet. The spec, the asks and the flags may change.

The guide is in reading order. The [quick start](quick-start.md) runs
this example in Docker. [How Straum works](how-it-works.md) is the
one idea behind it, cells, and is enough to read the rest: the
[spec](spec.md), the [asks](asks.md), then [PostgreSQL](postgresql.md)
and [running](running.md) for production.

## What it is not

Straum is not a SQL engine, a replica, a cache or a durable store. It
answers only what its spec declares, and it keeps nothing on disk: every
start loads every table again, and a crash leaves no slot behind holding
WAL. PostgreSQL stays the source of truth.
