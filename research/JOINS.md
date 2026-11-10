# Joins: the data model, the code, and where it comes from (2026-11-10)

Straum aggregates one fact table joined N:1 to dim tables. This note says
what a join is in Straum's data, which code does what, how the join runs
in PostgreSQL, what that means for answers, what was built before it (M9
to M11), and the sources behind the choice (M12, PLAN.md decision of
2026-11-10). Tags as in README.md (R, U, I).

## 1. The data model

```text
PostgreSQL                                 Straum, per fact
------------------------------------       ---------------------------------
fact table   --COPY, pgoutput------>       row:  columns          attrs
dim tables   --SELECT ... WHERE key IN-->        [c1 .. cn]  ++  [a1 .. am]
                                                 Fact::columns   Fact::attrs
                                                      |
                                                      v  Rollup::add
                                           cell key = by slots, attrs, bucket
                                                      (Fact::rollup_key)
                                           cell     = rows, sums, extents,
                                                      distinct counts
```

- A **fact** is one view: a table, a time column, by columns, refs and
  measures (`Fact`, `crates/straum-domain/src/spec.rs`).
- A **ref** is an N:1 pointer from fact columns (`via`) to a dim's key,
  with the dim columns the view selects through it (`attrs`). A ref has no
  value of its own.
- A **dim** is a table with a key of one column or more and the union of
  the attributes every view reads of it (`Dim`). Straum keeps none of its
  rows.
- A **row** of a fact, as the source hands it over, holds
  `Fact::columns()` (time, by columns, via columns, measure inputs, each
  once) and then `Fact::attrs()`, the attributes of every ref in ref order.
  The source resolves the attributes; the domain never sees a dim.
- A **cell** is keyed by the by slots, then the attributes, then the time
  bucket (`Fact::rollup_key`). Attributes are rollup-key columns like any
  other: values coded once, cells keyed by codes.

Invariants:

- Every key an ask can name is a slot of the cell key (`Spec::resolve`
  gives `Key::By` or `Key::Attr`), so an ask looks nothing up. The ref
  itself is not a key (`AskError::RefAsKey`), and an attribute the view
  does not select is not either (`AskError::AttrNotRead`).
- A row's attributes are what the dims said when the row came in. A dim
  change rewrites nothing. Straum follows fact tables only; the dims are
  read, never published.
- The view body is what PostgreSQL computes. Straum's answers equal it as
  long as no dim row changed between a fact row's arrival and the ask;
  after a dim change they differ for the rows from before, until Straum
  starts over (§3d).

## 2. The code model

| Piece | Where | What |
| --- | --- | --- |
| `Ref`, `Dim`, `Fact::columns`, `Fact::attrs`, `Fact::rollup_key` | `crates/straum-domain/src/spec.rs` | The declarations and the row and key layouts. `Spec::new` refuses a ref that reads nothing (`RefNoAttrs`), reads a column its dim lacks (`UnknownAttr`), joins on a column twice (`ViaReused`) or on another number of columns than the key has (`KeyWidth`). |
| `Rollup` | `crates/straum-domain/src/rollup.rs` | Rows of `columns.len() + attrs` values; `Coded::Value(columns.len() + i)` for the i-th attribute; `set_types` over the whole row. |
| `Store` | `crates/straum-domain/src/store.rs` | The rollups, and nothing else. `slot()` maps a key path to its cell-key slot. |
| `Spec::check`, `AskError` | `crates/straum-domain/src/ask.rs` | `RefAsKey` and `AttrNotRead` say why `g` or `g.id` is no key. |
| `Walker::join`, `add_dim`, `select` | `crates/straum-notation/src/sql/view.rs` | `LEFT JOIN d ON d.k = f.v AND ...` makes a `JoinedRef`; a dim is its table, keyed in the first view's order; the selected dim columns become `attrs`; a join that selects nothing and a FILTER on an unselected attribute are errors; a dim key column selected through the ref is the fact column joined to it (`Column::Fact`). |
| `Source`, `Change` | `crates/straum-app/src/source.rs` | `load_fact` hands over whole rows; `Change` carries fact rows only, old and new whole. |
| `State::apply` | `crates/straum-app/src/app.rs` | Facts only; a change to any other table is skipped. |
| `Tables`, `Access`, `check_dim` | `crates/straum-pg/src/catalog.rs` | Facts are `Streamed` (publication, `REPLICA IDENTITY FULL`, no row filter); dims are `LookedUp` (SELECT privilege, a primary key or unique index on exactly the key). |
| `copy_sql`, `fact_types` | `crates/straum-pg/src/load.rs` | The load's `COPY (SELECT ... LEFT JOIN ...)` and the row's types, the attributes' from the dim columns. |
| `Resolver`, `Lookup` | `crates/straum-pg/src/resolve.rs` | Per commit, per fact, per ref: one `SELECT key, attrs FROM dim WHERE (key) IN (...)`, 1,000 keys a statement; appends attributes or nulls. |
| `Pg::next_commit`, `Pg.side` | `crates/straum-pg/src/source.rs` | Resolves a commit on the side connection before handing it to the writer; the connection goes on error. |
| `Decoder` | `crates/straum-pg/src/decode.rs` | Maps fact relations only; a dim relation maps to nothing. |
| `plan` | `crates/straum-pg/src/plan.rs` | Publishes the fact tables; says what a dim's schema needs in a comment. |
| Fake source | `crates/straum-app/tests/fake/mod.rs` | Resolves refs against the tables at the commit's LSN and drops dim changes, as PostgreSQL would. |
| PostgreSQL tests | `crates/straum-pg/src/tests.rs` | `attributes_are_as_of_ingest`, `loads_copy_the_fact_joined_to_its_dims`, `a_value_left_out_comes_from_the_old_row`, `follows_the_task_tables`. |
| End to end | `crates/straum/tests/straum.rs` | A zone renamed shows once a trip in it changes and Straum starts over. |

