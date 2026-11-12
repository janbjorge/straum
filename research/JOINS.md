# Joins: the data model, the code, and where it comes from (2026-11-11)

Straum aggregates one fact table joined to dim tables. This note holds
what `docs/src/how-it-works.md` leaves to it: the exact semantics of a join at ask
time, what was built before it (M9 to M12), the sources behind the
choice (ADR-0030, 2026-11-11), and the numbers. Tags as in README.md
(R, U, I).

## 1. The data model

```text
PostgreSQL                              Straum, per fact
---------------------------------       ---------------------------------
fact table   --COPY, pgoutput-->        row: Fact::columns()
                                             |
                                             v  Rollup::add
                                        cell key = by slots, vias, bucket
                                                   (Fact::rollup_key)
                                        cell     = rows, sums, extents,
                                                   distinct counts
dim tables   --COPY, pgoutput-->        Projection: key -> [attr codes]
                                                    one per row (Dim::columns())
                                             ^
ask ------------------------------------------+  Store::ask maps each cell's
                                                 via columns through the
                                                 projections as they are now
```

The pieces, their dependencies and where each lives in the code are in
`docs/src/how-it-works.md`; the words are in `docs/src/glossary.md`. What a join adds:
every key an ask can name is a select name of the view, and an
attribute resolves to a lookup through the cell's via columns, so an
ask looks nothing up outside Straum. Dim columns are as of now, always.
The view body is what PostgreSQL computes, and Straum's answer equals
it at every commit, dim changes included (§2).

## 2. Semantics

Ask time, as the view in PostgreSQL: every answer joins the cells to
the dims as they are at the commit the answer reflects.

- A cell contributes once per element of the cross product of its
  refs' match lists. A left ref with no match is one null tuple; an
  inner or semi ref with none drops the cell; an anti ref with one
  drops it. count and sum scale by the multiplicity, min, max and count
  distinct do not, avg cancels. Exact, as PostgreSQL repeats the row.
- A chain reads through the parent's projection: the child's via
  values are the parent row's attribute codes, so a chained ref
  multiplies per parent row.
- The view's `WHERE` and a measure's `FILTER` are tested per join row,
  so a condition on a dim column follows the dim.
- `RIGHT` and `FULL` would add dim rows no fact points at, whose
  `date_bin` is null; Straum keeps null-time rows in a bucket no ask
  reads, so for every ask Straum can answer they are `INNER` and `LEFT
  OUTER`. The walker refuses the spellings and names those.

## 3. What it replaces, and what was not chosen

- **M9, late binding, one kind.** The join column was a key, Straum held
  every dim row and mapped attributes after aggregation, as Kylin's
  derived dimensions do
  ([Kylin cube design](https://kylin.apache.org/docs/3.1.3/howto/howto_optimize_cubes/),
  U). Left joins only, N:1 only, no chains, no `WHERE`.
- **M11, early binding.** Attributes read from held dim rows as each
  fact row came in, so cells collapsed to the attributes' few values.
  Two bindings doubled the spec, the walker, the store and the ask
  errors, and a dim change rewrote nothing.
- **M12, resolved in PostgreSQL (ADR-0013).** No dim rows held; the load
  joined in PostgreSQL and each commit looked its attributes up. 84 MiB
  instead of 897 on the warehouse copy, and a dim change invisible
  until a fact row of it changed and Straum started over.
- **This (ADR-0030).** Late binding always, the projection held beside
  the cells, every join kind, any cardinality, chains and `WHERE`. The
  cost is cells keyed by join ids, which the warehouse copy makes about
  a cell a task (§5). That is the question's cost, not a shortcut's.
- **Not chosen:** a per-dim choice of binding or a flip at run time
  (fitting the solution to the data); looking attributes up at ask time
  against PostgreSQL (asks would wait on it); denormalising the
  attributes onto the fact table (duplication in the database, a
  migration per view); summary tables kept by triggers, `pg_ivm` or
  `pg_cron` (a trigger or a lock on the write path, §4.10).

## 4. Sources

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
   U). Straum's semantics in §2 are these; the dimension table is
   PostgreSQL's.
5. **ClickHouse dictionaries.** The lookup layer with the layouts named
   outright: `hashed` holds the whole dim, `cache` a bounded part, and
   `direct` "has no in-memory storage" and queries the source per
   request; `dictGet` in a materialized view enriches rows at insert, and
   the docs warn against `cache` and `direct` on ingest paths
   ([Dictionary layouts](https://clickhouse.com/docs/reference/statements/create/dictionary/layouts/overview),
   [cache layout](https://clickhouse.com/docs/reference/statements/create/dictionary/layouts/cache),
   U). Straum is `direct`, batched per transaction, which the warehouse
   copy's rate of about two fact changes a second made cheap.
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
   U). Another engine that pays the floor of §4.2.
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

## 5. Numbers

Re-measured on 2026-11-11 after ADR-0030, on the warehouse copy of
`docs/src/sizing.md`: 13,116,878 tasks, 5,690,651 task groups, 851,964
storage locations; the view keyed by organization, live-ness, the
group's category and the location's type.

| | Early binding (M12) | Ask-time binding |
| --- | ---: | ---: |
| Hour cells | 311,969 | 11,932,307 |
| Day cells | 66,107 | 11,930,678 |
| Resident | 84 MiB | 1,998 to 2,378 MiB |
| Ready | 19 s | 10 s |
| Dim rows held | 0 | 6,542,615 |
| Group recategorised | start over, 20 s | 208 ms, no start |
| Locations retyped, one in ten | start over | 1.1 s, no start |
| Groups deleted, one in a hundred | start over | 3.8 s, no start |
| Ask by organization and category, one week | 0.9 ms | 89.2 ms |

The cells follow the join ids, as §4.2 says they must: a group holds a
task or two, so both widths hold about a cell a task, and the day width
collapses nothing. Memory is 24 to 28 times M12's, and an ask over a
week reads that week's hour cells and maps each through the
projections, so it costs 100 times what it did. In
exchange no dim change starts Straum over, and every ask equalled
PostgreSQL after each of the seven changes. The full tables are in
`docs/src/sizing.md`.
