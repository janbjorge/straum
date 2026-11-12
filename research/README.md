# Straum: research notes

Straum is an aggregate server that sits next to PostgreSQL. The name is the
Nynorsk word for a stream or current: its input is PostgreSQL's replication
stream.

## Name

Before anything is published, check the name for clashes. On 2026-11-09 the
crates `straum`, `straum-domain`, `straum-notation`, `straum-app`,
`straum-pg` and `straum-http` were free on crates.io, and the repository is
`github.com/janbjorge/straum`. Trademark clearance for database software is
still open.

## The idea (jb, 2026-11-08)

- A spec names PostgreSQL tables and columns.
- Straum subscribes to them over logical replication and ingests only that
  subset.
- It keeps the data so that counts, sums, averages, minimums and maximums come
  back about 1000x faster than from PostgreSQL, with near-zero drift.
- Written in Rust.

## Where the design went

The research started from SQL. Could PostgreSQL hand an aggregate query to
Straum by itself? Would that need an extension? Would Straum need a SQL parser?
On managed PostgreSQL every SQL route needed an extension providers don't
allow, a wire proxy in front of all traffic, or Straum parsing SQL itself
(POSTGRES-PLANNER.md, MANAGED-POSTGRES.md).

On 2026-11-08 jb dropped SQL. Aggregates are declared in a spec read at
start, and asked for over HTTP. The spec was Straum's own notation at first and
is now Straum SQL, a PostgreSQL `CREATE VIEW` subset made only for Straum specs
and parsed by `sqlparser`. `docs/src/how-it-works.md` describes that design; `docs/adr/` holds
the decisions.

What carried over from the research:

- **The speed comes from pre-aggregation.** Columnar engines are 31x (DuckDB)
  to 103x (ClickHouse) faster than cached PostgreSQL on ClickBench; 1000x and
  more needs rows that collapse into far fewer groups (COMPETITORS.md §2). M1
  showed that the rollup key decides how far they collapse.
- **Ingest works on managed PostgreSQL** with one parameter and a restart:
  `wal_level = logical` on Azure, `rds.logical_replication` on RDS and Aurora,
  `cloudsql.logical_decoding` on Cloud SQL (MANAGED-POSTGRES.md).
- **Drift is fixable per ask** with a WAL barrier from
  `pg_logical_emit_message(..., flush => true)` (POSTGRES-REPLICATION.md §8).
  v1 reports the LSN of each answer instead.

What it no longer needs: planner hooks, CustomScan, postgres_fdw, pgwire,
pg_query and DataFusion. POSTGRES-PLANNER.md and parts of RUST.md stay as
background in case a SQL door comes back.

## Files

- [ARCHITECTURE.md](ARCHITECTURE.md): how Straum uses PostgreSQL, with
  the citations. Load, apply, freshness, setup, RAM only, `wal_level`.
- [JOINS.md](JOINS.md): a join's semantics at ask time, what it
  replaced, the sources behind the choice, and the numbers.
- [AKS.md](AKS.md): first target, AKS next to Azure Flexible Server 18. Chart
  shape, one-time database setup, risks, the pg_ivm baseline.
- [POSTGRES-REPLICATION.md](POSTGRES-REPLICATION.md): logical replication
  internals. Protocol, publications, old tuples, initial sync, slots,
  consistency, freshness.
- [MANAGED-POSTGRES.md](MANAGED-POSTGRES.md): what AWS, Google Cloud, Azure and
  other providers let customers do themselves.
- [POSTGRES-PLANNER.md](POSTGRES-PLANNER.md): how the planner could hand
  aggregates to an external server. Background for the SQL routes not taken.
- [COMPETITORS.md](COMPETITORS.md): the landscape, ClickBench ratios, dead
  projects, gaps.
- [RUST.md](RUST.md): Rust building blocks.

## Tags

- **R**: read in the PostgreSQL tree, or in linked fetched source.
- **U**: unverified; from a search snippet, a third party, or memory.
- **I**: inference.

PostgreSQL tree: [upstream PostgreSQL](https://github.com/postgres/postgres/tree/0f3b42ce806),
version `20devel` (`meson.build:11`), master `0f3b42ce806` (2026-10-19). The
checkout read had a few unrelated local commits on top; none touch planner, FDW, table AM,
replication, transam or procarray code, so all citations here are upstream
code. Line numbers are from master, not the REL_18 or REL_19 branches. Paths
are relative to `src/backend` unless stated. Four agents did the research on
2026-11-08; the load-bearing citations (`planner.c:4458`, `walsender.c:1604`,
`message.c:78-79`, `xact.c:1488`/`:2446`, `snapbuild.c:478-484`,
`planner.c:7697-7701`) were re-read by hand the same day.

## Target versions

PostgreSQL 17 and newer (jb, 2026-11-09; 18 and newer on 2026-11-08). CI
tests 17, 18 and 19rc1. PG17 lacks the two PG18 items below, which Straum
doesn't use. The tree has no release tags,
so versions are placed by ancestry against the stamp commits: `2652835d3ef`
"Stamp HEAD as 19devel" (2025-06-29, the PG18 branch point) and `a281a3e6dbb`
"Stamp HEAD as 20devel" (2026-06-29, the PG19 branch point). Backpatches are
not accounted for.

Available on PG18 and PG19:
- `create_upper_paths_hook`, `set_rel_pathlist_hook`, `planner_hook`,
  CustomScan, postgres_fdw aggregate pushdown.
- Publication column lists and row filters (PG15), `streaming=parallel`
  (PG16), logical decoding on standby (PG16).
- Failover slots, `sync_replication_slots`, `synchronized_standby_slots`
  (PG17); `pg_logical_emit_message(..., flush)` (`173b56f1ef5`, PG17).
- `idle_replication_slot_timeout` (`ac0e33136ab`) and
  `publish_generated_columns` (`7054186c4eb`), PG18.

PG19 only (not PG18):
- `planner_setup_hook` / `planner_shutdown_hook` (`94f3ad3961a`).
- `build_simple_rel_hook`, which **replaces** `get_relation_info_hook`
  (`91f33a2ae92`). Code building on both should depend on neither.
- `join_path_setup_hook` / `joinrel_setup_hook` (`4020b370f21`).
- Eager aggregation, `enable_eager_aggregate` (`8e11859102f`).
- `WAIT FOR` (`447aae13b03`) and its write/flush modes (`7a39f43d885`). Not
  usable by Straum either way (POSTGRES-REPLICATION.md §8b).
- postgres_fdw `import_stats` / `ImportForeignStatistics` (`28972b6fc3d`).
- Publications `FOR ALL SEQUENCES` (`96b37849734`) with sequence sync
  (`5509055d695`), and `EXCEPT TABLE` (`fd366065e06`).
- Logical decoding turned on automatically while a logical slot exists, with
  `wal_level=replica` (`67c20979ce7`). A PG18 primary needs
  `wal_level=logical`, which requires a restart.

Tooling: pgrx 0.19.3 covers PG15–19 (RUST.md §5). supabase/etl lists
PG14–18; PG19 support is unverified.
