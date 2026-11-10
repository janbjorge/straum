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
CREATE VIEW orders AS
SELECT date_bin('1 hour', placed_at, timestamptz 'epoch') AS hour,
       status,
       customer_id,
       customers.country,
       count(*) AS orders,
       sum(amount) AS revenue,
       avg(amount) AS avg_amount
FROM orders
LEFT JOIN customers ON customers.id = customer_id
GROUP BY 1, 2, 3, 4;
```

`straum plan` prints the SQL a table owner runs once; `straum run` loads
the view and follows it. An ask is a `SELECT` over the view, every value
a parameter:

```sh
curl --json '{
  "query": "SELECT customers.country, orders, revenue FROM orders WHERE status = $1 GROUP BY customers.country",
  "parameters": ["paid"]
}' http://localhost:8080/ask
```

```json
{
  "as_of": "2026-11-09T17:12:41.902318Z",
  "rows": [
    {"customers.country": "NO", "orders": 2, "revenue": 139.99},
    {"customers.country": "SE", "orders": 1, "revenue": 80.50}
  ]
}
```

Every commit made before `as_of` is in the answer. The guide's [quick
start](https://janbjorge.github.io/straum/quick-start.html) runs this
against PostgreSQL in Docker in eight steps.

Straum is not a SQL engine, a replica, a cache or a durable store. It
answers only what its spec declares. It needs PostgreSQL 17 or newer as
shipped, with stock logical replication, no extension and no superuser
([setup](https://janbjorge.github.io/straum/postgresql.html)).

## How it works

![Straum between PostgreSQL and the apps that ask it](docs/src/diagrams/overview.svg)

Every start opens a temporary logical slot, loads each view from the
slot's snapshot at LSN L, then applies every commit after L. No commit is
missed between the load and the stream, and none is applied twice.

## Development

[AGENTS.md](AGENTS.md) has the commands and the rules. PostgreSQL tests
need Docker.

## Documentation

- [User guide](https://janbjorge.github.io/straum/), built from `docs/`
- [Decisions](docs/adr/)
- [Architecture](research/ARCHITECTURE.md)
- [Joins: data model, code and sources](research/JOINS.md)
- [PostgreSQL replication research](research/POSTGRES-REPLICATION.md)
- [Managed PostgreSQL constraints](research/MANAGED-POSTGRES.md)
- [AKS deployment design](research/AKS.md)

## License

[Apache-2.0](LICENSE-APACHE)
