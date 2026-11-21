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

Say the `usage` view above is the `GROUP BY` behind your dashboard, and
it has made the slow log. Trying Straum on it changes nothing in the
database: one command copies the columns the spec reads into Docker,
with a role that needs only `SELECT`, and you compare the ask with your
own SQL ([Try it on your data](your-data.md)). On six months of taxi
trips, trips by borough took PostgreSQL a second and Straum under a
millisecond ([Performance](performance.md#results)).

Running it leaves the database's structure alone. Straum is a sidecar
to PostgreSQL, a process of its own on the replication stream. It adds
no extension, table, trigger or index, so a managed service will do,
and needs no primary key or index on your tables; it does refuse a
partitioned table and a float column ([start
checks](postgresql.md#what-straum-checks-at-start)). The
[setup](postgresql.md#1-run-the-setup-sql) is a few lines of SQL: a
role that only reads, a publication, a setting on each table so updates
and deletes log their old row, and on PostgreSQL 17 and 18 `wal_level =
logical`, a restart where it is not set. The app writes as before; only
the dashboard changes, its query now an ask.

On an ordinary day an app inserts an event. No trigger runs in its
transaction, and milliseconds later Straum adds the event to three
cells: its hour, its day and its week. The dashboard sends the ask
above, then a month by day, then the median amount of one account. All
are asks of the same view, answered from cells that merge ([Aggregates
101](aggregates.md)), and none is a query PostgreSQL runs. Support
corrects the amount of an event from March: Straum changes that event's
three cells, the same work as for one from today. An account moves from
`eu` to `us`, and the next answer counts its events under `us`
([Following](how-it-works.md#following)). No refresh runs for either.

Weeks later a second view over the same tables is an edit to the spec
and a restart. PostgreSQL's share stays one replication slot, one
stream and one read of each table at a start ([What Straum costs the
server](postgresql.md#what-straum-costs-the-server)), and the summary
is never a table there to vacuum or back up. Stop Straum for good and
its slot goes with it, leaving the setup's few lines to undo.

Two extensions keep such a summary inside PostgreSQL instead.
[pg_ivm](https://github.com/sraoss/pg_ivm) changes it in the writer's
transaction, so it is never behind and each write waits for it.
TimescaleDB's [continuous
aggregates](https://www.tigerdata.com/docs/use-timescale/latest/continuous-aggregates/about-continuous-aggregates)
change it when a refresh runs, over a table TimescaleDB partitions by
time. Either leaves a table that any SQL can read and join, which
Straum does not. Choose pg_ivm when the summary must be right inside
the transaction, a continuous aggregate when the data is already in
TimescaleDB, and Straum when the database should stay as it is, the
work should leave it and an answer may be [milliseconds
behind](performance.md#transactions).

## What it is not

Straum is not a SQL engine, a replica, a cache or a durable store. It
answers only what its spec declares, so it has no ad hoc queries, keeps
no table rows and uses [no
sketch](aggregates.md#median-and-percentiles). It keeps nothing on disk:
every start loads every table again, and a crash leaves no slot behind
holding write-ahead log (WAL). PostgreSQL stays the source of truth.

Straum supports PostgreSQL 17 and newer, and connects as a role that is
not a superuser. The name is Nynorsk for a stream or current: Straum's
input is PostgreSQL's replication stream.

> [!WARNING]
> Straum is not stable yet. The spec, the asks and the flags may change.

[llms.txt](llms.txt) lists these pages for an agent.

Next: the [quick start](quick-start.md) runs Straum in Docker with one
command on a month of New York taxi trips and leaves you in its shell.
After it, you write a [spec](spec.md) and an [ask](asks.md).
