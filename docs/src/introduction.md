# Straum

Straum keeps aggregates of PostgreSQL tables in memory and answers asks
about them over HTTP. You declare the aggregates in a spec, written in a
small subset of PostgreSQL's `CREATE VIEW`. Straum loads them from a
snapshot, then follows every commit through logical replication. Each
answer says which commit it reflects.

![PostgreSQL holds the tables and the WAL. A table owner runs the setup SQL that straum plan prints, once. On every start, straum run loads a snapshot at LSN L, then applies every commit after L. Apps ask over HTTP.](diagrams/overview.svg)

> [!WARNING]
> Straum is not stable yet. The spec, the asks and the flags may change.

## What it does

- Maintains `count`, `count distinct`, `sum`, `min`, `max` and `avg`.
- Groups by columns of the table, by columns of tables it joins N:1, and
  by UTC time buckets, with wider buckets for long ranges.
- Follows the database: an insert, update or delete shows up in answers
  as soon as Straum applies its commit, and a transaction shows up whole.
- Uses only stock PostgreSQL 18: logical replication with `pgoutput`, a
  temporary slot and an exported snapshot. It needs no extension and no
  superuser, so it works on managed PostgreSQL.

An ask never reads rows and never queries PostgreSQL. It merges the cells
Straum keeps per group and time bucket, so its cost grows with the number
of cells in its range.

## What it is not

Straum is not a SQL engine, a replica, a cache or a durable store. It
answers only the aggregates its spec declares, and it keeps nothing on
disk: every start loads every table again. PostgreSQL stays the source of
truth.

## Where to go next

- [Quick start](quick-start.md): Straum against PostgreSQL in Docker, in a
  few minutes.
- [Writing a spec](spec.md) and [Asking](asks.md): what you can declare
  and how to read it back.
- [Setting up PostgreSQL](postgresql.md), [Running Straum](running.md) and
  [Kubernetes](kubernetes.md): putting it next to a real database.
- [How Straum works](how-it-works.md): the snapshot, the stream and the
  rollups.
