# Straum

Straum keeps aggregates of PostgreSQL tables in memory and answers asks
about them over HTTP. You declare the aggregates in a spec, a small subset
of PostgreSQL's `CREATE VIEW`. Straum loads them from a snapshot, then
follows every commit through logical replication. Each answer says how
fresh it is: every commit made before its `as_of` is in it.

The name is Norwegian. In Nynorsk, one of the two written standards of
Norwegian, *straum* means a stream or current, and Straum's input is
PostgreSQL's replication stream.

![PostgreSQL holds the tables and the WAL. A table owner runs the setup SQL that straum plan prints, once. On every start, straum run loads a snapshot at LSN L, then applies every commit after L. Apps ask over HTTP.](diagrams/overview.svg)

> [!WARNING]
> Straum is not stable yet. The spec, the asks and the flags may change.

Straum maintains `count`, `count(DISTINCT)`, `sum`, `min`, `max` and
`avg`, grouped by columns of the fact table, columns of the tables it
joins N:1, and UTC time buckets. An ask is a `SELECT` over one view with
every value as a parameter; it costs in proportion to the cells it reads
and never touches PostgreSQL. Straum needs PostgreSQL 17 or newer as
shipped, with no extension and no superuser, so managed services work.

## What it is not

Straum is not a SQL engine, a replica, a cache or a durable store. It
answers only what its spec declares, and it keeps nothing on disk: every
start loads every table again, and a crash leaves no slot behind holding
WAL. PostgreSQL stays the source of truth.
