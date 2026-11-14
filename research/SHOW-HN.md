# Show HN draft

## Title

```text
Show HN: Straum - in-memory PostgreSQL aggregates via logical replication
```

## URL

```text
https://github.com/janbjorge/straum
```

## Text

I built Straum for dashboards that repeat the same `GROUP BY` over
PostgreSQL tables. It keeps aggregates from a declared view in memory,
then follows the view's input tables through logical replication. Apps
ask it over HTTP; PostgreSQL stays the source of truth.

At startup, Straum loads a PostgreSQL snapshot, then streams commits from
the same LSN. Each answer includes `as_of`, the time through which commits
are applied.

It is intentionally narrow: one declared view, restricted queries, no
disk, and PostgreSQL remains authoritative. It needs PostgreSQL 16+ and
logical replication, but no extension or superuser.

On six months of NYC taxi trips, 20 million rows, it answered seven
aggregate queries in 125-324 µs versus 28-1,751 ms for PostgreSQL over
the raw tables. The
repo has the runnable Docker example and benchmark details.

I would especially value feedback on real dashboard workloads, operational
risks, and where this fits or loses to materialized views, caches, or OLAP.
