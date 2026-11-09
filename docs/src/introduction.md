# Straum

Straum keeps aggregates of PostgreSQL tables in memory and answers asks
about them over HTTP. You declare the aggregates in a spec, a small subset
of PostgreSQL's `CREATE VIEW`. Straum loads them from a snapshot, then
follows every commit through logical replication. Each answer names the
commit it reflects.

The name is Norwegian. In Nynorsk, one of the two written standards of
Norwegian, *straum* means a stream or current, and Straum's input is
PostgreSQL's replication stream.

![PostgreSQL holds the tables and the WAL. A table owner runs the setup SQL that straum plan prints, once. On every start, straum run loads a snapshot at LSN L, then applies every commit after L. Apps ask over HTTP.](diagrams/overview.svg)

> [!WARNING]
> Straum is not stable yet. The spec, the asks and the flags may change.

## The model

A spec declares facts, one view per fact table, such as `orders`. Each
fact row lands in a cell for its time bucket and its combination of by
columns, and the cell keeps what the measures need, such as a count and a
sum. Dims are tables a fact joins N:1, such as `customers`. Straum keeps
their rows by key and looks up their columns when it answers.

An ask names a fact, measures, groups, filters and a time range. Straum
merges the cells in the range. It never reads rows and never queries
PostgreSQL, so an ask costs in proportion to the cells it reads.

Straum maintains `count`, `count(DISTINCT)`, `sum`, `min`, `max` and
`avg`, grouped by fact columns, dim columns and UTC time buckets. A
transaction shows up whole, as soon as Straum applies its commit. It needs
PostgreSQL 17 or newer as shipped, with no extension and no superuser, so
managed services work.

## What it is not

Straum is not a SQL engine, a replica, a cache or a durable store. It
answers only what its spec declares, and it keeps nothing on disk: every
start loads every table again, and a crash leaves no slot behind holding
WAL. PostgreSQL stays the source of truth.
