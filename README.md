# Straum

**In-memory PostgreSQL aggregates, kept current as PostgreSQL commits.**

[![CI](https://github.com/janbjorge/straum/actions/workflows/ci.yml/badge.svg)](https://github.com/janbjorge/straum/actions/workflows/ci.yml)

Straum keeps the aggregates a spec declares as cells in memory. It loads
them from a snapshot, keeps them current with every commit after it, and
answers asks about them over HTTP in microseconds
([performance](https://janbjorge.github.io/straum/performance.html)).
PostgreSQL stays the source of truth; Straum keeps nothing on disk.

On six months of New York taxi trips, 20,332,093 of them, tips by
dropoff borough take PostgreSQL 1,746.0 ms and Straum 225 µs, 7,754x
faster. One run on an Apple M2 Pro with 16 GB, on 2026-11-19. The
[performance](https://janbjorge.github.io/straum/performance.html) page
has every ask, including the median fare, which takes Straum 25.2 ms.

`bench/demo.py` loads January and times trips by pickup borough on your
machine, then inserts a trip, deletes it, and moves two zones into a
borough of their own. It needs Docker and
[uv](https://docs.astral.sh/uv/). The times it prints are not the
7,754x above.

**Not stable.** The spec, the asks and the flags may change. Straum
needs PostgreSQL 17 or newer, with no extension and no superuser.
`--wal physical` reads the WAL a server you run already writes; RDS,
Aurora and Cloud SQL do not hand out that stream
([setup](https://janbjorge.github.io/straum/postgresql.html#physical-wal)).

## What it looks like

A spec is a view in a small subset of PostgreSQL's `CREATE VIEW`, here
over an `events` table and the `accounts` it belongs to. Straum never
creates it. `buckets(...)` is the view's time: the column, then every
bucket width Straum keeps.

```sql
CREATE VIEW usage AS
SELECT buckets(e.at, '1 hour', '1 day', '7 days') AS bucket,
       e.kind AS kind,
       e.account_id AS account_id,
       a.region AS region,
       (e.device_id IS NOT NULL) AS has_device,
       count(*) AS events,
       sum(e.amount) AS amount,
       avg(e.amount) AS avg_amount,
       max(e.amount) AS top_amount,
       percentile_disc(0.5) WITHIN GROUP (ORDER BY e.amount) AS median_amount,
       percentile_disc(0.99) WITHIN GROUP (ORDER BY e.amount) AS p99_amount,
       count(*) FILTER (WHERE e.kind = 'purchase') AS purchases,
       count(*) FILTER (WHERE e.device_id IS NOT NULL) AS from_devices
FROM public.events AS e
LEFT OUTER JOIN public.accounts AS a ON a.id = e.account_id
GROUP BY 1, 2, 3, 4, 5;
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

Every commit made before `as_of` is in the answer. The
[quick start](https://janbjorge.github.io/straum/quick-start.html) runs
it in Docker with one command and opens the shell.

## Development

[AGENTS.md](AGENTS.md) has the commands and the rules. PostgreSQL tests
need Docker.

## Documentation

The [User guide](https://janbjorge.github.io/straum/) is built from `docs/`.
An [llms.txt](https://janbjorge.github.io/straum/llms.txt) file indexes the
guide for agents. Decisions are in [docs/adr/](docs/adr/). Research on
the same topics is in [Straum and PostgreSQL](research/POSTGRESQL.md),
[Joins: history and sources](research/JOINS.md),
[Aggregates: merge and delete](research/AGGREGATES.md) and
[User-defined aggregates](research/UDAF.md).

## License

[Apache-2.0](LICENSE-APACHE)