## 3. How a join runs

### 3a. The load

One binary `COPY` per fact, the fact joined to its dims in PostgreSQL
(`load::copy_sql`). For the tasks fixture of the tests:

```sql
COPY (SELECT t0."created", t0."organization_id", t0."taskgroup_id",
             t0."location_id", t1."category", t2."location_type"
        FROM "public"."tasks" AS t0
        LEFT JOIN "public"."taskgroups" AS t1
               ON t1."organization_id" = t0."organization_id"
              AND t1."taskgroup_id" = t0."taskgroup_id"
        LEFT JOIN "public"."locations" AS t2 ON t2."id" = t0."location_id")
  TO STDOUT (FORMAT binary)
```

`t0` is the fact and `t1` on its refs, in ref order, so the attributes
come in `Fact::attrs` order after the columns. PostgreSQL plans the join
as it would for the view, which may hash a dim; Straum groups the rows as
they come and holds no table. On the copy of pio's tables the load went
from 8 s with the dims copied as tables of their own to 19 s with the
join, and memory from 897 MiB to 84 MiB (`docs/src/benchmark.md`).

### 3b. The stream

pgoutput brings fact rows as the publication has them: `Fact::columns`,
the old row whole under `REPLICA IDENTITY FULL`. Before the writer applies
a commit, `Resolver::resolve` gives every fact row its attributes:

1. For each fact with refs, collect the rows in the commit: an insert's
   row, an update's old and new rows, a delete's old row.
2. For each ref, take the via values of every row as a key, skipping keys
   with a null in them (null joins nothing), and run

   ```sql
   SELECT "organization_id", "taskgroup_id", "category"
     FROM "public"."taskgroups"
    WHERE ("organization_id", "taskgroup_id") IN ((27, 1), (27, 2), ...)
   ```

   a thousand keys a statement, on the plain connection that also reads
   the WAL position. Integers print as digits, numerics as decimals in
   their scale, booleans as `true` and `false`, text through
   `escape_literal`.
3. Append the attributes found to each row, or nulls for a key with no
   row.

Then the commit goes under the write lock and applies as before: insert
adds, update removes the old row and adds the new, delete removes. The
lookups run outside the lock, so asks never wait on them.

### 3c. Removal

The old row of an update or delete gets its attributes from the dims as
they are now. If a dim row changed since the row came in, the old row
resolves to another cell key than the one it sits under, the rollup
reports `RowError::Missing`, the writer returns `Error::Unfit`, and
Straum starts over from a new slot, reading every row as the dims are
then. The same path handles a fact row that never fit.

### 3d. Semantics

Processing time, as a stream engine's lookup join: attributes are as of
when the row was processed, not as of the commit, and not as of the ask.

- A dim change is not a change to Straum: it is not in the stream, and
  the cells before it keep what they had. Rows after it read the new
  value. A transaction that touches only unpublished tables sends no
  commit at all (pgoutput skips empty transactions since PostgreSQL 15).
