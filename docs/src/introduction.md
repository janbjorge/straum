# Straum

Straum keeps aggregates of PostgreSQL tables in memory and answers
asks about them over HTTP in microseconds.

A dashboard runs the same `GROUP BY` over the same tables all day, and
PostgreSQL reads the rows again each time. Straum reads them once, from
a snapshot, keeps each group as a cell in memory, and follows every
commit through logical replication. Every answer reports how current it
is. You declare what to keep as a view, in a small subset of PostgreSQL's
`CREATE VIEW`:

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

An ask is a `SELECT` over that view, with every value as a parameter:

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

Every commit made before `as_of` is in the answer. An ask never touches
PostgreSQL; its cost follows the number of cells it reads. On a month of
New York taxi trips, the [benchmark](benchmark.md) answers in 150 to
228 µs where PostgreSQL takes 22 to 171 ms, from 40 MiB of memory.

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
