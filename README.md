# Straum

**Live, exact aggregates over PostgreSQL in microseconds: a
materialized view that is never stale.**

[![CI](https://github.com/janbjorge/straum/actions/workflows/ci.yml/badge.svg)](https://github.com/janbjorge/straum/actions/workflows/ci.yml)
[![Docs](https://img.shields.io/badge/docs-user%20guide-blue)](https://janbjorge.github.io/straum/)

A dashboard runs the same `GROUP BY` over the same tables all day, and
PostgreSQL reads every row again each time. Straum reads them once,
keeps each group in memory, and follows every insert, update and delete
from PostgreSQL's replication stream. Apps ask over HTTP and never
touch the database.

It takes the place of a materialized view refreshed on a schedule or a
rollup table kept by triggers, and needs no extension or superuser.
PostgreSQL stays the source of truth.

## How fast

Six months of New York taxi trips, 20,332,093 rows, the same ask to
both, answers checked equal
([all sixteen asks](https://janbjorge.github.io/straum/performance.html)):

| Ask | PostgreSQL | Straum | Faster |
| --- | ---: | ---: | ---: |
| Trips by pickup borough | 1,058.8 ms | 269 µs | 3,938x |
| Tips by dropoff borough | 1,746.0 ms | 225 µs | 7,754x |
| Fares per week | 2,154.9 ms | 233 µs | 9,260x |
| Median fare | 4,839.8 ms | 25.2 ms | 192x |

Those times are PostgreSQL reading every trip. A materialized view of
the same rollup answers in 903 µs to 68.3 ms, but only as of its last
refresh, which reads every trip again and took 6,934.4 to 10,819.7 ms
here, and it has no answer for the median, percentile, mode and
distinct asks ([against a materialized
view](https://janbjorge.github.io/straum/performance.html#against-a-materialized-view)).

One run on an Apple M2 Pro with 16 GB, on 2026-11-19. Straum held
305 MiB, and 202 MiB on 2026-11-20 once the distinct fares read the
percentiles' counts; the table and its indexes take 2,475 MiB in
PostgreSQL.

## Try it

One command starts PostgreSQL, loads a month of taxi trips, starts
Straum and opens its shell
([quick start](https://janbjorge.github.io/straum/quick-start.html)).
Needs Docker and bash or zsh.

```bash
docker compose -f <(curl -fsSL https://raw.githubusercontent.com/janbjorge/straum/main/docs/compose/quick-start.yaml) \
  run --rm shell
```

## What you write

A spec is a view, in a small subset of PostgreSQL's `CREATE VIEW`.
Straum never creates it. `buckets(...)` names the time column and each
bucket width to keep.

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

`straum plan` prints the SQL a table owner runs once; `straum run`
loads the view and follows it. An ask is a `SELECT` over the view,
every value a parameter:

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

Every commit made before `as_of` is in the answer.

## What you get

Every answer is exact, percentiles included. A spec can use `count`,
`sum`, `avg`, `min`, `max`, variance and standard deviation,
percentiles, `mode`, distinct counts and `FILTER`
([aggregates](https://janbjorge.github.io/straum/aggregates.html)). It
can join lookup tables and keep hourly, daily and weekly buckets side
by side.

Straum runs against PostgreSQL 17 or newer as shipped. It needs a role
with `SELECT` and a publication, so managed services work. `--wal
physical` reads the write-ahead log (WAL) a server you run already
writes; RDS, Aurora and Cloud SQL do not hand out that stream
([setup](https://janbjorge.github.io/straum/postgresql.html)).

Straum keeps nothing on disk: every start loads the tables again, and a
crash leaves no slot behind holding WAL. It is not a SQL engine, a
replica or a cache, and answers only what its spec declares.

> [!WARNING]
> Straum is not stable yet. The spec, the asks and the flags may change.

## Documentation

The [user guide](https://janbjorge.github.io/straum/) is built from
`docs/`, and [llms.txt](https://janbjorge.github.io/straum/llms.txt)
indexes it for agents. Decisions are in [docs/adr/](docs/adr/).
Research: [Straum and PostgreSQL](research/POSTGRESQL.md),
[Joins](research/JOINS.md), [Aggregates](research/AGGREGATES.md),
[User-defined aggregates](research/UDAF.md). What is not built yet is
in [ROADMAP.md](ROADMAP.md).

## Development

[AGENTS.md](AGENTS.md) has the commands and the rules. PostgreSQL tests
need Docker. `bench/demo.py` loads January's trips on your machine,
times trips by pickup borough, then inserts a trip, deletes it, and
moves two zones into a borough of their own. It needs Docker and
[uv](https://docs.astral.sh/uv/).

## License

[Apache-2.0](LICENSE-APACHE)
