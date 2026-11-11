# Sizing memory

Straum's memory follows cells, plus a projection of each dim: its key
and the columns the views read, nothing else of it. Each bucket width
keeps one cell per distinct combination of keys and bucket, where a
dim's column counts as the fact columns joined on, so per width:

```text
cells <= min(rows, distinct key combinations x buckets)
```

Keys that are nearly unique per row keep nearly a cell per row. Fewer
key columns or wider buckets collapse more rows into each cell. A dim
column collapses nothing beyond its join columns: the cells are keyed
by the ids, and the column's few values are read at ask time, so a dim
change moves answers without moving cells. `count(DISTINCT c)` adds an
entry per distinct value in each cell. In the two runs below the
resident set came to 177 and 104 bytes a cell, measures and
projections included.

The key decides how many cells there are. Measured on the taxi month
under a wide key, and on a warehouse's tables under a narrow one whose
dims carry the ids.

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
six times: after the load, after 200 streamed commits, after an update,
after a zone is renamed, after a zone moves to a new key, and after a
trip in the renamed zone changes. Nothing starts over. One run on
2026-11-11, release build, Docker on the same laptop:

| Step or ask | Groups | Straum | PostgreSQL |
| --- | ---: | ---: | ---: |
| Load | | 2.0 s | |
| 200 streamed commits | | 5.1 s | |
| A zone renamed, one commit | | 2.4 ms | |
| A zone moved to a new key, one commit | | 2.1 ms | |
| A trip in the renamed zone, one commit | | 2.4 ms | |
| Every key | 2,322,783 to 2,329,715 | 2.7 to 3.2 s | 6.3 to 6.4 s |
| By pickup borough | 8 | 11.3 to 16.3 ms | 4.1 to 4.3 s |

The ask over every key reads every 15 minute cell, maps each through
the zones' projection and answers nearly as many groups, so it is only
2.0 to 2.4 times faster than the view in PostgreSQL. Cells per width,
each a `count(DISTINCT ...)` of that key and the bucket over the month:

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
category and the type are columns of two dims, so the cells are keyed
by the group and location ids and Straum holds each dim's projection
beside them: its key and the one column the view reads. The tables are
a copy of a production database and are not in the repository:
13,116,878 tasks, 5,690,651 task groups and 851,964 storage locations,
2,645 MiB of tasks in PostgreSQL. One run on 2026-11-11, the machine
and build of the [benchmark](benchmark.md#results).

| Rollup | Cells |
| --- | ---: |
| 1 hour | 11,932,307 |
| 1 day | 11,930,678 |

A group holds one task or two, and every task has its own hour, so
both widths hold about a cell a task; the day width collapses nothing,
since the group id is in its key. Keyed by the category and the type
instead, as a view that never followed a dim change could be, the
widths would hold 311,969 and 66,107 cells.

Straum was ready 10 s after it started and held 2,378 MiB: the cells
and the two projections, 6,542,615 dim rows. A run right before held
1,998 MiB; 88 to 104 bytes a cell, projections included.

Every ask equalled PostgreSQL after the load and after each streamed
change. Each change is one transaction; the time runs from the commit
until Straum had applied it:

| Change | Rows | Applied |
| --- | ---: | ---: |
| New tasks for one organization, completed now | 1,000 | 99 ms |
| Open tasks completed, one in fifty | 19,024 | 5.9 s |
| Completed tasks of one day cancelled, one in twenty | 1,873 | 111 ms |
| Tasks of one day deleted, one in thirty | 7,426 | 95 ms |
| Task groups of one day recategorised | 3,365 | 208 ms |
| Storage locations retyped, one in ten | 85,196 | 1.1 s |
| Task groups deleted, one in a hundred | 56,875 | 3.8 s |

The last three change dims. Each is applied to the projection, no cell
moves, and the next ask reads the new category or type; Straum never
started over. The run right before applied the big task update in
2.8 s, so that row is the noisier one.

The asks, PostgreSQL run twice on the loaded tables and Straum the better
of two:

| Ask | Rows | PostgreSQL, first | PostgreSQL, second | Straum |
| --- | ---: | ---: | ---: | ---: |
| Picks by organization, one day | 44 | 33 ms | 15 ms | 0.9 ms |
| Picks by organization and category, one week | 110 | 1,448 ms | 751 ms | 89.2 ms |
| Live picks by location type, one day | 3 | 127 ms | 100 ms | 6.7 ms |
| Picks per hour for one organization, one day | 16 | 14 ms | 5 ms | 0.6 ms |

PostgreSQL has an index on `completed` here, and these are the plain
queries the asks mirror, not a dashboard's own. Straum reads the cells
of one width in the range, about a cell a task, and maps each through
the projections, so an ask over a week costs more than one over a day.
