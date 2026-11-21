# Straum

Straum keeps aggregates of PostgreSQL tables in memory and answers
asks about them over HTTP, counts and sums in microseconds.

A dashboard runs the same `GROUP BY` over the same tables all day, and
PostgreSQL reads the rows again each time. Straum reads them once,
keeps each group as a cell, and follows every commit. A spec is a file
of views, each in a small subset of PostgreSQL's `CREATE VIEW`:

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

An ask is a `SELECT` over a view, every value a parameter:

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

Every commit made before `as_of` is in the answer, and an ask never
touches PostgreSQL. [Performance](performance.md) times the same asks
against PostgreSQL, over the raw tables and over a materialized view
with its refresh.

![Straum between PostgreSQL and the apps that ask it](diagrams/overview.svg)

*A table owner runs the SQL `straum plan` prints, once. `straum run`
loads the tables and follows them. Apps ask over HTTP.*

## What it is good for

Straum suits a question asked often about changing tables, answered
by counts, sums, averages, minima, maxima or percentiles by a few keys
over time ([Measures](spec.md#measures)): dashboard tiles and charts
such as events per hour or amounts by region, the same `GROUP BY` on
every page load.

It suits numbers an app shows its own users, such as usage this month or
orders today. Make the account a key, and your backend puts the
signed-in user's account in each ask. Straum checks no one: anyone who
reaches it can ask for any account ([Who can
ask](deploy.md#who-can-ask)).

It replaces a summary you maintain by hand today, such as a materialized
view refreshed on a schedule or a rollup table kept by triggers. Straum
follows inserts, updates and deletes itself, and every answer is exact.

## What you gain

Straum is a sidecar to PostgreSQL: a process of its own that loads the
tables once, then follows the replication stream. The work of a summary
moves there with it. Follow the `usage` view above through a day.

An app inserts an event. No trigger runs in its transaction, so the
insert costs what it did before Straum. Milliseconds later Straum reads
the commit from the stream and adds the event to three cells: its hour,
its day and its week.

The dashboard sends the ask above, purchases by region for a day.
Straum answers from the cells, so that `GROUP BY` is no longer a query
PostgreSQL runs. Someone then picks a month by day, and after it the
median amount of one account. Each is another ask of the same view. A
cell keeps counts and sums that merge ([Aggregates 101](aggregates.md)),
so one view answers any range of its buckets, by any of its keys, a row
per hour, day or week, with the median still exact ([the
query](asks.md#the-query)).

In the afternoon support corrects the amount of an event from March.
Straum takes the old amount out of that event's three cells and puts
the new one in, the same work as for an event from today. An account
moves from `eu` to `us`: one row changes in what Straum keeps of
`accounts`, and the next answer counts its events under `us`
([Following](how-it-works.md#following)). Neither waits for a refresh,
and nothing reads the tables again.

All day PostgreSQL holds a replication slot and serves the stream, and
it is read once at each start ([What Straum costs the
server](postgresql.md#what-straum-costs-the-server)). That does not
grow with the views, since Straum reads each table once however many
views read it: a second view over the same tables is an edit to the
spec and a restart. Nor is the summary a table in PostgreSQL, so
nothing of it is written, vacuumed or backed up there.

Two extensions keep such a summary inside PostgreSQL instead.
[pg_ivm](https://github.com/sraoss/pg_ivm) changes it in the writer's
transaction, so it is never behind and each write waits for it.
TimescaleDB's [continuous
aggregates](https://www.tigerdata.com/docs/use-timescale/latest/continuous-aggregates/about-continuous-aggregates)
change it when a refresh runs, over a table TimescaleDB partitions by
time. Either leaves a table that any SQL can read and join, which
Straum does not, and reading it is a query on the database. Choose
pg_ivm when the summary must be right inside the transaction, a
continuous aggregate when the data is already in TimescaleDB, and
Straum when the work should leave the database and an answer may be
[milliseconds behind](performance.md#transactions). The quotes behind
this are in
[ALTERNATIVES.md](https://github.com/janbjorge/straum/blob/main/research/ALTERNATIVES.md).

## What it is not

Straum is not a SQL engine, a replica, a cache or a durable store. It
answers only what its spec declares, so it has no ad hoc queries, keeps
no table rows and uses [no
sketch](aggregates.md#median-and-percentiles). It keeps nothing on disk:
every start loads every table again, and a crash leaves no slot behind
holding write-ahead log (WAL). PostgreSQL stays the source of truth.

Straum supports PostgreSQL 17 and newer as shipped, with no extension
and a role that is not a superuser, so managed services work. It reads a
publication, or with `--wal physical` the WAL the server already writes
([setup](postgresql.md)). The name is Nynorsk for a stream or current:
Straum's input is PostgreSQL's replication stream.

> [!WARNING]
> Straum is not stable yet. The spec, the asks and the flags may change.

[llms.txt](llms.txt) lists these pages for an agent.

Next: the [quick start](quick-start.md) runs Straum in Docker with one
command on a month of New York taxi trips and leaves you in its shell.
After it, you write a [spec](spec.md) and an [ask](asks.md).
