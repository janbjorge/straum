# Alternatives: pg_ivm and TimescaleDB continuous aggregates (2026-11-20)

Two PostgreSQL extensions keep an aggregate current inside the
database. This note holds what their own documentation says, quoted, so
the guide can set Straum beside them without guessing. The guide's
version is What you gain in `docs/src/introduction.md`. Nothing
here is a decision, and nothing here was run: no line below is a
measurement of ours.

Tags as in README.md (R, U, I). R here: pages read on 2026-11-20
through a fetch tool that quotes them, at most 125 characters a quote,
so a quote can end before its sentence does. A search snippet is U.
Every item the guide rests on was read a second time the same day,
and §4 lists what the guide infers past the quotes. Items that say
"read 2026-11-21", and §6, were read once, on that day. On 2026-11-21
every quote was matched against the raw text of its page, and the
quotes made longer or corrected then come from that read. Tiger Data
had moved its documentation by then; the links are the new paths.

## 1. pg_ivm

Sources, all R: the [README](https://github.com/sraoss/pg_ivm) on
`main`, the [releases](https://github.com/sraoss/pg_ivm/releases) and
the [LICENSE](https://github.com/sraoss/pg_ivm/blob/main/LICENSE).

1. **Version.** 1.17, published 2026-10-18. "The extension supports
   PostgreSQL 13-19." The PostgreSQL License. 1.13 (2025-10-20) added
   outer joins, 1.16 (2026-09-30) PostgreSQL 19. Between them came 1.14
   (2026-03-31) and 1.15 (2026-06-30): four releases in 2026.
2. **Mechanism.** "The `pg_ivm` module provides Incremental View
   Maintenance (IVM) feature for PostgreSQL." It is immediate
   maintenance: "views are updated in the same transaction that its
   base table is modified", "immediately in AFTER triggers when a base
   table is modified". `create_immv`: "A table of the name `immv_name`
   is created and a query specified by `view_definition` is executed",
   and "some triggers are automatically created so that the view's
   contents are immediately updated". The README says nothing of a
   rollback, or of the writer reading the view inside its own
   transaction. That the view commits and rolls back with the write,
   and that the writer's next statement reads the new row, is I: the
   trigger writes a table in that transaction.
3. **Install.** "add it to either shared_preload_libraries or
   session_preload_libraries in postgresql.conf", and `CREATE
   EXTENSION`. From 1.10 its objects are in the `pgivm` schema.
4. **What a view takes.** "Inner joins including self-join are
   supported." "Simple subqueries in `FROM` clause and `EXISTS`
   subqueries in 'WHERE' clause are supported." "Simple CTEs (`WITH`
   queries) are supported." "`DISTINCT` is allowed in IMMV's definition
   queries." "Supported aggregate functions are count, sum, avg, min
   and max." and "user defined aggregates cannot be used." Not taken:
   "window functions, `HAVING`, `ORDER BY`, `LIMIT`/`OFFSET`", set
   operations, `WITH RECURSIVE`.
5. **Outer joins.** "Outer joins are supported with the following
   restrictions." Among them: "Only simple equijoin is supported." and
   "Aggregate functions cannot be used with outer joins."
6. **Base tables.** "The base tables must be simple tables.":
   "inheritance parent tables, partitioned tables, partitions, and
   foreign tables can not be used", nor views and materialized views.
7. **min and max under delete.** When "a tuple containing the current
   minimal or maximal values are deleted from a base table", "the new
   values could be re-calculated from base tables with regard to the
   affected groups", and "it can takes a long time to update an IMMV
   containing these functions."
8. **Floats.** "using `sum` or `avg` on `real` (`float4`) type or
   `double precision` (`float8`) type in IMMV is unsafe", since
   "aggregated values in IMMV can become different from results
   calculated from base tables due to the limited precision of these
   types." Straum refuses the same columns for the same reason
   (ADR-0003).
9. **Concurrent writers.** "`ExclusiveLock` is held on an IMMV
   immediately after a base table is modified in `READ COMMITTED`
   isolation level." The exception is a view of one base table with no
   `DISTINCT` or `GROUP BY` under `INSERT`, which takes
   `RowExclusiveLock`. "Incremental updates of a view are basically
   performed in sequentially even with concurrent transactions
   running." "In `REPEATABLE READ` or `SERIALIZABLE` isolation level,
   an error is raised immediately if lock acquisition fails". The
   README gives the lock no duration in so many words, but it "ensures
   that the IMMV is updated in the latter transaction only after the
   former transaction has committed" (item 13), so under `READ
   COMMITTED` a second writer waits for the first to commit.
10. **When it is not effective.** "Due to the overhead of immediate
    maintenance, IVM is not effective when a base table is modified
    frequently." Also "when a large part of a base table is modified or
    large data is inserted into a base table, IVM is not effective";
    for that, "we can use `refresh_immv` function with `with_data =
    false` to disable immediate maintenance before modifying a base
    table."
11. **Its own example.** `pgbench_accounts` joined to
    `pgbench_branches`, 10,000,000 rows, no aggregate. One `UPDATE`
    of an account: `Time: 9.052 ms` beside a plain materialized view,
    `Time: 15.448 ms` beside the IMMV, and `Time: 3224.741 ms` after
    the IMMV's index is dropped. `REFRESH MATERIALIZED VIEW`: `Time:
    20575.721 ms`. "If there are no indexes, it will take a long time."
    With `GROUP BY`, "a unique index is created on the columns of GROUP
    BY expressions".
12. **Other limits.** "Logical replication is not supported": "IMMVs at
    subscriber nodes defined on these base tables are not updated."
    Under row-level security, "rows that are not visible to the
    materialized view's owner are excluded from the result."

13. **How it maintains.** "only incremental changes are computed and
    applied on views". "IMMVs allow faster updates than `REFRESH
    MATERIALIZED VIEW`" and "at the price of slower updates to their
    base tables." The lock of item 9 "ensures that the IMMV is updated
    in the latter transaction only after the former transaction has
    committed." `refresh_immv` "completely replaces the contents of an
    IMMV". The README names no function that changes a view's query;
    `restore_immv` with `replace` registers an existing table again, an
    option "intended for use with `pg_upgrade`".
14. **What it installs** (read 2026-11-21). The server loads it: "add
    it to either shared_preload_libraries or session_preload_libraries
    in postgresql.conf", then "restart PostgreSQL for the configuration
    to take effect." Each database runs `CREATE EXTENSION`.
    `create_immv` adds a table, "some triggers" and, where it can
    derive one, "a unique index". The view's query is a row of
    `pg_ivm_immv`, and "the contents of `pgivm.pg_ivm_immv` are not
    included in `pg_dump` output": a script the README gives runs
    before `pg_dump` and `pg_upgrade` and "recreates the metadata,
    triggers, and indexes" after.

## 2. TimescaleDB continuous aggregates

Sources, all R unless tagged: Tiger Data's documentation, linked at
each item, and the
[releases](https://github.com/timescale/timescaledb/releases).

1. **Version.** 2.32.1, published 2026-11-19, after 2.32.0
   (2026-11-12), 2.31.0 (2026-10-20) and 2.30.2 (2026-09-29). Its
   release note: "This release is compatible with the latest PostgreSQL
   minor releases, 18.7, 17.12 and 16.16". The range of majors it
   supports was not read anywhere else.
2. **What one is**
   ([about](https://www.tigerdata.com/docs/learn/continuous-aggregates)).
   "Continuous aggregates in TimescaleDB are a kind of hypertable that
   is refreshed automatically in the background". They "take raw data
   from the original hypertable, aggregate it, and store the aggregated
   data". "Because continuous aggregates are based on hypertables, you
   can query them in exactly the same way as your other tables." A
   hypertable is one of the "PostgreSQL tables that
   automatically partition your time-series data by time"
   ([hypertables](https://www.tigerdata.com/docs/learn/hypertables/understand-hypertables)).
3. **The query**
   ([create](https://www.tigerdata.com/docs/build/continuous-aggregates/create-a-continuous-aggregate)).
   "The `GROUP BY` clause must include a `time_bucket` expression which
   uses time dimension column of the hypertable." "Mutable functions
   have experimental support in the continuous aggregate query
   definition." "Window functions are disabled by default." "By
   default, when you create a view for the first time, it is populated
   with data." Under Query continuous aggregates, of the `SELECT` that
   reads the view: "You can only specify a single hypertable in the
   `FROM` clause. Including more hypertables, tables, views, or
   subqueries in your `SELECT` query is not supported." The page does
   not square that with the joins of item 8.
4. **Refresh**
   ([policies](https://www.tigerdata.com/docs/build/continuous-aggregates/refresh-policies)).
   "Refresh policies control how and when a materialized continuous
   aggregate catches up with its source hypertable." `start_offset`
   and `end_offset` bound the window "relative to when the policy
   runs"; `schedule_interval` is "the refresh interval in minutes or
   hours. Defaults to 24 hours." A refresh "recomputes the data within
   the window that has changed in the underlying hypertable since the
   last refresh", and "Only buckets that are wholly within the
   specified range are refreshed." No policy comes with the view:
   "Continuous aggregates require a policy for automatic refreshing."
   By hand: "CALL refresh_continuous_aggregate('example', '2021-05-01',
   '2021-06-01');". The page's own policy example has
   `schedule_interval => INTERVAL '1 h'`. The
   [API page](https://www.tigerdata.com/docs/reference/timescaledb/continuous-aggregates/add_continuous_aggregate_policy)
   marks `schedule_interval` required and gives it a default of "24
   hours" in the same row, without saying how both hold, so the guide
   names neither an interval nor a default. The policy returns the
   "TimescaleDB background job ID created to implement this policy".
5. **Tracking changes** (about). "Any change to the data in a
   hypertable could potentially invalidate some materialized rows."
   "When data older than the invalidation threshold is changed, each
   transaction logs the minimum and maximum timestamps of the rows it
   modified." "This logging does cause some write load. However, the
   threshold lags behind the area of data that is currently changing,
   so the writes are small and rare." A transaction
   that touches 10:00 and 15:00 invalidates every bucket between them;
   two transactions invalidate only the two.
6. **Real-time aggregates**
   ([page](https://www.tigerdata.com/docs/learn/continuous-aggregates/real-time-aggregates)).
   "Real-time aggregates automatically add the most recent data when
   you query your continuous aggregate." "In TimescaleDB v2.13 and
   later, real-time aggregates are **DISABLED** by default."
   "If you add new historical data to an already-materialized bucket,
   it won't be reflected in a real-time aggregate." "You can think of
   real-time aggregates as being eventually consistent for historical
   data." "Real-time aggregates, however, use the aggregated data and
   add the most recent raw data to it."; "they include data more recent
   than your last materialized bucket." For a change in an older
   bucket: "You should wait for the next scheduled refresh, or manually
   refresh by calling `refresh_continuous_aggregate`." What it costs a
   query is not on the page.
7. **Aggregates** (about). "In TimescaleDB v2.7 and later, continuous
   aggregates support all PostgreSQL aggregate functions.", ordered-set
   aggregates, `DISTINCT` and `FILTER` in an aggregate included. The
   create page says less: "You can use most PostgreSQL aggregate
   functions in continuous aggregations." The
   [2.7.0 release](https://github.com/timescale/timescaledb/releases/tag/2.7.0)
   lists "Continuous Aggregates finals form" and "FILTER, DISTINCT,
   ORDER BY as well as Ordered-Set Aggregate"; neither page says why
   one allows the other. That a refresh computing a bucket from its
   rows needs no state that subtracts is I, and so is a float sum being
   among the aggregates taken.
8. **Joins** (about). "You can use an `INNER`, `LEFT`, and `LATERAL`
   joins; no other join type is supported." One hypertable with many
   plain tables from 2.16. "Only the changes to the hypertable are
   tracked, and they are updated in the continuous aggregate when it is
   refreshed." "Changes to standard PostgreSQL table are not tracked."
9. **Stacking** (about). "You can create a continuous aggregate on top
   of another continuous aggregate."
10. **Dropping raw rows**
    ([drop data](https://www.tigerdata.com/docs/build/continuous-aggregates/drop-data)).
    "In many cases, dropping underlying data replaces the aggregate
    with NULL values, which can lead to unexpected results in your
    view." To keep the buckets, "set the `start_offset` value of the
    aggregate policy to a smaller interval than the `drop_after`
    parameter of the retention policy."
11. **Licence**
    ([editions](https://www.tigerdata.com/docs/get-started/choose-your-path/timescaledb-editions)).
    The table's "CREATE MATERIALIZED VIEW (Continuous Aggregate)" row
    is no for the Apache 2 Edition and yes for the Community Edition,
    which is "available under the terms of the Tiger Data License
    (TSL)". "You can install TimescaleDB Community Edition in your own
    on-premises or cloud infrastructure and run it for free." "You
    cannot sell TimescaleDB Community Edition as a service, even if you
    are the main contributor."
12. **Invalidation from the WAL, tried and removed**
    ([2.25.0](https://github.com/timescale/timescaledb/releases/tag/2.25.0),
    2026-01-29). "This release removes the WAL-based invalidation of
    continuous aggregates." "This feature was introduced in 2.22.0 as
    tech preview to use logical decoding for building the invalidation
    logs." "The feature was designed for high ingest workloads,
    reducing the write amplification." "With the upcoming stream of
    improvements to continuous aggregates, this feature was
    deprioritized and removed."

13. **Invalidation and refresh in the source**, `main` on 2026-11-20.
    [invalidation.c](https://github.com/timescale/timescaledb/blob/main/tsl/src/continuous_aggs/invalidation.c):
    "Invalidations track the regions/intervals [start, end] of a
    continuous", "initially written to a hypertable invalidation log
    [hypertable_id, start, end]." An entry holds a time range and no
    key of a row; on `main` both logs have the columns "(id,
    lowest_modified_value, greatest_modified_value, seqnum)", the last
    nullable. "Since a refresh always materializes full buckets, we can
    safely expand an invalidation to bucket boundaries".
    [materialize.c](https://github.com/timescale/timescaledb/blob/main/tsl/src/continuous_aggs/materialize.c)
    then runs "DELETE FROM %s.%s AS D " "WHERE D.%s >= $1 AND D.%s <
    $2 %s;" and "INSERT INTO %s.%s SELECT * FROM %s.%s AS I " "WHERE
    I.%s >= $1 AND I.%s < $2 %s %s;", or with a setting on a `MERGE`
    that rewrites the rows that differ, "WHEN MATCHED AND ROW(M.*) IS
    DISTINCT FROM ROW(P.*)". Either way the range is computed again
    from the view's query. One case on `main` narrows that (read
    2026-11-21): a granular refresh the release notes name from 2.29.0
    ("Initial placeholder version of granular refresh API") and no
    documentation page was found for. With it, "only rows whose
    tenant_column matches a tracked tenant" in the range "are
    re-materialized", the `%s` after `$2` in the `DELETE` and the
    `INSERT` being that predicate, and the `MERGE` is "not for
    tenant-scoped ranges". `seqnum` is its link from a log entry:
    "seqnum 0 means "no associated granular tracking" and is stored as
    SQL NULL."
14. **Stacking, in detail**
    ([hierarchical](https://www.tigerdata.com/docs/learn/continuous-aggregates/hierarchical-continuous-aggregates)).
    One view has one bucket width (I, from item 3): "To get a daily
    summary, you can create a new continuous aggregate on top of your
    hourly aggregate." The upper width "needs to be a multiple of the
    underlying time bucket." The page's link to the refresh page reads
    "Set up automatic refresh policies for each level." "Not
    all calculations return the correct result if you stack them.";
    for those the page points to hyperfunctions "designed with a
    two-step aggregation pattern", where "The first step creates a
    summary aggregate that can be rolled up".
15. **Unique indexes on a hypertable**
    ([page](https://www.tigerdata.com/docs/build/performance-optimization/hypertables-and-unique-indexes)).
    "When you create a unique index, it must contain all the
    partitioning columns of the hypertable."
16. **What it installs, and making a table a hypertable** (read
    2026-11-21). Self-hosted, in the page's steps for a build from
    source
    ([install](https://www.tigerdata.com/docs/get-started/choose-your-path/install-timescaledb)):
    "Open the `postgresql.conf` file and update
    `shared_preload_libraries`", "Restart the PostgreSQL instance",
    then `CREATE EXTENSION IF NOT EXISTS timescaledb;` in each
    database. A table that holds rows
    ([page](https://www.tigerdata.com/docs/learn/hypertables/optimize-data-in-hypertables)):
    "To convert an existing table with data in it, call
    `create_hypertable` on that table", with `migrate_data` true, and
    "if you have a lot of data, this may take a long time." Keys
    ([page](https://www.tigerdata.com/docs/learn/data-model/primary-keys-time-and-uniqueness)):
    "TimescaleDB requires that every unique constraint or primary key
    includes the partitioning column." For a large table that takes
    writes, the
    [tutorial](https://www.tigerdata.com/docs/learn/tutorials/migrate-a-table-to-a-hypertable)
    makes a new hypertable beside the old table, sends new rows to it
    with a trigger, copies the history so that "each batch commits on
    its own, so no single transaction holds locks or builds up
    write-ahead log (WAL) for long", and renames the two. Another
    table's foreign key may point at a hypertable from 2.16.0, whose
    release note says "You can add foreign keys from regular tables to
    hypertables, with support for all types of cascading options."
    (R, read 2026-11-21).

## 3. Where each is offered

Read on 2026-11-20; these lists change.

| Service | pg_ivm | TimescaleDB |
| --- | --- | --- |
| Azure Database for PostgreSQL flexible server | 1.15 on PostgreSQL 13 to 19 (R) | 2.24.0 on 15 to 18, "the TimescaleDB Apache-2 edition" (R), so no continuous aggregates (I, from §2.11) |
| Google Cloud SQL | "PostgreSQL versions 13 and later support version 1.9." (R) | not on the page (R) |
| Amazon RDS | not in the tables for PostgreSQL 16 to 19 (R) | not in those tables (R) |
| Tiger Cloud | not looked up | the Community Edition (R, editions) |

Sources: Azure [by
name](https://learn.microsoft.com/en-us/azure/postgresql/extensions/concepts-extensions-versions)
and
[considerations](https://learn.microsoft.com/en-us/azure/postgresql/extensions/concepts-extensions-considerations),
[Cloud SQL](https://docs.cloud.google.com/sql/docs/postgres/extensions),
[Amazon RDS](https://docs.aws.amazon.com/AmazonRDS/latest/PostgreSQLReleaseNotes/postgresql-extensions.html).
The RDS page was read up to its PostgreSQL 15 table. Aurora, AlloyDB,
Neon and Supabase were not looked up.

## 4. Straum beside them

What the guide says of Straum there has its home elsewhere:

- No extension, PostgreSQL 17 or newer, plain tables only, a hypertable
  refused: `docs/src/postgresql.md`.
- The stream, the old row and `REPLICA IDENTITY FULL`: ADR-0007,
  ADR-0008, POSTGRESQL.md.
- Cells in memory, a load at every start: ADR-0003, ADR-0010.
- Asks over HTTP and no other SQL: ADR-0005.
- A count per value for `min` and `max`: AGGREGATES.md §3.
- Commit to applied, 11.7 ms for 1,000 rows:
  `docs/src/performance.md`, Transactions.

The three differ in where and when the work is done, and in what the
summary holds (I):

| | Does the work | The summary holds |
| --- | --- | --- |
| pg_ivm | In the writer's transaction, in triggers (§1.2) | A table of answers, a row a group |
| Continuous aggregate | In a refresh, which computes each changed time range again (§2.4, §2.13) | A hypertable of answers, one bucket width a view (§2.14) |
| Straum | In its own process, on the stream after the commit | Cells of counts and sums that merge and subtract, several widths a view |

What each asks of the database before its first answer (R: §1.14,
§2.2, §2.4, §2.16, `docs/src/postgresql.md`). On a managed service the
first column is whatever the service offers (§3):

| | In the server | In the database |
| --- | --- | --- |
| pg_ivm | A preloaded library and a restart | `CREATE EXTENSION`; a table, triggers and an index a view |
| Continuous aggregate | A preloaded library and a restart | `CREATE EXTENSION`; the table made a hypertable, its unique keys holding the time column; a materialization hypertable and a refresh policy a view |
| Straum, `--wal logical` | `wal_level = logical`, a restart on 17 and 18 where it is not set | A role, `SELECT`, a publication, `REPLICA IDENTITY FULL` on each spec table |
| Straum, `--wal physical` | Nothing at the defaults | A role, `SELECT`, one `GRANT EXECUTE` |

Why someone would try Straum, each read from Straum's own pages on
2026-11-21; the guide's What you gain tells them in this order, but
for the fifth, which is only here:

- Trying changes nothing in the database: one command copies the
  columns the spec reads into Docker with a role that needs only
  `SELECT`, and the ask is compared with SQL on the copy
  (`your-data.md`).
- The answer: trips by pickup borough took PostgreSQL 1,058.8 ms and
  Straum 269 µs, the answers checked equal (`performance.md`,
  Results).
- The structure stays: "It installs nothing in PostgreSQL"
  (`how-it-works.md`), "The fact table needs no primary key" and "A
  dim needs no index on its key" (`spec.md`); the try-out copy is
  "without indexes, keys or defaults" (`your-data.md`). The other
  side: Straum refuses a partitioned table and a float column a view
  reads (`postgresql.md`, What Straum checks at start).
- The app's writes do not change. Its read does: the dashboard's
  query becomes an ask over HTTP (`asks.md`).
- The view is PostgreSQL's SQL: "Written as `date_bin` instead, each
  view gives the same rows in PostgreSQL" (`spec.md`), so an answer
  can be checked there.
- Leaving: the slot is temporary and goes with the process
  (`postgresql.md`, What Straum costs the server); what the setup SQL
  added is what remains.

What follows for Straum, each with its home in the guide:

- The work leaves the database: no trigger, no refresh, and no query
  for an ask (`how-it-works.md`, Answering an ask: "It never touches
  PostgreSQL"; `postgresql.md`, What Straum costs the server).
- One view answers many asks exactly: any range of its buckets, any of
  its keys, a row per hour, day or week, with averages, percentiles
  and distinct counts merged from the cells (`aggregates.md`, Merging;
  `asks.md`, The query).
- A change costs what it changes: one cell a width for a row inserted
  or deleted and two at most for one updated, whatever its time, and
  one projection row for a dim, which moves cells only where the view
  keeps bound cells (`how-it-works.md`, Following and Dims).
- An answer says how current it is, `as_of`, and a transaction applies
  whole (`asks.md`, `how-it-works.md`).
- A Straum that fails exits and its slot goes with it. One that falls
  behind keeps answering, each answer with its `as_of`, until the WAL
  kept for it passes `--max-retained-wal`; then it exits too
  (`deploy.md`, States; `postgresql.md`).

Smaller ones, read from Straum's own pages on 2026-11-21. The first
two are in the guide's What you gain; the rest are only here:

- The server's standing cost does not grow with the views: one slot
  and one stream, however many views read a table (`postgresql.md`,
  What Straum costs the server). Nor does the load: one `COPY` a dim
  and one a fact table, with the columns of every view that reads it
  (`crates/straum-pg/src/load.rs`, since 2026-11-22), so a second view
  over the same fact table adds its columns to that copy and no read of
  the table. A second view over the same tables is an edit to the spec
  and a restart, with no statement to run in PostgreSQL.
- The summary is not a table in PostgreSQL, so nothing of it is
  written, vacuumed or backed up there (`how-it-works.md`: "It
  installs nothing in PostgreSQL and writes nothing to disk"). In one
  run two hourly materialized views took 245 MiB on disk and Straum
  201 MiB in memory (`performance.md`, Against a materialized view).
- On Kubernetes a new spec loads beside the old one and takes over, so
  asks keep answering while a view changes (`deploy.md`, Kubernetes).
- An ask over a long range reads the widest buckets that fit, so its
  time follows the cells it reads (`how-it-works.md`, Answering an
  ask). Against a plain materialized view of the same hourly rows, the
  eleven count and sum asks took 145 µs to 2.6 ms where the view took
  903 µs to 68.3 ms, on a busy machine (`performance.md`). Neither
  extension was timed (§5).
- A client can wait for its own write by `Straum-LSN` (`asks.md`,
  Seeing your own write), and `/readyz` turns off past `--max-lag`
  (`deploy.md`, States).
- A deleted minimum or maximum lowers a count in its cell, and no row
  is read again (`aggregates.md`, Removing a row; §1.7 has pg_ivm's
  words on the same case).
- Straum keeps no state across a failure, so a restart builds every
  view from the tables again and there is nothing to repair
  (`deploy.md`, States).
- With `--wal physical` no table and no server setting changes: the
  default `wal_level`, a role, `SELECT` and one `GRANT EXECUTE`
  (`postgresql.md`). RDS, Aurora and Cloud SQL do not hand out that
  stream (U: `postgresql.md` says so, and no provider page was read
  for it).
- An app that asks holds no database connection and no database
  password. Straum checks no one, so who reaches it is decided in
  front of it (`deploy.md`, Who can ask).

What follows for the other two, which Straum does not have: pg_ivm's
view is right inside the writing transaction and can hold a join with
no aggregate; a continuous aggregate takes "all PostgreSQL aggregate
functions" by one page and "most" by another (§2.7), is on disk and
can outlive its raw rows. Either is a table any SQL reads and joins
(I, below). Straum holds its views in memory, loads at every start,
answers only its spec and needs PostgreSQL 17 or newer.

What the guide says of the two extensions that no quote above says,
all I:

- With pg_ivm "it is never behind and each write waits for it": the
  trigger runs in the writing statement (§1.2).
- "Either leaves a table that any SQL can read and join": pg_ivm
  creates a table (§1.2), and a continuous aggregate is queried "in
  exactly the same way as your other tables" (§2.2). Neither source
  speaks of joins to it.
- A continuous aggregate fits "when the data is already in
  TimescaleDB": it reads a hypertable (§2.3), which Straum refuses.

## 5. Not looked up

- Either extension running: write cost per row, read time of the
  summary, lock waits under concurrent writers.
- Whether a pg_ivm aggregate takes `FILTER`; the README does not name
  it.
- Which PostgreSQL majors TimescaleDB 2.32 supports, past the minor
  releases its note names (§2.1).
- What real-time aggregation costs a query.
- TimescaleDB's hyperfunctions inside a continuous aggregate
  (AGGREGATES.md §2 has the percentile sketches).
- Engines beside PostgreSQL: Materialize and Feldera are in JOINS.md
  §2 and AGGREGATES.md §2, ReadySet in JOINS.md §2, for their joins
  and aggregates, not as alternatives. §6 reads three home pages for
  their wording only.

## 6. How tools beside the database state their case

Read on 2026-11-21 for the angles a reader weighs, not as
alternatives. Each quote is the vendor's own claim (R); none was
checked.

- [Readyset](https://readyset.io/), a cache in front of the database:
  "Deploy Readyset without modifying your database schema or
  application code"; "No stale reads, no TTLs to tune."; "Scale
  growing workloads without adding read replicas".
- [Epsio](https://www.epsio.io/), which keeps a results table in the
  database from its replication stream: "using the SQL you already
  know in the database you already have."; "Work inside your own
  database, without any migrations"; "Replace complex caching
  solutions and denormalizations with a single SQL command."
- [Materialize](https://materialize.com/): "Offload complex queries
  from your databases"; "reducing load on your source databases,
  improving stability".

The angles repeat: nothing to change in the schema, no stale answer
and no expiry to tune, load taken off the database, SQL the reader
already writes. Straum can say three of itself (§4): no expiry to
tune, load taken off the database, SQL the reader already writes. The
rest it cannot say whole. Its answer "may be milliseconds behind"
(`introduction.md`) and says so in `as_of`; `--wal logical` sets
`REPLICA IDENTITY FULL` on each spec table; and its reads are asks
over HTTP, so the application's read does change.
