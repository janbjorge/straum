# Straum

Straum keeps aggregates of PostgreSQL tables in memory, current to the
last commit, and answers questions about them over HTTP in microseconds.

A dashboard runs the same `GROUP BY` over the same tables all day, and
PostgreSQL reads the rows again each time. Straum reads them once, from
a snapshot, keeps each group as a cell in memory, and follows every
commit through logical replication, so the cells are always the current
answer. You declare what to keep as a view, in a small subset of
PostgreSQL's `CREATE VIEW`:

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

An ask is a `SELECT` over that view, with every value as a parameter:

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

Every commit made before `as_of` is in the answer. An ask costs in
proportion to the cells it reads and never touches PostgreSQL: on a
month of New York taxi trips, the [benchmark](benchmark.md) answers in
150 to 190 µs where PostgreSQL takes 22 to 171 ms, from 40 MiB of
memory.

![Straum between PostgreSQL and the apps that ask it](diagrams/overview.svg)

*A table owner runs the SQL `straum plan` prints, once. Every start of
`straum run` loads a snapshot at LSN L and applies every commit after L.
Apps ask over HTTP.*

Straum needs PostgreSQL 17 or newer as shipped, with no extension and no
superuser, so managed services work. The name is Nynorsk for a stream
or current: Straum's input is PostgreSQL's replication stream.

> [!WARNING]
> Straum is not stable yet. The spec, the asks and the flags may change.

## What it is not

Straum is not a SQL engine, a replica, a cache or a durable store. It
answers only what its spec declares, and it keeps nothing on disk: every
start loads every table again, and a crash leaves no slot behind holding
WAL. PostgreSQL stays the source of truth.
