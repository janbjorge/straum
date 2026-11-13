# Straum

**In-memory PostgreSQL aggregates, kept current through logical replication.**

[![CI](https://github.com/janbjorge/straum/actions/workflows/ci.yml/badge.svg)](https://github.com/janbjorge/straum/actions/workflows/ci.yml)

Straum keeps the aggregates a spec declares as cells in memory, loaded
from a snapshot and kept current by every commit after it, and answers
asks about them over HTTP. On six months of New York taxi trips its asks
answer in 125 to 324 µs where PostgreSQL takes 28.1 to 1,750.8 ms, from 65 MiB
([benchmark](https://janbjorge.github.io/straum/benchmark.html)).
PostgreSQL stays the source of truth; Straum keeps nothing on disk.

**Not stable.** The spec, the asks and the flags may change.

## What it looks like

A spec is a view in a small subset of PostgreSQL's `CREATE VIEW`, here
over New York taxi trips and their pickup zones. Straum never creates
it; PostgreSQL would give the same rows.

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

`straum plan` prints the SQL a table owner runs once; `straum run` loads
the view and follows it. An ask is a `SELECT` over the view, every value
a parameter, here card trips by pickup borough on one day:

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

Every commit made before `as_of` is in the answer. The guide's [quick
start](https://janbjorge.github.io/straum/quick-start.html) runs this
from the `ghcr.io/janbjorge/straum` image against PostgreSQL in Docker,
in four steps, on the full month.

Straum is not a SQL engine, a replica, a cache or a durable store. It
answers only what its spec declares. It needs PostgreSQL 17 or newer as
shipped, with stock logical replication, no extension and no superuser
([setup](https://janbjorge.github.io/straum/postgresql.html)). To try
a spec or measure Straum with no PostgreSQL, `straum run --feed` takes
its tables from Parquet files through `straum-parquet`
([feed](https://janbjorge.github.io/straum/feed.html)); nothing is
followed then.

## How it works

![Straum between PostgreSQL and the apps that ask it](docs/src/diagrams/overview.svg)

One writer loads every table from one snapshot and applies every
commit after it, and asks read meanwhile. The guide's [How Straum
works](https://janbjorge.github.io/straum/how-it-works.html) is the
model; [Design](https://janbjorge.github.io/straum/design.html) is
the same model with the code's names, for contributors.

## Development

[AGENTS.md](AGENTS.md) has the commands and the rules. PostgreSQL tests
need Docker.

## Documentation

- [User guide](https://janbjorge.github.io/straum/), built from `docs/`;
  [Design](https://janbjorge.github.io/straum/design.html) is the
  design
- [Decisions](docs/adr/)
- [Straum and PostgreSQL](research/POSTGRESQL.md)
- [Joins: history and sources](research/JOINS.md)

## License

[Apache-2.0](LICENSE-APACHE)
