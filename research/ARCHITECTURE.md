# Straum and PostgreSQL (2026-11-12)

How Straum uses PostgreSQL, with the citations behind each claim. The
design itself is DOMAIN.md; `docs/adr/` holds the decisions this
follows; the user guide (`docs/`) says how to set PostgreSQL up and run
Straum. First target: AKS next to Azure Database for PostgreSQL
Flexible Server 18 (AKS.md). The constraints are the managed ones: no
custom extension, no superuser, logical replication allowed. Tags as in
README.md (R, U, I). PostgreSQL paths are relative to the root of
[upstream PostgreSQL](https://github.com/postgres/postgres/tree/0f3b42ce806).

Everything Straum needs ships in stock PostgreSQL 17 and newer and
works on Azure: pgoutput, temporary logical slots, exported snapshots,
SQL on a replication connection.

## 1. Start and initial load

The spec is known at start, so one slot and one snapshot serve every
table:

![The writer opens a temporary slot whose snapshot is at LSN L, loads each table in it, then streams from L through a stream thread.](../docs/src/diagrams/start.svg)

- Each table, dim first, loads with one `COPY (SELECT <columns> FROM
  <table>) TO STDOUT (FORMAT binary)` (`load::copy_sql`), and Straum
  groups the fact rows and projects the dim rows itself
  (`crates/straum-pg/src/load.rs`, `Rollup::add_rows`,
  `Projection::add_rows`). No output functions run, and nothing from the
  spec runs there. A reader thread decodes batches of 8192 rows, four in
  flight, so a load holds about 1.2 MB per column besides the store.
- SQL and replication commands share one connection in logical walsender mode
  (`doc/src/sgml/protocol.sgml:2186-2190`, R), simple query protocol only
  (`:2193-2194`, R). Loads come back as binary `COPY`; pgoutput v1 sends the
  stream's values as text, which Straum reads under the `DateStyle = ISO` and
  `TimeZone = UTC` it sets at startup (`crates/straum-pg/src/value.rs`).
- The catalog checks run before the slot, since below PostgreSQL 19 a server
  at `wal_level = replica` can't make one and PostgreSQL's error names no
  fix, and again in the slot's snapshot, so they see the tables as the load
  does. Each problem found names its fix (`crates/straum-pg/src/catalog.rs`).
- `SNAPSHOT 'use'` must run first in a REPEATABLE READ, read-only transaction
  (`src/backend/replication/walsender.c:1342-1364`, R). The load sees the state
  at L, and the stream starts with the first commit after L.
- `TEMPORARY`: "Temporary slots are not saved to disk and are automatically
  dropped on error or when the session has finished"
  (`protocol.sgml:2371-2374`, R).
- The snapshot transaction pins xmin while the load runs and holds back vacuum
  for the whole database (POSTGRES-REPLICATION.md §4). It also holds a lock on
  every table it read, which blocks `ALTER TABLE`. Straum commits it right after
  the last load, not at the first stream read (M5; a test's `ALTER TABLE`
  waited on it before). Reading only the spec's columns, in binary, keeps it
  short: the taxi month, 2,964,624 trips, loads in 2.0 s (M5).

## 2. Apply

![What each pgoutput message does to facts with REPLICA IDENTITY FULL, facts with the default identity and dims, and how Straum starts over. Drawn before ADR-0031: a dim is published with FULL like a fact, and its changes go to its projection.](diagrams/apply.svg)

Old values: by default pgoutput sends only the key on DELETE and no old values
on an UPDATE that leaves the key alone. Only REPLICA IDENTITY FULL sends the
whole old row (POSTGRES-REPLICATION.md §3, R). So:

- Facts need the old row to take it out of its cell, so every fact table
  has REPLICA IDENTITY FULL; Straum checks it at every start. FULL writes the
  old row to WAL on UPDATE and DELETE only, so a table that only gets inserts
  pays nothing for it. An UPDATE or DELETE that still comes without the old
  row, as after the identity is changed back, starts Straum over, and that
  start reports the missing FULL.
- Dims are in the stream like facts, with FULL (ADR-0031). A dim row's
  insert, update, delete or truncate applies to its projection in the
  same commit as the fact rows beside it; a dim row not held starts
  Straum over, as a fact row that doesn't fit does.
- Truncate empties the table in place. Changes before it in the transaction
  are gone, changes after it apply.

Starting over: reading one table again on a second connection was cut
(ADR-0015).

- Straum closes the stream, so PostgreSQL drops its temporary slot, waits,
  and loads every table from a new slot and its snapshot. A new slot
  reserves WAL from the current insert position
  (`src/backend/replication/slot.c:1759-1788`, R), and creating it waits for
  the transactions open at that moment
  (`src/backend/replication/logical/snapbuild.c:98-107`, R).
- Asks answer not ready until the new store is swapped in.

A commit's LSN is the end of its commit record, so "up to `at`" means the
commits that end at or before it. A transaction is decoded only if its commit
record starts at or past the start point
(`src/backend/replication/logical/snapbuild.c:305-311`, R), and pgoutput's
Commit carries the record's start, then its end
(`src/backend/replication/logical/proto.c:89-90`, R). M5 tested it: 2,000
single-row commits racing the slot's creation, three times, and no commit
lost or applied twice (`the_snapshot_and_the_stream_split_exactly`).

Schema drift: pgoutput resets `schema_sent` when a table's relcache entry is
invalidated (`src/backend/replication/pgoutput/pgoutput.c:2122-2128`, R), so it
sends a Relation message before the next change to an altered table. Straum
compares the columns it reads with those it loaded: a column that reads
differently, such as `numeric(10, 2)` turned `numeric(12, 3)`, starts Straum
over. A column dropped, or turned to a type Straum doesn't read, stops
the stream, and the setup check on the next start names it. A table that is
altered but not written to goes unnoticed until its next change (I).

An update that leaves a TOASTed value alone doesn't send it. Straum takes it
from the old row when one came; without one, it starts over.

`streaming` is off in v1, so changes arrive after commit. A transaction larger
than `logical_decoding_work_mem` spills to the primary's disk
(POSTGRES-REPLICATION.md §6, R). Rollups change at Commit, so an ask never sees
half a transaction.

## 3. Freshness

v1 has no barrier; every answer carries `as_of` (ADR-0007). A per-ask
barrier stays possible: Straum calls `pg_logical_emit_message(...,
flush => true)` on a SQL connection and answers once the stream passes
the returned LSN (POSTGRES-REPLICATION.md §8, R).

## 4. Setup

`straum plan` prints the SQL a table owner runs once
(`docs/src/postgresql.md`). Why it is shaped so:

- Adding a table to a publication needs ownership of it
  (`src/backend/commands/publicationcmds.c:2051-2054`, R); `FOR ALL TABLES`
  needs superuser (`publicationcmds.c:1508-1516`, R).
- Setup lives in the catalog and survives Straum restarts, PostgreSQL restarts
  and failover. Straum rechecks it on every start.
- The spec names every column Straum reads, so the publication could list only
  those columns. A column list must cover the replica identity, and REPLICA
  IDENTITY FULL allows none (`publicationcmds.c:404`, R). Without a list every
  column leaves the database, and column privileges and RLS don't apply to the
  stream. Open point.

## 5. Wire diagram

![Straum in AKS next to the apps, with one replication connection to Flexible Server.](diagrams/deploy.svg)

Every path already exists in the cluster: Straum reaches PostgreSQL the way the
apps do, and apps reach Straum in-cluster.

## 6. RAM only

Nothing to persist, and the temporary slot is what makes that safe: a crashed,
OOM-killed or deleted pod leaves no slot behind, so it can't fill the server's
disk. The costs:

- **Every start is a full load**, one binary `COPY` per table under one
  snapshot.
- **The load pins xmin** for the whole database while it runs (§1).
- **No slot failover.** "cannot enable failover for a temporary replication
  slot" (`src/backend/replication/slot.c:411-420`, R). Azure HA failover drops
  the connection and Straum starts over. In exchange,
  `sync_replication_slots` and `hot_standby_feedback` aren't needed.
- **Lag still holds WAL while connected.** Azure fixes `max_slot_wal_keep_size`
  at `-1` (MANAGED-POSTGRES.md). Straum reads the WAL held for its slot every
  second and starts over past `--max-retained-wal` (4Gi).
- **Each replica is a slot plus a walsender**, and each walsender decodes all
  WAL (POSTGRES-REPLICATION.md §9). Default is one replica.

Not in v1: a warm start from a snapshot file. It needs a persistent slot to
resume from, which brings the WAL risk back.

## 7. Source modes: `wal_level`

PostgreSQL 17 is the floor (jb, 2026-11-09). CI tests 17, 18 and 19rc1.

- PostgreSQL 17 and 18 with `wal_level = logical`: pgoutput and a temporary
  slot.
- PostgreSQL 17 and 18 with `replica`: Straum refuses to start and says to set
  `wal_level = logical` and restart. It checks before it makes the slot,
  since a logical slot fails with "logical decoding requires \"wal_level\" >=
  \"logical\"" (`src/backend/replication/logical/logical.c:121-123` as of
  `67c20979ce7^`, R), which names no fix.
- PostgreSQL 19 and later, `logical` or `replica`: under `replica`, PostgreSQL
  turns logical decoding on while a logical slot exists and off lazily after the
  last one goes (`67c20979ce7`, `logical/logicalctl.c:5-38`, R). The start check
  reads `server_version_num` to tell. On 19rc1 a temporary slot with an exported
  snapshot works under `replica`, and `effective_wal_level` reads `logical`
  while it exists.

One code path covers all of them. Azure Flexible Server 18 defaults to
`replica` (U), so it needs one restart with `wal_level = logical`.

## Open points

1. Which aggregates the apps need, and whether a spec can declare them.
2. Publication column lists from the spec (§4).
3. How long the load pins xmin on real tables (§1).
4. Low collapse: warn in `straum plan`, or leave it to the spec author
   (`docs/src/sizing.md`).
5. What clients do while Straum is not ready: retry, or ask PostgreSQL.
6. Long asks delay the writer while they hold the read lock (ADR-0009).
