# weir: research notes

weir = working name for external aggregate server for PostgreSQL. Weir = low dam across river to measure flow; server sits on replication stream, measures it.

## The idea (jb, 2026-11-08)

- Spec lists PostgreSQL schemas, tables, columns.
- Server connects to PostgreSQL as logical replication subscriber, ingests only that subset.
- Stores data so GROUP BY with sum, avg, min, max, count ~1000x faster than PostgreSQL, near-zero drift.
- Ideal: PostgreSQL itself sees "this is an aggregate over a table the server has", asks server.
- Rust.

Questions asked:
1. Can PostgreSQL use server by itself to answer aggregates?
2. If so, need extension?
3. Need SQL parser? What to lean on?
4. Rust.

## Files

| File | What |
|---|---|
| [POSTGRES-PLANNER.md](POSTGRES-PLANNER.md) | How planner could hand aggregates to external server: FDW upper paths, postgres_fdw, hooks, CustomScan, partial aggregation |
| [POSTGRES-REPLICATION.md](POSTGRES-REPLICATION.md) | Logical replication internals: protocol, publications, old tuples, initial sync, slots, consistency, freshness |
| [COMPETITORS.md](COMPETITORS.md) | Competitor landscape, ClickBench ratios, dead projects, gap analysis |
| [RUST.md](RUST.md) | Rust building blocks, recommended stack, what we must write |
| [MANAGED-POSTGRES.md](MANAGED-POSTGRES.md) | What AWS, Google Cloud, Azure, other managed providers let customer do themselves; options without vendor; tests to run |
| [AKS.md](AKS.md) | First target: AKS + Azure Flexible Server 18; Helm chart shape, one-time DB setup, publication risks, pg_ivm baseline |
| [ARCHITECTURE.md](ARCHITECTURE.md) | Draft design with diagrams: wire diagram, replication and barrier, query routing and rollup matching, components, lifecycle, RAM-only with temporary slots, memory layout, bytes per row |

## Tags

- **R**: read in PostgreSQL tree, or in linked fetched source.
- **U**: unverified; from search snippet, third party, or memory.
- **I**: inference.

PostgreSQL tree `/Users/janbjorge/src/postgres`, version `20devel` (`meson.build:11`), upstream master `0f3b42ce806` (2026-10-19). Checkout has few unrelated local commits; none touch planner, FDW, table AM, replication, transam, procarray code, so all citations here = upstream code. Line numbers from master, not REL_18/REL_19 branches. Paths relative to `src/backend` unless stated. Research by four agents 2026-11-08; load-bearing citations (`planner.c:4458`, `walsender.c:1604`, `message.c:78-79`, `xact.c:1488`/`:2446`, `snapbuild.c:478-484`, `planner.c:7697-7701`) re-read by hand same day.

## Target versions

PostgreSQL 18, 19, newer (jb, 2026-11-08). Tree has no release tags, so versions placed by ancestry vs stamp commits: `2652835d3ef` "Stamp HEAD as 19devel" (2025-06-29, PG18 branch point) and `a281a3e6dbb` "Stamp HEAD as 20devel" (2026-06-29, PG19 branch point). Backpatches not accounted for.

Available on PG18 and PG19:
- `create_upper_paths_hook`, `set_rel_pathlist_hook`, `planner_hook`, CustomScan, postgres_fdw aggregate pushdown.
- Publication column lists and row filters (PG15), `streaming=parallel` (PG16), logical decoding on standby (PG16).
- Failover slots, `sync_replication_slots`, `synchronized_standby_slots` (PG17); `pg_logical_emit_message(..., flush)` (`173b56f1ef5`, PG17).
- `idle_replication_slot_timeout` (`ac0e33136ab`) and `publish_generated_columns` (`7054186c4eb`), PG18.

PG19 only (not PG18):
- `planner_setup_hook` / `planner_shutdown_hook` (`94f3ad3961a`).
- `build_simple_rel_hook`, **replaces** `get_relation_info_hook` (`91f33a2ae92`). Code building on both should depend on neither.
- `join_path_setup_hook` / `joinrel_setup_hook` (`4020b370f21`).
- Eager aggregation, `enable_eager_aggregate` (`8e11859102f`).
- `WAIT FOR` (`447aae13b03`) and write/flush modes (`7a39f43d885`). Not usable by weir either way (POSTGRES-REPLICATION.md §8b).
- postgres_fdw `import_stats` / `ImportForeignStatistics` (`28972b6fc3d`).
- Publications `FOR ALL SEQUENCES` (`96b37849734`) with sequence sync (`5509055d695`), and `EXCEPT TABLE` (`fd366065e06`).
- Logical decoding auto-on while logical slot exists, with `wal_level=replica` (`67c20979ce7`). PG18 primary needs `wal_level=logical`, requires restart.

Tooling: pgrx 0.19.3 covers PG15–19 (RUST.md §5). supabase/etl lists PG14–18; PG19 support unverified.

## Summary

Buildable; nobody ships full combo for stock PostgreSQL. Three things shape design:

