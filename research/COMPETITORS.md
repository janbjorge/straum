# Competitors and prior art

Web research, 2026-11-08. Every claim has source URL; **U** = claim seen only in search snippet, third party, or memory. Arithmetic via python3 (`/tmp/cbcalc/calc.py`, not kept).

## 1. Bottom line

- Nothing on market combines all five properties for stock PostgreSQL: planner hook sending GROUP BY on original heap tables to external, change-capture-fed, pre-aggregated store, gated on LSN catch-up. Closest: TimescaleDB 2.27+ automatic cagg rewrites (experimental, hypertables only), AlloyDB columnar engine (proprietary, raw columnar, no rollups), Materialize real-time recency (separate endpoint, private preview).
- "A poor man's TimescaleDB" = wrong analogy. Timescale continuous aggregates run inside PostgreSQL, hypertables only, `time_bucket` groupings only, TSL license. weir closer to MySQL HeatWave or TiDB TiFlash (optimizer offloads to change-stream-fed secondary engine, checks freshness), plus StarRocks/Doris-style automatic rewrite onto rollups.
- Logical-replication ingest = commodity: PeerDB, supabase/etl, moonlink, Materialize, RisingWave, Feldera, Epsio, Readyset, Aurora zero-ETL all do it. Not differentiator.
- Transparent planner delegation on original PostgreSQL tables barely exists: AlloyDB (proprietary), TimescaleDB 2.27 `timescaledb.enable_cagg_rewrites` (experimental), ParadeDB pg_search aggregate custom scan (needs BM25 index and `@@@`). Everything else needs different table name, foreign table, schema or endpoint.
- LSN-consistent reads against external engine: only Materialize real-time recency. TiFlash read index = pattern to copy. No PostgreSQL-side product does it.
- 1000x realistic only with pre-aggregation or disk-bound PostgreSQL (§2).

## 2. ClickBench reality check

