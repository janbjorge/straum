# Sizing memory

Straum's memory follows cells, and only cells: it holds no rows and no
dim rows. Each bucket width keeps one cell per distinct combination of
keys and bucket, so per width:

```text
cells <= min(rows, distinct key combinations x buckets)
```

Keys that are nearly unique per row keep nearly a cell per row. Fewer
key columns or wider buckets collapse more rows into each cell, and
selecting a dim's column instead of the fact column joined on collapses
most. `count(DISTINCT c)` adds an entry per distinct value in each
cell. In the two runs below the resident set came to 210 and 233 bytes
a cell, measures included.

The key decides how many cells there are. Measured on the taxi month
under a wide key, and on a warehouse's tables under a narrow one.

## The taxi month under a wide key

```sh
cargo test -p straum-pg --release taxi_month -- --ignored --nocapture
```

It needs the data `bench/bench.py` fetches. It loads the month with
`crates/straum-notation/tests/taxi.sql`, a wider view than the
benchmark's: 15 minute buckets with day and week widths, keyed by payment
type, vendor, pickup zone, dropoff zone and the pickup zone's borough,
zone and service zone, with two filtered counts. Straum loads and
follows it, and the test compares asks with the view body in PostgreSQL
four times: after the load, after 200 streamed commits, after an update,
and after two zones change and a trip in one of them makes Straum start
over. One run on 2026-11-10, release build, Docker on the same laptop:

| Step or ask | Groups | Straum | PostgreSQL |
| --- | ---: | ---: | ---: |
| Load | | 2.1 s | |
| 200 streamed commits | | 4.1 s | |
| Start over after the zones changed | | 2.6 s | |
| Every key | 2,322,783 to 2,329,698 | 1.7 to 1.8 s | 4.6 to 4.8 s |
| By pickup borough | 8 | 4.5 to 5.2 ms | |

The ask over every key reads every 15 minute cell and answers nearly as
many groups, so it is only 2.6 to 2.8 times faster than the view in
PostgreSQL. Cells per width, each a `count(DISTINCT ...)` of that key
and the bucket over the month:

| Rollup | Cells | Trips a cell |
| --- | ---: | ---: |
| Raw trips | 2,964,624 | 1.0 |
| 15 minutes | 2,322,783 | 1.3 |
| 1 hour | 1,685,731 | 1.8 |
| 15 minutes, no dropoff zone | 568,612 | 5.2 |
| 1 day | 519,726 | 5.7 |
| 7 days | 199,398 | 14.9 |

Pickup and dropoff zone together are nearly unique per trip, so this
spec collapses little at 15 minutes. Without the dropoff zone it
collapses 5.2 times.

## A warehouse's tables

The same lever on a warehouse shape: tasks keyed by organization,
whether the task is live, its group's category and its storage
location's type, over hour buckets with a day width beside them. The
category and the type are columns of two dims, read from the group
and location tables as each task comes in. The tables are a copy of a
production database and are not in the repository: 13,116,878 tasks,
5,690,651 task groups and 851,964 storage locations, 2,645 MiB of tasks
in PostgreSQL. One run on 2026-11-10, the machine and build of the
[benchmark](benchmark.md#results).

| Rollup | Cells |
| --- | ---: |
| 1 hour | 311,969 |
| 1 day | 66,107 |

Keyed by task group and storage location instead, the view would hold
about a cell a task, 12 million a width: a group holds one task or two,
and every task has its own hour. The dim columns' few values collapse the
day width to 66 thousand cells.

Straum was ready 19 s after it started and held 84 MiB: the cells, and
nothing else. Holding the 6.5 million group and location rows for
lookups of its own, as it did before joins moved to PostgreSQL, it held
897 MiB and was ready in 8 s.

Every ask equalled PostgreSQL after the load and after each streamed
change. Each change is one transaction, applied once its dim columns are
looked up; the time runs from the commit until Straum had applied it:

| Change | Rows | Applied |
| --- | ---: | ---: |
| New tasks for one organization, completed now | 1,000 | 74 ms |
| Open tasks completed, one in fifty | 19,024 | 3.4 s |
| Completed tasks of one day cancelled, one in twenty | 1,873 | 156 ms |
| Tasks of one day deleted, one in thirty | 7,426 | 275 ms |

An update looks up the old row and the new, so the big one asked
PostgreSQL for 38 thousand group keys and as many location keys. A task
group recategorised is nothing to Straum until a task of it changes;
then Straum starts over, 20 s, and the asks equal PostgreSQL again.

The asks, PostgreSQL run twice on the loaded tables and Straum the better
of two:

| Ask | Rows | PostgreSQL, first | PostgreSQL, second | Straum |
| --- | ---: | ---: | ---: | ---: |
| Picks by organization, one day | 44 | 19 ms | 10 ms | 0.3 ms |
| Picks by organization and category, one week | 110 | 667 ms | 603 ms | 0.9 ms |
| Live picks by location type, one day | 3 | 100 ms | 92 ms | 0.9 ms |
| Picks per hour for one organization, one day | 16 | 7 ms | 5 ms | 0.9 ms |

PostgreSQL has an index on `completed` here, and these are the plain
queries the asks mirror, not a dashboard's own. Straum reads the cells
of one width in the range whatever the question, so its time barely
moves between them.
