# Straum

**In-memory PostgreSQL aggregates, kept current as PostgreSQL commits.**

[![CI](https://github.com/janbjorge/straum/actions/workflows/ci.yml/badge.svg)](https://github.com/janbjorge/straum/actions/workflows/ci.yml)

Straum keeps the aggregates a spec declares as cells in memory, loaded
from a snapshot and kept current by every commit after it, and answers
asks about them over HTTP. In the
[benchmark](https://janbjorge.github.io/straum/benchmark.html) its counts
and sums answer in 166 to 413 µs where PostgreSQL takes 22.3 to 2,376.2
ms, and a median fare in 32 ms where it takes 6.6 s, from 113 MiB.
The target is under 1 ms for every ask. The two percentile asks over
every trip miss it. PostgreSQL stays the source of truth; Straum
keeps nothing on disk. A crash leaves no slot behind holding WAL.

**Not stable.** The spec, the asks and the flags may change.

## What it looks like

A spec is a view in a small subset of PostgreSQL's `CREATE VIEW`, here
over an `events` table and the `accounts` it belongs to. Straum never
creates it; PostgreSQL would give the same rows.

```sql
CREATE VIEW usage WITH (buckets = '1 hour, 1 day, 7 days') AS
SELECT date_bin('1 hour', e.at, timestamptz 'epoch') AS bucket,
       e.kind AS kind,
       e.account_id AS account_id,
       a.region AS region,
       count(*) AS events,
       sum(e.amount) AS amount,
       avg(e.amount) AS avg_amount
FROM public.events AS e
LEFT OUTER JOIN public.accounts AS a ON a.id = e.account_id
GROUP BY 1, 2, 3, 4;
```

`straum plan` prints the SQL a table owner runs once; `straum run` loads
the view and follows it. An ask is a `SELECT` over the view, every value
a parameter, here purchases by region on one day:

```sh
curl --json '{
  "query": "SELECT region, events, amount FROM usage WHERE kind = $1 AND bucket >= $2 AND bucket < $3 GROUP BY region",
  "parameters": ["purchase", "2026-11-13T00:00:00Z", "2026-11-14T00:00:00Z"]
}' http://localhost:8080/ask
```

```json
{
  "as_of": "2026-11-14T09:12:40.118204573Z",
  "rows": [
    {"region": "eu", "events": 412, "amount": 18240.50},
    {"region": "us", "events": 655, "amount": 30117.25}
  ]
}
```

Every commit made before `as_of` is in the answer. The guide's [quick
start](https://janbjorge.github.io/straum/quick-start.html) runs Straum
from the `ghcr.io/janbjorge/straum` image against PostgreSQL in Docker,
in four steps, on a month of New York taxi trips.

Straum is not a SQL engine, a replica, a cache or a durable store. It
answers only what its spec declares. It needs PostgreSQL 17 or newer as
shipped, with no extension and no superuser
([setup](https://janbjorge.github.io/straum/postgresql.html)). The
logical stream uses stock logical replication. Where
`wal_level = logical` can't be had, `--wal physical` reads the WAL
`replica` writes instead.

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

- [User guide](https://janbjorge.github.io/straum/), built from `docs/`
- [Decisions](docs/adr/)
- [Straum and PostgreSQL](research/POSTGRESQL.md)
- [Joins: history and sources](research/JOINS.md)
- [Aggregates: merge and delete](research/AGGREGATES.md)

## License

[Apache-2.0](LICENSE-APACHE)