Source: raw result JSON from [ClickHouse/ClickBench](https://github.com/ClickHouse/ClickBench), path `<system>/results/<date>/<machine>.json`. Method per README: hot = min(run 2, run 3); per-query ratio = (PostgreSQL + 10 ms) / (engine + 10 ms); geometric mean over 43 queries.

Baselines: c6a.4xlarge = `postgresql/results/20260921/c6a.4xlarge.json`; c6a.metal = `postgresql/results/20260919/c6a.metal.json`. Other files: `duckdb/results/20260511/{c6a.4xlarge,c6a.metal}.json`, `clickhouse/results/20260907/{c6a.4xlarge,c6a.metal}.json`, `umbra/results/20261016/c6a.4xlarge.json`, `datafusion/results/20260820/c6a.4xlarge.json`, `pg_clickhouse/results/20260511/c6a.4xlarge.json`, `pg_duckdb/results/20250904/c6a.4xlarge.json`, `postgresql-indexed/results/20250310/c6a.4xlarge.json`.

Instance specs from memory (U). Data shows PostgreSQL heap 106.5 GB. On c6a.4xlarge PostgreSQL hot runs (~258 s) almost as slow as cold (~270 s) → disk-bound. On c6a.metal hot runs drop to 2.5–167 s.

| Engine (run date) | 4xlarge hot geomean vs PG | 4xlarge median | 4xlarge cold geomean | metal hot geomean | metal cold geomean |
|---|---|---|---|---|---|
| Umbra (2026-10-16) | 4,837x | 4,970x | 960x | n/a | n/a |
| ClickHouse (2026-09-07) | 1,788x | 1,511x | 242x | 103x | 280x |
| pg_clickhouse (2026-05-11) | 1,460x | 1,595x | 145x | n/a | n/a |
| DuckDB (2026-05-11) | 1,091x | 761x | 380x | 31x | 288x |
| DataFusion, Parquet (2026-08-20) | 658x | 421x | 239x | n/a | n/a |
| PostgreSQL with indexes (2025-03-10) | 95x | 126x | 37x | n/a | n/a |
| pg_duckdb on heap tables (2025-09-04) | 1.0x | 1.0x | 1.0x | n/a | n/a |

Findings:
- pg_duckdb on heap = 1.0x: still reads 106 GB heap. Vectorized executor alone buys nothing when I/O-bound; columnar storage matters.
- FDW hop cheap: pg_clickhouse only 1.23x slower than native ClickHouse (hot geomean, 4xlarge).
- Storage: PostgreSQL 106.5 GB vs ClickHouse 9.4 GB (11.3x smaller), DuckDB 20.5 GB (5.2x), Umbra 6.7 GB (15.9x).
- Sum of hot times over 43 queries: PostgreSQL 11,875 s (4xlarge), 763 s (metal); ClickHouse 18.0 s (4xlarge), 4.8 s (metal); Umbra 5.4 s (4xlarge); DuckDB 26.3 s (4xlarge), 17.3 s (metal); pg_clickhouse 33.6 s (4xlarge).
- PostgreSQL 4xlarge vs metal, hot geomean: 36.1x, pure caching + hardware effect.
- ClickHouse Q0 `count(*)` = 1 ms answered from metadata, i.e. already pre-aggregate.
- **Takeaway:** vs well-cached PostgreSQL expect ~30–100x from raw columnar. 1000x needs rollups (rows → groups) or I/O-starved PostgreSQL.
- Vendor claims:
  - pg_duckdb beta "1500x" on one TPC-DS query, 81.8 s → 52 ms (recomputed 1,573x): [PR Newswire](https://www.prnewswire.com/news-releases/motherduck-announces-beta-release-of-pgduckdb-brings-duckdbs-analytics-power-to-postgresql-users-302286647.html)
  - Supabase: pg_duckdb "over 600x": [blog](https://supabase.com/blog/hydra-joins-supabase)
  - ClickHouse: pg_clickhouse "only slightly slower than native ClickHouse": [blog](https://clickhouse.com/blog/pg_clickhouse-fastest-analytics-for-postgres)
  - EDB PGAA "30x faster than standard Postgres": [The Register](https://www.theregister.com/2025/06/20/edb_enhances_analytics_in_postgresql/)
  - AlloyDB, independent TPC-H SF100 Q6: 39x, rising to 311x once row store spills: [boringsql](https://boringsql.com/posts/google-alloydb/)

## 3. External engines fed by PostgreSQL change capture

Format: architecture | routing | freshness and consistency | UPDATE/DELETE | model | language and license | status.

- **Materialize**: differential-dataflow IVM; native PostgreSQL source; requires REPLICA IDENTITY FULL ([FAQ](https://materialize.com/docs/ingest-data/postgres/faq/)) | separate wire-protocol endpoint | strict serializable default; real-time recency (private preview, `SET real_time_recency = true`) guarantees query includes "at least all data visible to Materialize in any external source", at extra latency ([docs](https://materialize.com/docs/get-started/isolation-level/)) | yes | pre-aggregated, any SQL | Rust; BSL 1.1, Apache 2.0 after 4 years ([repo](https://github.com/MaterializeInc/materialize/)) | active; Self-Managed v26 needs license key; Community edition free up to 24 GiB memory / 48 GiB disk ([blog](https://materialize.com/blog/materialize-self-managed-v26-0-0-release/)).
- **RisingWave**: streaming DB with postgres-cdc connector ([blog](https://risingwave.com/blog/postgres-cdc-to-iceberg/)) | separate endpoint | vendor claims "<100 ms" end-to-end freshness, 10–20 ms p99 serving ([repo](https://github.com/risingwavelabs/risingwave)) | yes | pre-aggregated | Rust; Apache-2.0 | active; 3.0 GA 2026-06-11 per third-party wiki (U).
- **Feldera**: DBSP, SQL compiled to incremental Rust ([docs](https://docs.feldera.com/sql/intro/), [paper](https://docs.feldera.com/vldb23.pdf)) | separate endpoint and sinks | README: "strongly consistent" ([repo](https://github.com/feldera/feldera)) | yes | pre-aggregated | Rust; MIT | active; PostgreSQL CDC connector experimental ([docs](https://docs.feldera.com/connectors/sources/)), built on supabase/etl fork ([#7107](https://github.com/feldera/feldera/issues/7107)), needs superuser ([#7142](https://github.com/feldera/feldera/issues/7142)).
- **Epsio**: change capture into incremental engine on separate compute; writes results back to source DB ([docs](https://docs.epsio.io/)) | query result view made by `CALL epsio.create_view`, not base table | "Eventual Consistent, never effecting write performance" ([FAQ](https://docs.epsio.io/faq/)) | yes (U) | pre-aggregated (JOIN, CTE, GROUP BY) | commercial, license U | quiet; 9 employees as of May 2026 per [Tracxn](https://tracxn.com/d/companies/epsio/__c7qST0weCvpHULJz35yTixMULMQZ5VIZA8leQRcohWc); $4.5M seed per [PitchBook](https://pitchbook.com/profiles/company/535213-09).
- **Readyset**: replication stream feeds dataflow; aggregates maintained incrementally on insert, update, delete ([blog](https://readyset.io/blog/behind-the-magic-how-readyset-speeds-up-queries-with-streaming-dataflow)) | wire-compatible proxy; cached queries served, "everything else is proxied straight through" ([docs](https://readyset.io/docs/)) | eventual (I, from "incrementally updates these results over time") | yes | per-query pre-aggregation | Rust; BSL 1.1 ([repo](https://github.com/readysettech/readyset)) | active; query limits at [docs](https://readyset.io/docs/reference/features/queries).
- **pg_mooncake / moonlink**: moonlink consumes logical replication into Iceberg v3 with deletion vectors; reads via pg_duckdb ([architecture](https://docs.mooncake.dev/pg/intro/architecture), [moonlink](https://github.com/Mooncake-Labs/moonlink)) | columnstore mirror table queried in PostgreSQL | README: "sub-second freshness" ([repo](https://github.com/Mooncake-Labs/pg_mooncake)) | yes | raw columnar | Rust (pgrx); MIT (moonlink BSL) | effectively dead: Databricks acquired Mooncake Labs ~October 2025 ([blog](https://www.databricks.com/blog/mooncake-labs-joins-databricks-accelerate-vision-lakebase)); last release v0.1.2 February 2025; "We wouldn't start a new project on it" ([ClickHouse article](https://clickhouse.com/resources/engineering/best-postgres-extensions-for-analytics-2026)).
- **BemiDB**: PostgreSQL → Iceberg, DuckDB engine, wire protocol | separate endpoint | raw columnar | Go; AGPL before v2 | pivoted: renamed PgStack, v2.0.0 "Postgres read replicas for Search and Audit" ([release](https://github.com/pgstack-io/pgstack/releases/tag/v2.0.0)).
- **Supabase ETL**: embeddable Rust logical-replication engine ([repo](https://github.com/supabase/etl)) | no query path, pipeline only | at-least-once; initial copy holds ACCESS SHARE locks | yes | n/a | Rust; Apache-2.0 | public alpha; destinations ClickHouse, BigQuery, DuckLake, Snowflake; Iceberg destination deprecated; PostgreSQL 14–18. Hydra team joined Supabase 2026-02-10 ([blog](https://supabase.com/blog/hydra-joins-supabase)).
- **PeerDB / ClickPipes**: logical replication into `ReplacingMergeTree(_peerdb_version)` with `_peerdb_is_deleted` ([docs](https://clickhouse.com/docs/integrations/clickpipes/postgres/deduplication)) | separate ClickHouse endpoint, or PostgreSQL via pg_clickhouse | async; exact results need `FINAL` and `_peerdb_is_deleted = 0`; Managed Postgres default sync interval 60 s | yes, as versioned inserts | raw columnar | AGPL-3.0 ([LICENSE](https://github.com/PeerDB-io/peerdb/blob/main/LICENSE)); README claims "10x faster" CDC and freshness of "a few 10s of seconds" ([repo](https://github.com/PeerDB-io/peerdb)) | ClickHouse acquired PeerDB July 2024 ([blog](https://blog.peerdb.io/clickhouse-acquires-peerdb-for-native-postgres-cdc-integration)); 1,000+ ClickPipes CDC customers ([blog](https://clickhouse.com/blog/postgres-at-open-house-nyc)).
- **pg_clickhouse**: FDW pushing aggregates, filters, GROUP BY, HAVING, semi-joins to ClickHouse ([blog](https://clickhouse.com/blog/introducing-pg_clickhouse), [docs](https://pgxn.org/dist/pg_clickhouse/doc/pg_clickhouse.html)) | foreign tables or dedicated schema, not original tables ([stack](https://github.com/ClickHouse/postgres-clickhouse-stack)) | inherits change-capture lag, no gating | via ClickHouse | raw columnar | C; Apache-2.0 | v0.11.0 (2026-09-30); successor of archived clickhouse_fdw; preinstalled in ClickHouse Managed Postgres ([blog](https://clickhouse.com/blog/postgres-clickhouse-oss)).
- **ClickHouse MaterializedPostgreSQL**: snapshot + LSN, then WAL stream ([docs](https://clickhouse.com/docs/engines/database-engines/materialized-postgresql)) | ClickHouse endpoint | async; TOAST not replicated ([old docs](https://content.clickhouse.tech/docs/en/engines/database-engines/materialized-postgresql/)) | yes | raw columnar | C++ | still experimental; docs point to ClickPipes; not on ClickHouse Cloud ([docs](https://clickhouse.com/docs/engines/table-engines/integrations/materialized-postgresql)).
- **Cube / Cube Store**: semantic layer; rollups in Cube Store (Rust, Parquet, Arrow) ([code](https://github.com/cube-js/cube/tree/master/rust/cubestore)) | Cube API or SQL API over wire protocol ([cubesql](https://lib.rs/gh/cube-js/cube.js/cubesql)) | rollups rebuilt on refresh_key, default hourly ([docs](https://cube.dev/docs/product/caching/using-pre-aggregations)); lambda rollups union batch + live source data ([docs](https://docs.cube.dev/docs/pre-aggregations/lambda-pre-aggregations)) | by rebuild | pre-aggregated | Apache-2.0 | active.
- **Sequin**: change capture to queues and streams | Elixir; MIT | dead upstream: acquired August 2025 (acquirer not named), Sequin Cloud shut October 2025, unmaintained since February 2026, per fork README ([fork](https://github.com/triptechtravel/sequin)).
- **Artie**: log-based change-capture SaaS into warehouses, "sub-minute latency" (vendor, [blog](https://www.artie.com/blogs/best-database-replication-solutions)) | commercial | active.
- **Estuary Flow**: pgoutput capture, exactly-once, sub-100 ms option (vendor; [quickstart](https://github.com/estuary/cdc-quickstart), [article](https://www.smartdatacollective.com/best-postgres-cdc-tools-software-for-real-time-replication/)) | commercial | active.
- **pg_lake** (Snowflake, formerly Crunchy Data Warehouse): Iceberg tables in PostgreSQL; DuckDB in separate pgduck_server process ([repo](https://github.com/Snowflake-Labs/pg_lake)) | transactional writes | raw columnar | Apache-2.0 | v3.5.10 (2026-10-27); no heap-to-Iceberg change capture documented. Snowflake acquired Crunchy June 2025 ([press](https://www.businesswire.com/news/home/20250602455530/en/snowflake-acquires-crunchy-data-to-bring-enterprise-ready-postgres-offering-to-the-ai-data-cloud)).
- **Aurora PostgreSQL zero-ETL to Redshift**: managed change capture | separate Redshift endpoint | "within seconds" ([AWS](https://aws.amazon.com/about-aws/whats-new/2024/10/amazon-aurora-postgresql-zero-etl-integration-redshift-generally-available/)) | yes | raw columnar | proprietary | GA; Aurora PG 16.4+.
- **Tiger Lake** (Tiger Cloud Iceberg connector): change capture to Iceberg on S3 Tables, "eventual consistency" ([docs](https://docs.tigerdata.com/use-timescale/latest/tigerlake)) | proprietary | beta status unclear.
- **EDB Postgres AI Analytics Accelerator**: DataFusion vectorized engine + tiered tables to Iceberg ([docs](https://www.enterprisedb.com/docs/edb-postgres-ai/latest/analytics/)) | pushdown from PostgreSQL | async for tiered data | raw | proprietary | GA.
- **Databricks Lakebase (Neon)**: synced tables Delta → Lakebase; Change Data Feed Lakebase → Delta in public preview ([docs](https://docs.databricks.com/aws/en/oltp/projects/sync-tables)) | separate | async | proprietary | Lakebase GA ([blog](https://www.databricks.com/blog/databricks-lakebase-generally-available)).
- **Azure HorizonDB**: no native columnar found; analytics via Fabric mirroring ([InfoWorld](https://www.infoworld.com/article/4093191/azure-horizondb-microsoft-goes-big-with-postgresql.html)) | public preview.

## 4. Inside PostgreSQL (extensions and forks)

- **TimescaleDB continuous aggregates**:
  - invalidation records per transaction min/max timestamp touched ([blog](https://www.tigerdata.com/blog/continuous-aggregate-refresh-demystified));
  - invalidation trigger removed in 2.23, replaced by tuple routing in core, 10–20% more DML throughput ([blog](https://www.tigerdata.com/blog/timescaledb-2-22-2-23-90x-faster-distinct-queries-postgres-18-support-configurable-columnstore-indexes-uuidv7));
  - WAL-based invalidation: preview 2.22, retired 2.23 ("did not meet our IOPS reduction goals"), removed 2.25.0 ([release](https://github.com/timescale/timescaledb/releases/tag/2.25.0));
  - real-time aggregation unions materialized buckets with raw rows above watermark; off by default since 2.13; changes below watermark serve stale values until next refresh; outside refresh window never fixed (same blog);
  - 2.27.0 added experimental `timescaledb.enable_cagg_rewrites`: planner rewrites hypertable GROUP BY `time_bucket` queries to matching cagg; exact match only, no re-aggregation, needs `materialized_only=false`, not on PG15 ([docs](https://www.tigerdata.com/docs/build/performance-optimization/cagg-query-rewrites));
  - license: core Apache-2.0, Community features TSL ([licenses](https://www.tigerdata.com/legal/licenses)); columnstore TSL per ClickHouse article;
  - latest 2.31.0 (2026-10-20), incl. faster UPDATE/DELETE on columnstore ([releases](https://github.com/timescale/timescaledb/releases)); company renamed TigerData 2025-06-17 ([Wikipedia](https://en.wikipedia.org/wiki/TimescaleDB)).
- **pg_duckdb**: in-process DuckDB; `duckdb.force_execution=true` routes queries on regular tables ([repo](https://github.com/duckdb/pg_duckdb)) | same transaction | yes | vectorized, reads heap | C++; MIT | v1.0 September 2025 ([blog](https://motherduck.com/blog/pg-duckdb-release/)), v1.1.1 December 2025 per ClickHouse article; ClickBench on heap 1.0x.
- **AlloyDB columnar engine**: in-memory column store; planner costs columnar vs row per node via Custom Scan ([blog](https://cloud.google.com/blog/products/databases/alloydb-for-postgresql-columnar-engine)); auto-columnarization hourly default ([flags](https://docs.cloud.google.com/alloydb/docs/reference/columnar-engine-flags)); all referenced columns must be in column store ([docs](https://docs.cloud.google.com/alloydb/docs/columnar-engine/about)) | strongly consistent: DML invalidates blocks, invalid blocks read from row store and merged ([forum](https://discuss.google.dev/t/is-alloydb-eventually-or-strongly-consistent/189447/1), [freshness docs](https://docs.cloud.google.com/alloydb/omni/containers/current/docs/columnar-engine/maintain-content-freshness), [patent](https://image-ppubs.uspto.gov/dirsearch-public/print/downloadPdf/12158844)); refresh at 50% invalid | yes | raw columnar, no rollups | proprietary | active.
- **ParadeDB pg_search**: aggregate custom scan over index's columnar "fast fields"; needs ParadeDB index and `@@@` operator ([limits](https://www.paradedb.com/docs/documentation/aggregates/limitations), [config](https://www.paradedb.com/docs/reference/configuration)) | transactional (I) | raw columnar | AGPL (U) | active, v0.23 ([release](https://github.com/paradedb/paradedb/releases/tag/v0.23.0)). pg_analytics archived 2025-03-19 ([repo](https://github.com/paradedb/pg_analytics)).
- **pg_ivm**: AFTER triggers, view updated "in the same transaction" ([repo](https://github.com/sraoss/pg_ivm)) | exact | count, sum, avg, min, max only | PostgreSQL License | active, PG 13–19; ExclusiveLock on view per write under READ COMMITTED (serializes writers); no partitioned or foreign tables; no HAVING, window, ORDER BY, UNION.
- **pg_incremental**: pg_cron batch pipelines over sequences and time ranges, exactly-once ([repo](https://github.com/CrunchyData/pg_incremental), [blog](https://www.crunchydata.com/blog/pg_incremental-incremental-data-processing-in-postgres)) | append-only focus | PostgreSQL License | active.
- **Citus columnar**: table access method | exact | append-only, no UPDATE/DELETE ([docs](https://docs.citusdata.com/en/latest/admin_guide/table_management.html)) | AGPL-3.0 | ships with Citus 14.3.0 (2026-10-23).
- **Hydra columnar**: Citus columnar fork | no code changes since 2024 (ClickHouse article); team joined Supabase February 2026.
- **pg_fdw_mv_rewrite**: transparent GROUP BY/DISTINCT rewrite onto matviews via planner ([repo](https://github.com/d-e-n-t-y/pg_fdw_mv_rewrite)) | 2018 proof of concept, "not production ready" ([mail](https://www.postgresql.org/message-id/DB608841-3837-48AA-A10E-7F1345208BD8@QQdd.eu)).
- **AnalyticDB for PostgreSQL** (Alibaba fork): matview query rewrite incl. incremental real-time matviews, on by default via `enable_incremental_matview_query_rewrite` ([docs](https://help.aliyun.com/en/analyticdb/analyticdb-for-postgresql/user-guide/query-rewrite-for-materialized-views)) | proprietary fork | closest prior art for in-planner rewrite.
- **PG-Strom**: GPU offload | PostgreSQL License | v6.1 ([docs](https://heterodb.github.io/pg-strom/install/)).
- **Swarm64**: columnar indexes + parallelism via planner hooks | acquired by ServiceNow August 2021 ([article](https://www.itbusinessedge.com/database/servicenow-acquires-swarm64/)); product ended 2021 ([dbdb](https://dbdb.io/db/swarm64)).

## 5. Analogs in other databases (patterns to copy)

- **MySQL HeatWave**: optimizer offloads when all operators supported and estimate cheaper; Dynamic Query Offload (9.0+) also weighs change-propagation state ([docs](https://dev.mysql.com/doc/heatwave/en/mys-hw-dynamic-query-offload.html)). Propagation every 200 ms, at 64 MB, or when HeatWave query reads DML-changed data; DML not delayed; stale tables not offloaded ([docs](https://dev.mysql.com/doc/heatwave/en/mys-hw-change-propagation.html)). Raw columnar.
- **TiDB TiFlash**: cost-based optimizer picks TiKV or TiFlash ([docs](https://docs.pingcap.com/tidb/stable/use-tidb-to-read-tiflash/)). Raft learner, async replication; at read time checks leader's read index, waits until applied → snapshot isolation ([overview](https://docs.pingcap.com/tidb/stable/tiflash-overview/), [design](https://github.com/pingcap/tiflash/blob/master/docs/design/0000-00-00-architecture-of-distributed-storage-and-transaction.md), [VLDB paper](https://www.vldb.org/pvldb/vol13/p3072-huang.pdf)). Raw columnar.
- **Oracle Database In-Memory**: optimizer routes; dual format, transactionally consistent as of query SCN ([whitepaper](https://www.oracle.com/a/tech/docs/twp-oracle-database-in-memory-19c.pdf), [VLDB](http://www.vldb.org/pvldb/vol8/p1630-mukherjee.pdf)).
- **SQL Server nonclustered columnstore**: optimizer; delta rowgroups + delete bitmap; COMPRESSION_DELAY ([docs](https://learn.microsoft.com/en-us/sql/relational-databases/indexes/get-started-with-columnstore-for-real-time-operational-analytics?view=sql-server-ver17), [blog](https://techcommunity.microsoft.com/blog/sqlserver/real-time-operational-analytics-dml-operations-and-nonclustered-columnstore-inde/384639)).
- **SingleStore**: unified table = in-memory rowstore segment + columnstore segments ([dbdb](https://dbdb.io/db/singlestore), [blog](https://www.singlestore.com/blog/pushing-htap-databases-forward-with-singlestoredb/)).
- **StarRocks async matviews**: automatic rewrite, by default only when matview consistent with base tables; `mv_rewrite_staleness_second` relaxes; partition UNION rewrite combines fresh matview partitions with base-table data ([docs](https://docs.starrocks.io/docs/using_starrocks/async_mv/use_cases/query_rewrite_with_materialized_views/)).
- **Apache Doris async matviews**: SPJG transparent rewrite; grace_period; valid partitions + UNION ALL of base ([docs](https://doris.apache.org/docs/dev/key-features/incremental-materialized-view/), [docs](https://doris.apache.org/docs/3.x/query-acceleration/materialized-view/async-materialized-view/functions-and-demands/)).
- **Oracle matview query rewrite**: REFRESH FAST ON COMMIT via matview logs; MIN/MAX fast refresh only for inserts ([docs](https://docs.oracle.com/en/database/oracle/oracle-database/19/dwhsg/refreshing-materialized-views.html), [docs](https://docs.oracle.com/en/database/oracle/oracle-database/26/dwhsg/basic-materialized-views.html)).
- **Apache Druid**: ingestion-time rollup ([docs](https://druid.apache.org/docs/latest/ingestion/ingestion-spec/)); immutable segments, updates = time-range overwrites ([docs](https://druid.apache.org/docs/latest/data-management/update/)).
- **Apache Pinot**: star-tree pre-aggregation at segment build + primary-key upsert ([paper](https://arxiv.org/pdf/2104.00087), [comparison](https://www.jusdb.com/compare/druid-vs-pinot)).
- **Apache Kylin**: cubes; v5.0.5 (2026-07-13), status "Ongoing" ([release notes](https://kylin.apache.org/docs/release_notes/), [board minutes](https://whimsy.apache.org/board/minutes/Kylin.html)).

Patterns for weir: TiFlash read index (wait until replicated LSN >= LSN at query start); HeatWave query-triggered propagation + "never offload stale tables"; StarRocks/Doris partition UNION (fresh rollup partitions + base-table delta).

## 6. Dead, pivoted, acquired

- **ParadeDB pg_analytics**: archived 2025-03-19; work consolidated into pg_search, other tools already read object stores ([changelog](https://api.pgxn.org/src/pg_search/pg_search-0.16.2/docs/changelog/0.15.9.mdx)).
- **clickhouse_fdw**: archived; README points to pg_clickhouse.
- **pg_mooncake**: Databricks acqui-hire ~October 2025, folded into Lakebase; last release February 2025.
- **BemiDB**: renamed PgStack, repositioned to search + audit replicas; no reason stated.
- **Hydra**: columnar dormant since 2024; team joined Supabase February 2026.
- **Sequin**: acquired August 2025, cloud shut October 2025, upstream unmaintained February 2026; acquirer not named.
- **Tembo**: managed Postgres shut 2025 (no new instances from 2025-05-05), pivoted to coding agents ([HN](https://news.ycombinator.com/item?id=44038896), [Neon guide](https://neon.com/guides/migrate-tembo-to-neon), [Pavlo retrospective](https://www.cs.cmu.edu/~pavlo/blog/2026/01/2025-databases-retrospective.html)).
- **Swarm64**: acquired by ServiceNow August 2021, product ended 2021; no public reason.
- **TimescaleDB WAL-based invalidation**: removed; missed IOPS goals.
- **supabase/etl Iceberg destination**: deprecated; no reason stated.

Money pattern: consolidation went to PostgreSQL-to-lakehouse sync (Databricks bought Neon + Mooncake; Snowflake bought Crunchy; ClickHouse bought PeerDB) ([VentureBeat](https://venturebeat.com/data-infrastructure/enterprise-alert-postgresql-just-became-the-database-you-cant-ignore-for-ai-applications)). Nobody funded planner-level transparency.

## 7. Gap analysis

Properties: (a) spec-driven subset, (b) logical-replication ingest, (c) PostgreSQL planner transparently routes queries on original tables, (d) LSN-consistent reads or near-zero drift, (e) pre-aggregation handling UPDATE/DELETE.

| Product | a | b | c | d | e |
|---|---|---|---|---|---|
| Materialize | yes (publication, EXCLUDE COLUMNS) | yes | no, own endpoint | yes, real-time recency (preview) | yes |
| RisingWave, Feldera | yes | yes (Feldera experimental) | no | U | yes |
| Epsio | yes | yes | partial: results in same DB, different object | no, eventual | yes |
| Readyset | per query | yes | partial: wire proxy, same SQL text | no (I) | yes |
| PeerDB + pg_clickhouse | yes | yes | partial: foreign tables or schema | no: async, FINAL | raw only |
| TimescaleDB caggs + 2.27 rewrite | no (hypertables) | no (in-process) | yes but experimental, exact match | partial: stale below watermark | partial |
| AlloyDB columnar | n/a | no (in-process) | yes | yes | no (raw) |
| pg_ivm | n/a | no (triggers) | no | yes | yes, high write cost |
| ParadeDB pg_search | n/a | no | partial: needs `@@@` and BM25 index | yes (I) | no |
| AnalyticDB PG fork | n/a | n/a | yes | yes | yes; proprietary fork |
| HeatWave, TiFlash (not PostgreSQL) | n/a | yes (equivalent) | yes | yes | no (raw) |

Already covered:
- (a)+(b) fully commoditized; supabase/etl = embeddable Rust core Feldera already builds on.
- (b)+(e) on separate endpoint crowded: Materialize, RisingWave, Feldera, Epsio, Readyset.
- (c)+(d) with raw columnar inside PostgreSQL: AlloyDB, proprietary, managed only.

Unoccupied:
1. (c)+(d)+(e) for stock PostgreSQL: external change-capture-fed rollup store PostgreSQL planner uses for GROUP BY on original heap tables. Nearest: TimescaleDB (hypertables, `time_bucket`, exact match, experimental, TSL).
2. LSN-gated reads from external analytical engine through PostgreSQL: capture LSN at query start, then wait, fall back to heap, or union rollup + delta. TiFlash and HeatWave do it; no PostgreSQL extension or FDW does. pg_clickhouse inherits async lag, no gating.
3. Rollups keeping MIN/MAX correct under DELETE without write locks: Oracle restricts MIN/MAX fast refresh to inserts; pg_ivm handles it but takes write locks.

Caveat (I): planner hooks need installable extension. RDS and Cloud SQL allowlist extensions, so on stock managed PostgreSQL only transparent path = wire proxy (Readyset model). ClickHouse sidesteps by preinstalling pg_clickhouse in own Managed Postgres.