- A fact row whose dim row does not exist resolves to nulls, as the
  view's `LEFT JOIN` gives. A dim row that arrives later changes nothing
  for it.
- Between a commit and its apply, the dims may have moved on; the lookup
  reads them as they are at apply time. A snapshot at the commit's LSN
  would need a slot per commit. Open in PLAN.md.
- PostgreSQL's view regroups on a dim change; Straum's answer catches up
  on the next start over. For a dim that must move history, select the
  join column too and map in the client, or ask PostgreSQL.

## 4. What it replaces, and what was not chosen

- **M9, late binding.** The join column was a key, Straum held every dim
  row (`DimTable`) and mapped attributes after aggregation, as Kylin's
  derived dimensions do
  ([Kylin cube design](https://kylin.apache.org/docs/3.1.3/howto/howto_optimize_cubes/),
  U). A dim change moved answers at once. The cost was cells: on pio's
  shape a task group holds one task or two, so the rollup held nearly a
  cell a task, 12 million a width.
- **M11, early binding.** Attributes read from the held dim rows as each
  fact row came in, so cells collapsed to the attributes' few values. The
  cost was the held dims: 6.5 million rows, 875 of 897 MiB on the copy of
  pio's tables. Two bindings doubled the spec, the walker, the store and
  the ask errors.
- **M12, this.** No dim rows held; one binding; the join runs where the
  dims live. Chosen under jb's constraints of 2026-11-10: no data
  duplication, PostgreSQL the single source of truth, pio a reference case
  and not the model.
- **Not chosen:** looking attributes up at ask time against PostgreSQL
  (the cells would still be keyed by the join column, and asks would wait
  on PostgreSQL); denormalising the attributes onto the fact table in
  PostgreSQL (duplication in the database, and a migration per view);
  summary tables kept by triggers, `pg_ivm` or `pg_cron` (no Straum at
  all; they put a trigger or a lock on the write path, §5.10). A bounded
  cache of recent keys in front of the lookup stays open as the lever if
  a source writes faster than the lookups keep up (§5.8).

## 5. Sources

The research of 2026-11-10 that settled the design. Each entry says what
the source says and what Straum took from it.

1. **The delta rule.** Maintaining a join incrementally needs the other
   side: ∆(a ⋈ b) = a ⋈ ∆b + ∆a ⋈ b + ∆a ⋈ ∆b, so "the join operator needs
   to know not only the recent changes (∆a and ∆b), but also it has to
   keep around both entire collections a and b" (Feldera, [Turns out we
   didn't need that second
   index](https://www.feldera.com/blog/turns-out-we-didnt-need-that-second-index),
   U; the rule is Gupta and Mumick's, 1995). Any engine that joins
   outside the database holds the dims. Straum's held copies of M9 and
   M11 were this, not a mistake.
2. **The floor.** Quass, Gupta, Mumick and Widom, "Making views
   self-maintainable for data warehousing", PDIS 1996
   ([PDF](http://ilpubs.stanford.edu:8090/155/1/1996-29.pdf), U): with key
   and foreign-key constraints, a select-project-join view over a fact and
   its dims is maintainable without the source given auxiliary views,
   which are the dims projected to their key and the referenced columns.
   That projection is the least an external maintainer can hold. Under
   "hold nothing", the only move left is to read it from the database.
3. **Flink lookup joins.** A lookup join reads the external table per
   probe row: "correlate rows to the latest version of a key in an
   external versioned table", and for the processing-time form "old rows
   are not stored in state"
   ([Joins](https://nightlies.apache.org/flink/flink-docs-release-2.3/docs/sql/reference/queries/joins/),
   R). Connectors cache with `lookup.cache` = `NONE`, `PARTIAL` or `FULL`
   (FLIP-221;
   [JDBC connector](https://nightlies.apache.org/flink/flink-docs-stable/docs/connectors/table/jdbc/),
   U). Straum is `NONE`, with one statement per ref and transaction
   instead of one per row; `PARTIAL` is the bounded cache left open.
4. **RisingWave process-time temporal joins.** `JOIN ... FOR SYSTEM_TIME
   AS OF PROCTIME()`: "the stream side maintains no state at all. The
   dimension table is the state"
   ([blog](https://risingwave.com/blog/risingwave-process-time-temporal-joins/),
   U); it "does not require RisingWave to maintain the join state, making
   it suitable for scenarios where the dimension table is not updated, or
   where updates to the dimension table do not affect the previously
   joined results" ([Joins](https://docs.risingwave.com/processing/sql/joins),
   U). Straum's semantics in §3d are these; the dimension table is
   PostgreSQL's.
5. **ClickHouse dictionaries.** The lookup layer with the layouts named
   outright: `hashed` holds the whole dim, `cache` a bounded part, and
   `direct` "has no in-memory storage" and queries the source per
   request; `dictGet` in a materialized view enriches rows at insert, and
   the docs warn against `cache` and `direct` on ingest paths
   ([Dictionary layouts](https://clickhouse.com/docs/reference/statements/create/dictionary/layouts/overview),
   [cache layout](https://clickhouse.com/docs/reference/statements/create/dictionary/layouts/cache),
   U). Straum is `direct`, batched per transaction, which the pio rate of
   about two fact changes a second makes cheap.
6. **Druid lookups.** "Lookups do not have history. They always use the
   current data"; ingestion-time lookups freeze the value in the row, and
   query-time lookups exist only for the case where a change must show on
   already-ingested rows
   ([Schema design](https://druid.apache.org/docs/latest/ingestion/schema-design/),
   [Lookups](https://github.com/a0x8o/druid/blob/master/docs/querying/lookups.md),
   U). Straum's attributes are ingestion-time lookups; the M9 late path
   was the query-time kind.
7. **Kimball's surrogate key pipeline.** "The ETL process for aligning
   the dimension tables with fact tables at load time is called the
   surrogate key pipeline", and type 1 against type 2 slowly changing
   dimensions is the choice between the current value and the value as it
   was ([SCD part 2](https://www.kimballgroup.com/2008/09/slowly-changing-dimensions-part-2/),
   U). M9 answered as type 1; M11 and M12 answer as type 2 without the
   history rows, which a start over rewrites to type 1.
8. **Noria and ReadySet partial state.** Hold the dim rows the queries
   have needed, evict under memory pressure, upquery the base table for
   the rest ([Noria, OSDI
   2018](https://pdos.csail.mit.edu/papers/noria:osdi18.pdf), U;
   [ReadySet deep caching](https://readyset.io/docs/concepts/deep-caching),
   U). The shape a bounded key cache in front of `Resolver` would take.
9. **Materialize arrangements.** Join inputs are arranged in memory,
   "memory proportional to the input sizes"; delta joins only avoid the
   intermediate state ([Delta joins](https://materialize.com/blog/delta-joins/),
   U). Another engine that pays the floor of §5.2.
10. **In-database alternatives for the reference case.** `pg_ivm` 1.13
    maintains a view in `AFTER` triggers per statement under an
    `ExclusiveLock` on it, "is not effective when a base table is modified
    frequently", and takes no aggregates over outer joins
    ([README](https://github.com/sraoss/pg_ivm), R); it is available on
    Azure Database for PostgreSQL flexible server but not installed on the
    reference database. TimescaleDB continuous aggregates are Community
    licensed ([editions](https://www.tigerdata.com/docs/about/latest/timescaledb-editions),
    R), and Azure ships the Apache-2 edition
    ([extension considerations](https://learn.microsoft.com/en-us/azure/postgresql/extensions/concepts-extensions-considerations),
    R). Both solve one database's aggregates on its write path; Straum
    costs the write path nothing and serves many views, which is the bar
    it has to clear.
11. **Event-carried state transfer.** Consumers keep their own copy of
    the data so they "never need to contact the source system"; "an
    obvious down-side of this pattern is that there's lots of data
    schlepped around and lots of copies"
    ([Fowler](https://martinfowler.com/articles/201701-event-driven.html),
    R). The copies are what jb ruled out, and the held dims of M9 to M11
    were one.

## 6. Numbers

The copy of pio's tables, release build, laptop Docker, 2026-11-10: ready
in 19 s, 84 MiB resident; 311,969 hour cells and 66,107 day cells;
streamed transactions of 1,000, 19,024, 1,873 and 7,426 rows applied in
74 ms, 3.4 s, 156 ms and 275 ms, every ask equal to PostgreSQL after
each; a task group recategorised showed once a task of it changed and
Straum started over. The taxi month: load 2.1 s, 200 commits 4.1 s, start
over 2.6 s. Details in `docs/src/benchmark.md`.