1. **1000x comes from pre-aggregation, not from moving data out.** Raw columnar engines 31x (DuckDB) to 103x (ClickHouse) faster than cached PostgreSQL on ClickBench; 1000x+ only when PostgreSQL disk-bound or rows collapse into far fewer groups (COMPETITORS.md §2).
2. **Drift is fixable per query.** WAL barrier (`pg_logical_emit_message(..., flush => true)`) + wait for server to apply up to that LSN = read-your-writes at query start. Zero drift possible with server as synchronous standby under `remote_apply`, cost on every commit (POSTGRES-REPLICATION.md §8).
3. **Transparent routing on the original tables needs an extension or a wire proxy.** `create_upper_paths_hook` + CustomScan = planner route; postgres_fdw works without custom extension but only on foreign tables (POSTGRES-PLANNER.md §1, §3).

"A poor man's TimescaleDB" = wrong analogy. Closer: MySQL HeatWave and TiDB TiFlash (optimizer offloads to secondary engine fed by change stream, checks freshness), plus StarRocks/Doris-style auto rewrite onto rollups.

### Routes for question 1

- **A. Planner hook (transparent).** `create_upper_paths_hook` at `UPPERREL_GROUP_AGG` fires for local tables (`optimizer/plan/planner.c:4458`). CustomPath with `scanrelid = 0` and Aggrefs in `custom_scan_tlist` competes on cost with native plan.
- **B. postgres_fdw (no custom extension, not transparent).** Users query foreign table; postgres_fdw ships whole aggregates as SQL to server speaking wire protocol.
- **C. Wire proxy (ReadySet model).** Transparent to apps, no extension, but parses every statement, adds hop to all traffic.

### Question 2

Transparent on original tables: yes, extension (A) or proxy (C). Foreign tables OK: no custom extension, contrib postgres_fdw enough.

### Question 3

- Route A: no SQL parser. PostgreSQL already parsed and analysed query; extension sends compact request (table, group keys, aggregates, ranges, barrier LSN).
- Route B: parse postgres_fdw's narrow deparsed dialect with `pg_query` (libpg_query, real parser).
- Route C: must parse everything; `pg_query`.
- Ad hoc SQL over raw columnar data: DataFusion brings own parser and planner.

### Question 4

See RUST.md: supabase/etl (or pg_walstream) for change capture, pgwire for wire protocol, DataFusion for ad hoc queries, pgrx for extension, pg_query for parsing.

## Managed PostgreSQL (jb, 2026-11-08)

Most teams run managed PostgreSQL, can't install new extension, so route A = hard path. What still works:
- **Ingest.** Logical replication = parameter + restart: RDS and Aurora `rds.logical_replication=1` with `rds_replication` role ([AWS](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/PostgreSQL.Concepts.General.FeatureSupport.LogicalReplication.html)); Cloud SQL `cloudsql.logical_decoding=on` ([Google](https://docs.cloud.google.com/sql/docs/postgres/replication/configure-logical-replication)); Azure `wal_level=logical`. Azure drops unused logical slots past storage threshold, no logical replication on read replicas ([Microsoft](https://learn.microsoft.com/en-us/azure/postgresql/configure-maintain/concepts-logical)).
- **Route B.** postgres_fdw available on all supported RDS and Aurora versions ([AWS guidance](https://docs.aws.amazon.com/pdfs/prescriptive-guidance/latest/aurora-postgresql-integration/aurora-postgresql-integration.pdf)), Cloud SQL since 2020-05-11 ([release notes](https://docs.cloud.google.com/sql/docs/postgres/release-notes)), Azure via `azure.extensions` allowlist ([Microsoft](https://learn.microsoft.com/en-us/azure/postgresql/extensions/concepts-extensions-considerations)). Cloud SQL disallows custom extensions ([Google](https://docs.cloud.google.com/sql/docs/postgres/extensions)).
- **The barrier.** `pg_logical_emit_message_bytea` has no privilege check (`replication/logical/logicalfuncs.c:371-382`), and `catalog/system_functions.sql` doesn't revoke from PUBLIC, so weir can run barrier itself over own connection. Whether provider revokes it: unverified.
- **Zero drift via `synchronous_standby_names`** probably not settable on managed services (U).
- **Route C** (wire proxy) and **direct endpoint** (BI tools connect to weir over wire protocol) need nothing from DB.
- **Consequence for question 3:** every remaining route sends weir SQL text, so weir needs parser (`pg_query`).

Direction (jb, 2026-11-08): apps ask weir directly over pgwire with same SQL they'd send PostgreSQL, fall back to PostgreSQL. Barrier runs inside weir. postgres_fdw = optional later door for SQL inside PostgreSQL. No wire proxy. Extension stays optional for self-hosted PostgreSQL, or dropped.

Setup (jb, 2026-11-08): admin runs `weir plan <tables>` once, runs printed role, grant and publication SQL. weir learns rollups from SQL apps prepare; no spec file (ARCHITECTURE.md §2c).

## Open decisions

1. Answered (jb, 2026-11-08): direct endpoint first. postgres_fdw later, optional; no wire proxy.
2. Default freshness: barrier on every query, or async with max-lag check falling back to native plan?
3. Old values: server-side PK-keyed store of projected columns (default replica identity enough), or REPLICA IDENTITY FULL with rollups only?
4. Answered (jb, 2026-11-08): setup by admin from `weir plan` for now. weir publishing tables itself (owner DSN or `SECURITY DEFINER` function) later.
