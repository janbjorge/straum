# Rust building blocks

Web research, 2026-11-08. Every claim has source URL. **I** = inference; **U** = third-party claim or search snippet, not fetched from primary source.

Pipeline: PostgreSQL change capture -> columnar store + rollups -> query path back from PostgreSQL.

## 1. SQL parsing

- **sqlparser** (apache/datafusion-sqlparser-rs) 0.63.0, 2026-09-13, Apache-2.0 ([crates.io](https://crates.io/api/v1/crates/sqlparser)). DataFusion 55.2.0 pins `^0.62.0` ([deps](https://crates.io/api/v1/crates/datafusion/55.2.0/dependencies)).
  - PostgreSqlDialect permissive. Issue #2237 tested vs libpg_query: accepts 37 of 129 (28.7%) statements PostgreSQL rejects ([#2237](https://github.com/apache/datafusion-sqlparser-rs/issues/2237)).
  - Open gaps: `?` JSON operator without space (#1992), PG18 virtual generated columns (#2407), `*=`/`@@@` tokenizing (#2809, on 0.63), unary minus vs `^` precedence (#2819, DataFusion #27094), json_to_record (#1375).
  - Users: DataFusion. ReadySet moved from nom-sql to own sqlparser-rs fork ([post](https://dev.to/readyset/from-parser-combinators-to-hand-written-tdop-adopting-sqlparser-rs-at-readyset-22ml)). GreptimeDB keeps synced fork ([#3796](https://github.com/GreptimeTeam/greptimedb/pull/3796), [#6634](https://github.com/GreptimeTeam/greptimedb/pull/6634)). RisingWave `src/sqlparser` is fork ([#27694](https://github.com/risingwavelabs/risingwave/pull/27694)). Materialize forked 2019; repo archived, parser now in-tree ([repo](https://github.com/MaterializeInc/sqlparser)).
- **pg_query** (pganalyze/pg_query.rs) 18.1.0, 2026-10-27, MIT. Embeds real PG 18.6 parser via libpg_query 18.1.0. Versioning jumped 6.2.1 (PG17) -> 18.x to track libpg_query major ([CHANGELOG](https://raw.githubusercontent.com/pganalyze/pg_query.rs/main/CHANGELOG.md)).
  - Parse to protobuf AST, deparse (now DeparseOptions + precedence-aware parens), normalize, fingerprint (PG18 queryid semantics, PG17 compat flag), scan, parse_plpgsql ([repo](https://github.com/pganalyze/pg_query.rs)).
  - Build deps: bindgen 0.66.1, cc, prost-build 0.13.5 ([Cargo.toml](https://raw.githubusercontent.com/pganalyze/pg_query.rs/main/Cargo.toml)), so C compiler + libclang (I). Compiles + statically links PostgreSQL sources.
  - PgDog forked to drop protobuf, reports 5.5x faster parse, 9.6x faster deparse (U, [post](https://medium.com/@trivajay259/we-made-postgresql-query-parsing-5-10-faster-by-ditching-protobuf-heres-how-2163d63b9363)).
- **pg_parse** (paupino) 0.15.0, June 2026: libpg_query via JSON interface, serde only ([lib.rs](https://lib.rs/crates/pg_parse)).

Use for weir:
- Route A (planner hook): no parser; extension sends compact request.
- Route B (postgres_fdw): `pg_query` for deparsed dialect, fidelity.
- Route C (proxy): `pg_query`.
- `pg_query` fingerprints also good cache + rollup-match keys.

## 2. Query engine

- **DataFusion** 55.2.0, 2026-10-07, Apache-2.0, needs arrow `^59.2` ([crates.io](https://crates.io/api/v1/crates/datafusion)). Majors ~every two months (54.0 June, 55.0 August).
  - 55.0: multi-column GROUP BY fast path for every column type in GroupValuesColumn. ClickBench partitioned Parquet: fastest on c7a.metal-48xlarge, second on c6a.4xlarge ([blog](https://datafusion.apache.org/blog/output/2026/08/25/datafusion-55.0.0/)).
  - Earlier: skip partial aggregation at high cardinality, columnar group storage ([blog](https://datafusion.apache.org/blog/2024/11/18/datafusion-fastest-single-node-parquet-clickbench/)).
  - Regression risk: TPC-DS Q75 regressed in 55 (#25157, cited in [#25310](https://github.com/apache/datafusion/issues/25310)).
  - **Pitfall:** UDAF without `GroupsAccumulator` falls back to `GroupsAccumulatorAdapter`. At Spice took ClickBench q33 from 806–927 ms to 7.7–8.4 s ([PR](https://github.com/spiceai/spiceai/pull/16066)).
- Extension points:
  - TableProvider ([guide](https://datafusion.apache.org/library-user-guide/custom-table-providers.html)), OptimizerRule + ExecutionPlan ([guide](https://datafusion.apache.org/library-user-guide/extending-operators.html)), UDAFs.
  - `GroupsAccumulator` "harder to implement than Accumulator, but can be much faster for queries with many group values", with `convert_to_state` for skip-partial-aggregation path ([docs](https://docs.rs/datafusion/latest/datafusion/logical_expr/trait.GroupsAccumulator.html)).
  - `Accumulator::retract_batch` documented only for bounded window frames ([docs](https://docs.rs/datafusion/latest/datafusion/logical_expr/trait.Accumulator.html)). DataFusion has no IVM.
  - DataFusion inside pgrx extension proven: ParadeDB pg_search lists it as primary dep, has `customscan/datafusion` module ([code](https://github.com/paradedb/paradedb/tree/main/pg_search/src/postgres/customscan)).
- **DuckDB** via `duckdb` crate 1.20000.1, 2026-10-28, MIT ([crates.io](https://crates.io/api/v1/crates/duckdb)). supabase/etl pins duckdb-rs rev that "bundles DuckDB v2.0.0", so 1.20000.x likely = DuckDB 2.0.0 (I, [Cargo.toml](https://raw.githubusercontent.com/supabase/etl/main/Cargo.toml)). Strong aggregation, but C++ build, fewer Rust-level hooks (I).
- **Polars** 0.55.2, 2026-08-06, MIT ([crates.io](https://crates.io/api/v1/crates/polars)). DataFrame lib, not SQL server engine; skip (I).
- **Arrow Flight SQL**: arrow-flight 60.1.0 has `flight-sql` feature, no longer `-experimental` ([crates.io](https://crates.io/api/v1/crates/arrow-flight)). Option for columnar link extension<->server, but needs gRPC + tokio inside PostgreSQL backend; pgrx says threads must not touch PostgreSQL internals.
- **Substrait**: substrait 0.65.0 (2026-08-27, Apache-2.0, [crates.io](https://crates.io/api/v1/crates/substrait)); datafusion-substrait 55.2.0 has producer + consumer ([code](https://github.com/apache/datafusion/tree/main/datafusion/substrait)). No producer from PostgreSQL Query/Plan tree to Substrait found; pg_substrait only consumes Substrait inside PostgreSQL ([repo](https://github.com/PrimeDataConversion/substrait-postgres)). Shipping Substrait from extension = write that producer; compact own request or deparsed SQL cheaper.

## 3. Wire protocol server

- **pgwire** 0.42.1, 2026-10-21, MIT/Apache-2.0 ([crates.io](https://crates.io/api/v1/crates/pgwire)).
  - Simple + extended query, COPY in/out/both, cancel, protocol 3.0 + 3.2, logical replication server + client APIs.
  - SSL incl. PG17 direct SSL; cleartext, MD5, SCRAM-SHA-256(-PLUS), OAUTH auth.
  - Users: GreptimeDB, PeerDB, SpacetimeDB, corrosion (fly.io), risinglight, dozer ([repo](https://github.com/sunng87/pgwire)). Client API still WIP.
- **datafusion-postgres** 0.19.0, 2026-10-20, Apache-2.0. Deps: datafusion `^55`, pgwire `^0.41` (not 0.42), arrow-pg `^0.16` ([deps](https://crates.io/api/v1/crates/datafusion-postgres/0.19.0/dependencies)).
  - Ships datafusion-pg-catalog (pg_catalog emulation), arrow-pg (Arrow -> PostgreSQL type encoding), TLS.
  - Tested with psql, DBeaver, pgcli, DataGrip, Metabase, Grafana, Power BI; postgres_fdw not mentioned. Small project, 165 stars ([repo](https://github.com/datafusion-contrib/datafusion-postgres)).
- **postgres_fdw as client**: exact statements in POSTGRES-PLANNER.md §2. Public sources agree ([docs](https://www.postgresql.org/docs/18/postgres-fdw.html), [interdb](https://www.interdb.jp/pg/pgsql04/01.html), [ongres](https://ongres.com/blog/boost-query-performance-using-fdw-with-minimal-changes/)). Greenplum failed as remote on exactly these ([mail](https://www.postgresql.org/message-id/tencent_3C49A923328D53C12A46570C@qq.com)).

## 4. Logical replication client

- **tokio-postgres** 0.7.18 (2026-06-12) exposes no replication or copy_both API ([crates.io](https://crates.io/api/v1/crates/tokio-postgres); [Iggy discussion](https://github.com/apache/iggy/discussions/3842), August 2026). Upstream issue open since 2015 ([#116](https://github.com/sfackler/rust-postgres/issues/116)).
- **Fork lineage**: sfackler -> MaterializeInc/rust-postgres (staging fork, no releases; consumers pin commit, [repo](https://github.com/materializeInc/rust-postgres)) -> supabase/rust-postgres ([repo](https://github.com/supabase/rust-postgres)).
  - Fork's `postgres-replication` 0.6.7 (MIT/Apache, by Petros Angelatos of Materialize) decodes pgoutput v1 tags B/M/C/O/R/Y/I/U/D/T, tuple kinds n/u/t/b.
  - No v2 streaming tags (Stream Start/Stop/Commit/Abort) in `protocol.rs` (I, from [the file](https://raw.githubusercontent.com/supabase/rust-postgres/master/postgres-replication/src/protocol.rs)).
- **supabase/etl**: Apache-2.0, 2.4k stars, PostgreSQL 14–18 (16+ when replicating from physical standby) ([repo](https://github.com/supabase/etl)).
  - Initial copy + catch-up; ordered insert/update/delete/truncate events; column + row filters via publications (PG15+); custom destinations.
  - Destinations: ClickHouse, BigQuery, DuckLake, Snowflake in alpha; Iceberg deprecated; schema-change handling beta.
  - Not on crates.io; git dep.
  - pgoutput decoding ([code](https://github.com/supabase/etl/blob/main/crates/etl/src/replication/decoding_state.rs)). Depends on supabase/rust-postgres pinned rev 8086f1f, plus arrow/parquet 57.0.0, iceberg 0.8.0 ([Cargo.toml](https://raw.githubusercontent.com/supabase/etl/main/Cargo.toml)).
  - Production user: Feldera `postgres_cdc_input` runs on Feldera's own fork. Known gaps: initial copy not resumable, blocks checkpoints ([#7172](https://github.com/feldera/feldera/issues/7172)); multi-table startup bug ([#7406](https://github.com/feldera/feldera/issues/7406)); see also [#7139](https://github.com/feldera/feldera/issues/7139).
- **pg_walstream** 0.10.0, 2026-11-02, BSD-3-Clause, 27k recent downloads ([crates.io](https://crates.io/api/v1/crates/pg_walstream)). Protocol v1–4 incl. parallel streaming, full decoding, rustls, pgoutput encoder, typed WalRouter ([crate](https://crates.io/crates/pg_walstream), [lib.rs](https://lib.rs/crates/pg_walstream)). Single maintainer (I).
- **pgwire-replication** 0.5.0, 2026-10-19, Apache/MIT, 248k recent downloads ([crates.io](https://crates.io/api/v1/crates/pgwire-replication)). Transport only: slot streaming, LSN feedback, TLS, SCRAM; no row decoding ([docs](https://docs.rs/pgwire-replication/latest/pgwire_replication/)). Young, single maintainer (Iggy discussion).
- **Mooncake-Labs/pg_replicate**: hard fork, no commits since April 2025 (U, Iggy discussion).
- **How others do it**:
  - Materialize: pgoutput, one slot per source ([docs](https://materialize.com/docs/sql/create-source/postgres/)); code in `src/storage/src/source/postgres/{replication,snapshot}.rs` ([code](https://github.com/MaterializeInc/materialize/tree/main/src/storage/src/source/postgres)); BSL 1.1.
  - ReadySet: `replicators/src/postgres_connector/` ([code](https://github.com/readysettech/readyset/blob/main/replicators/src/postgres_connector/connector.rs)) + own rust-postgres fork; BSL 1.1.
  - RisingWave: not pure Rust; embedded Debezium reads slot ([docs](https://docs.risingwave.com/ingestion/cdc-with-risingwave)).

## 5. Extension side

- **pgrx** 0.19.3, 2026-09-27, MIT, maintained by PgCentral Foundation ([crates.io](https://crates.io/api/v1/crates/pgrx)).
  - Feature flags pg15–pg19 only; pg13, pg14 gone ([Cargo.toml](https://raw.githubusercontent.com/pgcentralfoundation/pgrx/develop/pgrx/Cargo.toml)).
  - Caveats: threads must not call PostgreSQL internals; panic -> ERROR; lots of unsafe with poorly defined conditions; async "unexplored"; pre-1.0, breaking changes likely ([repo](https://github.com/pgcentralfoundation/pgrx)).
- **No safe CustomScan wrapper.** Use raw `pg_sys::CustomScanMethods`, `CustomPath`, `CustomExecMethods` + hook globals ([docs](https://docs.rs/pgrx-pg-sys/latest/pgrx_pg_sys/)).
- **Best Rust reference: ParadeDB pg_search** (AGPL-3.0: read, don't copy).
  - `hook.rs` registers `set_rel_pathlist_hook`, `create_upper_paths_hook`, `planner_hook`, chains previous hooks ([hook.rs](https://github.com/paradedb/paradedb/blob/main/pg_search/src/postgres/customscan/hook.rs)).
  - AggregateScan built at UPPERREL_FINAL ([PR #6845](https://github.com/paradedb/paradedb/pull/6845)).
- **pg_analytics** (FDW + executor hook into DuckDB): archived 2025-03-19 ([repo](https://github.com/paradedb/pg_analytics)).
- **pg_mooncake**: MIT, Rust/pgrx + DuckDB via pg_duckdb, PG 14–18 ([repo](https://github.com/Mooncake-Labs/pg_mooncake)).
- **pg_duckdb**: MIT, PG 14–18, C++ (I) ([repo](https://github.com/duckdb/pg_duckdb)).
- **supabase/wrappers**: Rust FDW framework on pgrx, WHERE/ORDER BY/LIMIT pushdown; aggregate pushdown not mentioned ([repo](https://github.com/supabase/wrappers)).
- **I** Network I/O from backend: CustomScan must talk to weir synchronously from backend thread, stay interruptible (latch/socket waits with interrupt checks), since pgrx forbids PostgreSQL calls from other threads.

## 6. Storage

- **arrow**: latest 60.1.0 (2026-10-28), but DataFusion 55.2 needs 59.x ([crates.io](https://crates.io/api/v1/crates/arrow)).
- **Hot path (I)**: no table format fits as hot mutable store. Moonlink pattern: buffer in memory, flush to Parquet, map deletes to deletion vectors via row-position index, cache on NVMe, commit to Iceberg v3 ([repo](https://github.com/Mooncake-Labs/moonlink)).
- **iceberg** 0.11.0 (2026-11-03): reads v3 deletion vectors (`DeleteVector::deserialize`, per third-party [PR](https://github.com/exasol-labs/lakehouse-engine-rs/pull/512)); write support U. Spec allows one deletion vector per data file, stored as Puffin ([spec](https://iceberg.apache.org/spec/)).
- **deltalake** 1.1.2 (2026-10-17): reads deletion vectors, never writes; update/delete/merge copy-on-write ([#4512](https://github.com/delta-io/delta-rs/issues/4512)).
- **lance** 14.0.2 (2026-10-29): per-fragment roaring deletion files + compaction; small frequent upserts weak due to read-before-write + commit conflicts ([format](https://lance.org/format/table/), [discussion](https://github.com/lance-format/lance/discussions/3842)).
- **vortex** 0.93.0 (2026-11-06, Apache-2.0, Linux Foundation): compute kernels on compressed data, DataFusion FileFormat with filter/projection/limit pushdown; aggregation pushdown open ([docs](https://docs.vortex.dev/user-guide/datafusion), [#10221](https://github.com/vortex-data/vortex/issues/10221)). Several releases/week, expect churn (I).
- **roaring** 0.12.0 (MIT/Apache) for delete bitmaps ([crates.io](https://crates.io/api/v1/crates/roaring)).

## 7. Incremental computation

- **MIN/MAX under deletes = hard case.**
  - Differential dataflow paper: Min/Max must keep full input traces ([CIDR 2013](https://www.cidrdb.org/cidr2013/Papers/CIDR13_Paper111.pdf)).
  - Feldera: incremental MAX under deletes tracks whole group ([HN](https://news.ycombinator.com/item?id=41685689)); append-only MIN/MAX much cheaper ([docs](https://feldera.com/docs/sql/aggregates)).
  - Materialize hierarchical reduction: reduce on (key, hash >> shift), shift +4 per level, 16 levels, ≤16 records touched per level per update ([blog](https://materialize.com/blog/robust-reductions-in-materialize/)).
- **dbsp** 0.379.0 (2026-11-07, MIT OR Apache-2.0): Feldera engine as crate, tutorial + Aggregator/Min/Max/Avg/Fold ([docs](https://docs.rs/dbsp/latest/dbsp/operator/index.html)). Near-daily releases (0.377 -> 0.379 over 2026-11-05..07); pin exactly.
- **differential-dataflow** 0.26.0 (2026-11-05, MIT) ([crates.io](https://crates.io/api/v1/crates/differential-dataflow)).
- **Simplest for our scope (I)**: hand-rolled per-group state. SUM, COUNT invertible; AVG = SUM/COUNT. MIN/MAX: per-group `BTreeMap<value, count>`, or Materialize-style buckets.
- **Sketches**:
  - datasketches 0.5.0: official Apache, pure Rust; HLL, theta, tuple, KLL, t-digest, count-min, frequencies, each behind feature flag; little-endian only ([repo](https://github.com/apache/datasketches-rust)).
  - sketches-ddsketch 0.4.0, Apache-2.0 ([crate](https://crates.io/crates/sketches-ddsketch)).
  - tdigests 1.0.1, MIT ([crate](https://crates.io/crates/tdigests)).
  - Sketches merge, can't retract; deletes force per-group or per-partition rebuild (I).

## 8. Existing end-to-end projects

- **Feldera** (Rust, MIT, 2.2k stars): closest to ingest + incremental views. Native PostgreSQL CDC via its etl fork feeds DBSP incremental views incl. GROUP BY aggregates. Ad hoc queries on embedded DataFusion over materialized views ([docs](https://docs.feldera.com/sql/ad-hoc/)). No wire-protocol endpoint found.
- **moonlink** (Rust, BSL, preview): PostgreSQL CDC into Iceberg v3 with deletion vectors; real-time reads via pg_mooncake, DuckDB, or DataFusion ([repo](https://github.com/Mooncake-Labs/moonlink)). Databricks acquired Mooncake fall 2025 ([VentureBeat](https://venturebeat.com/data-infrastructure/databricks-set-to-accelerate-agentic-ai-by-up-to-100x-with-mooncake)).
- **pg_mooncake** (MIT): spec subset, ingest + in-PostgreSQL querying, DuckDB inside PostgreSQL ([repo](https://github.com/Mooncake-Labs/pg_mooncake)).
- **BemiDB**: Go, AGPL-3.0, DuckDB + Iceberg + wire protocol ([HN](https://news.ycombinator.com/item?id=42078067)). Not Rust. Now PgStack; BemiDB history through v1.8.0 in git ([repo](https://github.com/BemiHQ/BemiDB)).
- **supabase/etl with DuckLake destination**: CDC into DuckLake, queried by DuckDB; no query path from PostgreSQL.
- **TimeFusion**: DataFusion + pgwire + Delta, direct ingest, no CDC ([repo](https://github.com/monoscope-tech/timefusion)).
- **ReadySet**: CDC + incremental cache over wire protocol; BSL, not columnar ([repo](https://github.com/readysettech/readyset)).

## Recommended stack

| Crate | Version | Why |
|---|---|---|
| datafusion | 55.2.0 (arrow/parquet 59.x) | Ad hoc query engine: GroupsAccumulator UDAFs, OptimizerRule for rollup rewrite, TableProvider over hot store |
| sqlparser | 0.62.x | Whatever DataFusion pins; don't take 0.63 separately |
| pg_query | 18.1.0 | Real parser: postgres_fdw dialect, proxy routing, fingerprints, parity tests |
| pgwire | 0.42.1 | Wire server. With datafusion-postgres 0.19.0, accept pgwire 0.41, reuse arrow-pg + datafusion-pg-catalog |
| supabase/etl | pinned git rev | Slot mgmt, initial copy, pgoutput decoding, ordered events, destination API; Feldera runs in prod |
| pg_walstream | 0.10.0 | Fallback if crates.io-only deps required |
| pgrx | 0.19.3 | Extension side; PG15+ only |
| roaring | 0.12.0 | Delete bitmaps |
| datasketches | 0.5.0 | Approx distinct counts + quantiles |
| parquet | 59.x | Snapshots + spill only; keep Iceberg, Delta, Lance, Vortex off hot path |
| dbsp | 0.379.0 | Optional, only for general IVM beyond sum/count/avg/min/max |

## Must write ourselves

1. **pgoutput to Arrow apply**: PostgreSQL->Arrow type mapping, PK upsert + delete, unchanged TOAST (`u`) columns, replica identity, TRUNCATE, Relation diffs.
2. **Hot mutable columnar store**: append buffer, PK->row-id index, delete bitmaps, compaction, reads as of applied LSN.
3. **Rollup maintenance**: per-group SUM/COUNT/AVG deltas, MIN/MAX under deletes (multiset or hierarchical buckets), rollup specs from config.
4. **Rollup-aware rewrite**: answer GROUP BY from rollup when keys ⊆ rollup keys, then re-aggregate (sum of sums, sum of counts).
5. **Freshness protocol**: expose applied LSN; implement barrier wait (POSTGRES-REPLICATION.md §8c).
6. **Wire session layer for postgres_fdw**: SETs, `START TRANSACTION ISOLATION LEVEL`, DECLARE/FETCH/CLOSE cursors, COMMIT, minimal pg_catalog.
7. **Extension**: `create_upper_paths_hook` + CustomScan on raw pg_sys; spec matching; compact request to weir; tuple conversion; fallback to heap when ineligible or stale.
8. **Slot + WAL safety**: retained-WAL monitoring, slot cleanup, resumable initial copy (missing in etl per Feldera #7172).
9. **PostgreSQL semantics parity**: numeric exactness, `avg(int)` returns numeric, float summation order, `date_trunc` on timestamptz depends on session TimeZone, collations for text group keys (let PostgreSQL sort above scan).
