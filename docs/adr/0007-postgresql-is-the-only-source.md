# ADR-0007: PostgreSQL is the only source

Status: Accepted 2026-11-10; revised 2026-11-24

## Context

`tokio-postgres` has no replication mode, `supabase/etl` owns the slot
and the copy, and the replication crates on crates.io build C for TLS,
which the static build can't take. The
first load ran one `GROUP BY` per fact in PostgreSQL: it hashed the
whole table there, could spill, ran spec text on the server and came
back as text, 9.4 s on the taxi month. Taking a row out of a cell
needs the old row. A Parquet feed once saved the benchmark a minute,
at the cost of two crates, a second binary, an unauthenticated socket
and a manifest format.

## Decision

`straum-pg` is Straum's own replication client on `postgres-protocol`
(framing, SCRAM), decoding pgoutput v1 itself. The slot is temporary.
It exports its snapshot, and a plain connection takes that up and
reads the catalog and the first load in it, so load and stream meet at
one point. The slot's connection starts streaming only once the
snapshot is taken up, since PostgreSQL keeps it only until that
connection's next command; from then on the stream thread reads beside
the load, as the physical stream's does. Another plain connection reads
the WAL position. Until 2026-11-23 the load ran on the slot's own
connection, in a transaction that used its snapshot, and nothing read
the slot until the last load.

A load is `COPY (SELECT <columns> FROM <table>) TO STDOUT (FORMAT
binary)` per dim and per fact table. One copy of a fact table carries
each column any of its views reads, once; Straum hands each view its
own columns and groups the rows. A copy per view was dropped on
2026-11-22: PostgreSQL 18 took 1802 ms to send the taxi month once for
each of the benchmark's two views, and 1304 ms to send the columns of
both once. A table read as a dim and as a fact table is still copied
for each, since dims load first. A reader thread decodes batches of
8192 rows, four in flight; each width fills on its own thread. The
snapshot commits right after the last load.

Every table a spec names, fact or dim, has `REPLICA IDENTITY FULL` and
is in the publication; `straum plan` prints both and every start checks
them, naming the fix. Updates and deletes apply from the old row the
WAL carries; truncate empties the cells or the projection. No table
needs a primary key, and there is no `key` or `append` option.

PostgreSQL is the only source: `straum-pg` is the one adapter behind
`Source`, and `straum` the one binary. Demos and benchmarks run on a
throwaway PostgreSQL holding the taxi trips, through `bench/taxi.py`.

Straum runs on PostgreSQL 17 and newer, for both streams. The start
check refuses older servers, naming the floor. CI runs 17, 18 and
19, picked by `STRAUM_TEST_POSTGRES`; locally 18.
Tests start PostgreSQL with `docker compose`, one project per test,
so each server has its own network and container name.
16 was dropped: it logs prune and vacuum as separate records, a second
parser for the physical stream (ADR-0008).

## Consequences

- The build stays pure Rust and static. Straum owns only the protocol
  it uses: simple query on the replication connection, binary `COPY`,
  pgoutput, keepalives (ADR-0002).
- A load holds about 1.2 MB per column beside the store. The taxi month
  loads in 1.9 to 2.0 s; PostgreSQL alone copies it out in 1.4 s.
  Nothing from the spec runs in PostgreSQL.
- A load holds `AccessShareLock` and pins xmin until its snapshot
  commits; held longer, it blocked a test's `ALTER TABLE`.
- The load takes one plain connection more while it runs, which a
  role's `CONNECTION LIMIT` must leave room for. The stream confirms
  all the WAL it reads during the load, the commits held in memory.
- `FULL` adds WAL on `UPDATE` and `DELETE` only. A row's identity is
  the whole row, so a dim row held twice comes out once per delete.
- From 19 a logical slot turns on decoding under `wal_level = replica`
  (PostgreSQL `67c20979ce7`); the start check reads
  `server_version_num` and accepts `replica` there.
- Docker with Compose is the one thing tests need beside Rust, and
  no crate drives it. A newer release's
  quirk is found by CI, not a user. A server still on 16 can't run
  Straum.
- A benchmark of Straum alone loads the trips into PostgreSQL first,
  about a minute.
