# Straum

**In-memory PostgreSQL aggregates, kept current through logical replication.**

[![CI](https://github.com/janbjorge/straum/actions/workflows/ci.yml/badge.svg)](https://github.com/janbjorge/straum/actions/workflows/ci.yml)

Straum keeps the aggregates a spec declares as cells in memory, loaded
from a snapshot and kept current by every commit after it, and answers
asks about them over HTTP. On a month of New York taxi trips its asks
answer in 150 to 190 µs where PostgreSQL takes 22 to 171 ms, from 40 MiB
([benchmark](https://janbjorge.github.io/straum/benchmark.html)).
PostgreSQL stays the source of truth; Straum keeps nothing on disk.

**Not stable.** The spec, the asks and the flags may change.

## What it looks like

A spec is a view in a small subset of PostgreSQL's `CREATE VIEW`. Straum
never creates it; PostgreSQL would give the same rows.

```sql
CREATE VIEW orders WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', orders.placed_at, timestamptz 'epoch') AS bucket,
       orders.status AS status,
       orders.customer_id AS customer_id,
       customers.country AS country,
       count(*) AS orders,
       sum(orders.amount) AS revenue,
       avg(orders.amount) AS avg_amount
FROM public.orders AS orders
LEFT OUTER JOIN public.customers AS customers ON customers.id = orders.customer_id
GROUP BY 1, 2, 3, 4;
```

`straum plan` prints the SQL a table owner runs once; `straum run` loads
the view and follows it. An ask is a `SELECT` over the view, every value
a parameter:

```sh
curl --json '{
  "query": "SELECT country, orders, revenue FROM orders WHERE status = $1 AND bucket >= $2 AND bucket < $3 GROUP BY country",
  "parameters": ["paid", "2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z"]
}' http://localhost:8080/ask
```

```json
{
  "as_of": "2026-11-09T17:12:41.902318Z",
  "rows": [
    {"country": "NO", "orders": 2, "revenue": 139.99},
    {"country": "SE", "orders": 1, "revenue": 80.50}
  ]
}
```

Every commit made before `as_of` is in the answer. The guide's [quick
start](https://janbjorge.github.io/straum/quick-start.html) runs this
from the `ghcr.io/janbjorge/straum` image against PostgreSQL in Docker,
in eight steps.

Straum is not a SQL engine, a replica, a cache or a durable store. It
answers only what its spec declares. It needs PostgreSQL 17 or newer as
shipped, with stock logical replication, no extension and no superuser
([setup](https://janbjorge.github.io/straum/postgresql.html)).

## How it works

![Straum between PostgreSQL and the apps that ask it](docs/src/diagrams/overview.svg)

One writer loads every table from one snapshot and applies every
commit after it, and asks read meanwhile. The guide's [How Straum
works](https://janbjorge.github.io/straum/how-it-works.html) is the
design: the use cases, the model and what the code holds true.

## Development

[AGENTS.md](AGENTS.md) has the commands and the rules. PostgreSQL tests
need Docker.

## Documentation

- [User guide](https://janbjorge.github.io/straum/), built from `docs/`;
  [How Straum works](https://janbjorge.github.io/straum/how-it-works.html)
  is the design
- [Decisions](docs/adr/)
- [Straum and PostgreSQL](research/ARCHITECTURE.md)
- [Joins: semantics, history and sources](research/JOINS.md)
- [PostgreSQL replication research](research/POSTGRES-REPLICATION.md)
- [Managed PostgreSQL constraints](research/MANAGED-POSTGRES.md)
- [AKS deployment design](research/AKS.md)

## License

[Apache-2.0](LICENSE-APACHE)
