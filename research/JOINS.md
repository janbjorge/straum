# Joins: history and sources (2026-11-11)

Straum aggregates one fact table joined to dim tables; how a join
works is in `docs/src/spec.md` and `docs/src/how-it-works.md`. This
note holds what was built before it (M9 to M12), the sources behind
the choice (ADR-0006, 2026-11-11), and the numbers. Tags as in
README.md (R, U, I).

## 1. What it replaces, and what was not chosen

- **M9, late binding, one kind.** The join column was a key, Straum held
  every dim row and mapped attributes after aggregation, as Kylin's
  derived dimensions do
  ([Kylin cube design](https://kylin.apache.org/docs/3.1.3/howto/howto_optimize_cubes/),
  U). Left joins only, N:1 only, no chains, no `WHERE`.
- **M11, early binding.** Attributes read from held dim rows as each
  fact row came in, so cells collapsed to the attributes' few values.
  Two bindings doubled the spec, the walker, the store and the ask
  errors, and a dim change rewrote nothing.
- **M12, resolved in PostgreSQL (ADR-0006, Context).** No dim rows
  held; the load joined in PostgreSQL and each commit looked its
  attributes up. 84 MiB instead of 897 on the warehouse copy, and a dim
  change invisible until a fact row of it changed and Straum started
  over.
- **This (ADR-0006).** Refs bound at ask time, the projection held
  beside the cells; inner, left, semi and anti joins, any cardinality,
  chains and `WHERE`. `RIGHT` and `FULL` are refused. The cost is cells
  keyed by join ids, which the warehouse copy makes about a cell a task
  (§3). That is the question's cost, not a shortcut's. Bound cells
  came that evening: today a fact whose join adds a key keeps them
  beside the keyed cells, asks read them and a dim change moves them
  (ADR-0006).
- **Not chosen:** a per-dim choice of binding or a flip at run time
  (fitting the solution to the data); looking attributes up at ask time
  against PostgreSQL (asks would wait on it); denormalising the
  attributes onto the fact table (duplication in the database, a
  migration per view); summary tables kept by triggers, `pg_ivm` or
  `pg_cron` (a trigger or a lock on the write path, §2.10).

## 2. Sources

The research of 2026-11-10, written the day M12 went in. Each entry
says what the source says and what M12 took from it. ADR-0006 came the
day after and cites the paper of §2.2; where an entry says what Straum
does since, it names ADR-0006.

1. **The delta rule.** Maintaining a join incrementally needs the other
   side: ∆(a ⋈ b) = a ⋈ ∆b + ∆a ⋈ b + ∆a ⋈ ∆b, so "the join operator needs
   to know not only the recent changes (∆a and ∆b), but also it has to
   keep around both entire collections a and b" (Feldera, [Turns out we
   didn't need that second
   index](https://www.feldera.com/blog/turns-out-we-didnt-need-that-second-index),
   U; the post names no author and says the rule "has been known for a
   long time"). Any engine that joins outside the database holds the
   dims. Straum's held copies of M9 and M11 were this, not a mistake.
2. **The floor.** Quass, Gupta, Mumick and Widom, "Making views
   self-maintainable for data warehousing", PDIS 1996
   ([DOI](https://doi.org/10.1109/PDIS.1996.568677),
   [PDF](http://ilpubs.stanford.edu:8090/155/1/1996-29.pdf), which
   answered 500 on 2026-11-21, and its
   [archived copy](http://web.archive.org/web/20250715140227/http://ilpubs.stanford.edu:8090/155/1/1996-29.pdf),
   read that day, R): with key and foreign-key constraints, a
   select-project-join view over a fact and its dims is maintainable
   without the source given auxiliary views. Each is a selection and a
   projection on one table, then semijoins with other auxiliary views:
   for a dim, its key and the referenced columns, less the rows the
   view's own conditions shut out. The paper proves that set "the
   unique minimal set"; it is the least an external maintainer can
   hold. Under "hold nothing", the only move left is to read it from
   the database.
3. **Flink lookup joins.** A lookup join reads the external table per
   probe row. It "uses the above Processing Time Temporal Join syntax",
   and that join is said to "correlate rows to the latest version of a
   key in an external versioned table"; of temporal table joins, "old
   rows are not stored in state"
   ([Joins](https://nightlies.apache.org/flink/flink-docs-release-2.3/docs/sql/reference/queries/joins/),
   R). The JDBC connector caches with `lookup.cache` = `NONE` or
   `PARTIAL`
   ([JDBC connector](https://nightlies.apache.org/flink/flink-docs-stable/docs/connectors/table/jdbc/),
   R); Flink's `LookupCacheType` has `FULL` as well
   ([source](https://github.com/apache/flink/blob/master/flink-table/flink-table-common/src/main/java/org/apache/flink/table/connector/source/lookup/LookupOptions.java),
   R, read 2026-11-21; FLIP-221, U). M12 was `NONE`: one statement per
   ref for each 1,000 keys of a transaction, instead of one per row;
   `PARTIAL` was the bounded cache left open. Under ADR-0006 nothing is
   looked up after the load.
4. **RisingWave process-time temporal joins.** `JOIN ... FOR SYSTEM_TIME
   AS OF PROCTIME()`: "the stream side maintains no state at all. The
   dimension table is the state"
   ([blog](https://risingwave.com/blog/risingwave-process-time-temporal-joins/),
   U); the append-only form "does not require RisingWave to maintain the
   join state, making it suitable for scenarios where the dimension table
   is not updated, or where updates to the dimension table do not affect
   the previously joined results"
   ([Joins](https://docs.risingwave.com/processing/sql/joins), U). M12's
   semantics were these; the dimension table was PostgreSQL's.
5. **ClickHouse dictionaries.** The lookup layer with the layouts named
   outright: `hashed` holds the whole dim, `cache` a bounded part, and
   `direct` has "No in-memory storage" and "queries the source directly
   for each request"; `dictGet` in a materialized view enriches rows at
   insert, and the docs warn against the cache layouts on ingest paths
   ([Dictionary layouts](https://clickhouse.com/docs/reference/statements/create/dictionary/layouts/overview),
   [cache layout](https://clickhouse.com/docs/reference/statements/create/dictionary/layouts/cache),
   U). M12 was `direct`, batched per transaction, which the warehouse
   copy's rate of about two fact changes a second made cheap (U:
   recorded nowhere else).
6. **Druid lookups.** "Lookups do not have history. They always use the
   current data"; the schema page says "Denormalize if possible", which
   freezes the value in the row at ingestion (I), and of lookup tables
   "it is only recommended to do this if you need the ability to update
   a lookup table and have the changes reflected immediately for
   already-ingested rows in your main table"
   ([Schema design](https://druid.apache.org/docs/latest/ingestion/schema-design/),
   [Lookups](https://druid.apache.org/docs/latest/querying/lookups/),
   R, read 2026-11-21). M12's attributes were frozen in the row in this
   way; the M9 late path was the query-time kind, as ADR-0006 is.
7. **Kimball's surrogate key pipeline.** "The extract, transform and
   load (ETL) process for aligning the dimension tables with fact tables
   at load time is called the surrogate key pipeline", and type 1
   against type 2 slowly changing
   dimensions is the choice between the current value and the value as it
   was ([SCD part 2](https://www.kimballgroup.com/2008/09/slowly-changing-dimensions-part-2/),
   U). M9 answered as type 1; M11 and M12 answered as type 2 without
   the history rows, which a start over rewrote to type 1. ADR-0006
   answers as type 1 again (I).
8. **Noria and ReadySet partial state.** Hold the dim rows the queries
   have needed, evict under memory pressure, upquery the base table for
   the rest ([Noria, OSDI
   2018](https://pdos.csail.mit.edu/papers/noria:osdi18.pdf), U;
   [ReadySet deep caching](https://readyset.io/docs/concepts/deep-caching),
   U). The shape a bounded key cache in front of `Resolver`, M12's
   lookup, would have taken; ADR-0006 removed `Resolver`.
9. **Materialize arrangements.** Of a join in most dataflow systems:
   "The operator maintains state proportional to the current records in
   each of its inputs." Materialize shares those arrangements between
   joins, and a delta join "maintains zero intermediate results"; the
   inputs stay held (I)
   ([Delta joins](https://materialize.com/blog/delta-joins/), R, read
   2026-11-21). Another engine that pays the floor of §2.2.
10. **In-database alternatives for the reference case.** `pg_ivm` 1.17
    maintains a view in `AFTER` triggers per statement under an
    `ExclusiveLock` on it, "is not effective when a base table is modified
    frequently", and takes no aggregates over outer joins
    ([README](https://github.com/sraoss/pg_ivm), R); it is offered on
    Azure Database for PostgreSQL flexible server
    ([extension versions](https://learn.microsoft.com/en-us/azure/postgresql/extensions/concepts-extensions-versions),
    R) but not installed on the reference database (U). TimescaleDB
    continuous aggregates are Community licensed
    ([editions](https://www.tigerdata.com/docs/get-started/choose-your-path/timescaledb-editions),
    R), and Azure ships the Apache-2 edition
    ([extension considerations](https://learn.microsoft.com/en-us/azure/postgresql/extensions/concepts-extensions-considerations),
    R). Both solve one database's aggregates on its write path; Straum
    puts no trigger there and no lock a write waits for, though with
    logical decoding `REPLICA IDENTITY FULL` makes each update and
    delete log its whole old row (`docs/src/postgresql.md`), and serves
    many views, which is the bar it has to clear. ALTERNATIVES.md has
    both in full.
11. **Event-carried state transfer.** Consumers keep their own copy of
    the data so that "they don't need to contact the source system in
    order to do further work". "An obvious down-side of this pattern is
    that there's lots of data schlepped around and lots of copies."
    ([Fowler](https://martinfowler.com/articles/201701-event-driven.html),
    R). The copies are what jb ruled out for M12, and the held dims of M9
    to M11 were one. ADR-0006 holds the projections again.

## 3. Numbers

Re-measured on 2026-11-11 after ADR-0006 and before bound cells, on
the warehouse copy of `docs/src/performance.md`: 13,116,878 tasks,
5,690,651 task groups, 851,964 storage locations; the view keyed by
organization, live-ness, the group's category and the location's type.
The M12 column is the run of 2026-11-10.

| | Early binding (M12) | Ask-time binding |
| --- | ---: | ---: |
| Hour cells | 311,969 | 11,932,307 |
| Day cells | 66,107 | 11,930,678 |
| Resident | 84 MiB | 1,998 to 2,378 MiB |
| Ready | 19 s | 10 s |
| Dim rows held | 0 | 6,542,615 |
| Group recategorised | start over, 20 s | 208 ms, no start |
| Locations retyped, one in ten | start over (I, not run) | 1.1 s, no start |
| Groups deleted, one in a hundred | start over (I, not run) | 3.8 s, no start |
| Ask by organization and category, one week | 0.9 ms | 89.2 ms |

The cells follow the join ids, as ADR-0006 reads §2.2 (I): a group
holds a task or two, so both widths hold about a cell a task, and the
day width collapses nothing. Memory is 24 to 28 times M12's, and an ask
over a week reads that week's hour cells and maps each through the
projections, so it costs 100 times what it did. In exchange no dim
change starts Straum over, and every ask equalled PostgreSQL after each
of the seven changes. The ask-time tables are in
`docs/src/performance.md`; of the M12 run only the 84 MiB is still
recorded, in ADR-0006. With bound cells, which came after this run, "a
joined view asks like an unjoined one" (ADR-0006).
