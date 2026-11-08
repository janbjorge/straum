# weir on managed PostgreSQL

Web research 2026-11-08, four agents (AWS, Google Cloud, Azure, other providers). jb constraint: weir must work on managed PostgreSQL using only customer self-serve (console, parameters, SQL). No vendor tickets, no allowlisting requests. Tags as README.md: **U** = unverified (no fetched doc confirms), **I** = inference.

## What weir needs from the database

| Need | What it means |
|---|---|
| Ingest | Logical replication w/ pgoutput; role with REPLICATION; `CREATE PUBLICATION ... FOR TABLE` |
| Transparent queries | Custom planner hook or CustomScan in PostgreSQL (route A) |
| In-database queries | postgres_fdw (or other FDW pushing down aggregates) reaching weir over network |
| Freshness barrier | `pg_logical_emit_message(..., flush => true)` callable by weir role |
| Zero drift | weir in `synchronous_standby_names` |
| Bounded WAL | `max_slot_wal_keep_size` settable |
| Slot survival | Logical slots survive HA failover, switchover, major-version upgrades |

PostgreSQL source facts behind privileges (master `0f3b42ce806`):
- `CREATE PUBLICATION` needs CREATE on database (`commands/publicationcmds.c:856`); `FOR TABLE` needs ownership of each table (`:2052`); `FOR ALL TABLES` (`:862-867`) and `FOR TABLES IN SCHEMA` (`:963-966`) need real superuser, which no managed service gives.
- Replication slots need REPLICATION attribute (`replication/slot.c:1700-1707`).
- `pg_logical_emit_message_bytea` has no privilege check (`replication/logical/logicalfuncs.c:371-382`), not revoked from PUBLIC in `catalog/system_functions.sql`.
- postgres_fdw `password_required=false` superuser-only, so user mappings must carry password.

## Verdict

- **Ingest: covered** on AWS (RDS, Aurora), Google Cloud (Cloud SQL, AlloyDB), Azure Flexible Server, Supabase, Neon, Crunchy Bridge, Snowflake Postgres, DigitalOcean Standard, Aiven, PlanetScale. Usually static parameter + one restart. Not on Heroku Essential/Standard/Premium; unclear on Tiger Cloud, DigitalOcean Advanced, ClickHouse Managed Postgres, Databricks Lakebase.
- **Transparent queries: blocked everywhere.** No provider lets customer load own C code. pg_tle (AWS, Supabase) exposes only `passcheck` and `clientauth` hooks ([pg_tle hooks](https://github.com/aws/pg_tle/blob/main/docs/04_hooks.md)). PL/Rust removed from RDS from PostgreSQL 18. Route A impossible without vendor.
- **In-database queries: mostly covered by postgres_fdw**, shipped by almost every provider. Caveat: network reach to weir; Cloud SQL weakest (only Cloud SQL private-IP targets documented).
- **Freshness barrier: likely covered** (upstream grants to PUBLIC); no provider documents revoking. Test per provider.
- **Zero drift: blocked.** `synchronous_standby_names` read-only on Azure, absent from Cloud SQL and AlloyDB flag lists, not documented modifiable on AWS.
- **Bounded WAL: partial.** Settable on RDS, Aurora; read-only `-1` on Azure; conflicting docs on Cloud SQL; absent on AlloyDB.
- **Slot survival: weak.** Major upgrades drop slots on Cloud SQL, Aurora, AlloyDB, Aiven, Supabase; several services drop idle/stalled slots. weir must treat "slot gone, resync" as normal path.
- **Versions:** PostgreSQL 18 GA on RDS, Aurora, Cloud SQL, AlloyDB, Azure, Neon, Crunchy Bridge, Snowflake Postgres, DigitalOcean, Aiven, Heroku, PlanetScale, Tiger Cloud; Supabase hosted not on 18 yet (U). PostgreSQL 19 GA nowhere managed: community RC1 2026-10-15, GA planned 2026-11-19 ([news](https://www.postgresql.org/about/news/postgresql-19-rc-1-released-3409/)); RDS has 19 RC1 only in Database Preview Environment.

## Hyperscalers

### AWS: RDS for PostgreSQL and Aurora PostgreSQL

- **Versions:** RDS PG18 GA 2025-11-14, latest 18.6 ([calendar](https://docs.aws.amazon.com/AmazonRDS/latest/PostgreSQLReleaseNotes/postgresql-release-calendar.html)); PG19 RC1 only in preview env, not production, instances deleted after 60 days ([news](https://aws.amazon.com/about-aws/whats-new/2026/10/postgresql-19-rc1-amazon-rds-database-preview-environment/)). Aurora PG18 GA 2026-06-11 ([news](https://aws.amazon.com/about-aws/whats-new/2026/06/amazon-aurora-postgresql-major-version-18/)); no Aurora PG19.
- **Ingest:**
  - `rds.logical_replication=1` (static, reboot); enabling needs `rds_superuser`, slots/streaming need `rds_replication` ([RDS](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/PostgreSQL.Concepts.General.FeatureSupport.LogicalReplication.html), [Aurora](https://docs.aws.amazon.com/AmazonRDS/latest/AuroraUserGuide/AuroraPostgreSQL.Replication.Logical.Configure.html)). pgoutput supported.
  - Master user NOSUPERUSER NOREPLICATION ([roles](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/Appendix.PostgreSQL.CommonDBATasks.Roles.rds_superuser.html)); `FOR TABLE` follows upstream rule (I).
  - RDS read replicas decode from PG16; PG17+ slots sync to replica. Aurora readers can't be logical source ([DMS](https://docs.aws.amazon.com/dms/latest/userguide/CHAP_Source.PostgreSQL.html), [Aurora](https://docs.aws.amazon.com/AmazonRDS/latest/AuroraUserGuide/AuroraPostgreSQL.Replication.Logical.html)).
- **Slot survival:**
  - RDS PG17+ failover slots w/ `sync_replication_slots`, `synchronized_standby_slots`, `hot_standby_feedback`, `rds.logical_slot_sync_dbname`, `failover=true`, documented for read-replica promotion ([doc](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/Appendix.PostgreSQL.CommonDBATasks.pglogical.slot.synchronization.html)). Multi-AZ instance failover and Aurora writer failover: U.
  - Major upgrade: RDS keeps slots on non-replicas from source 17+ if fully consumed ([doc](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/USER_UpgradeDBInstance.PostgreSQL.MajorVersion.Process.html)); Aurora requires dropping all slots ([doc](https://docs.aws.amazon.com/AmazonRDS/latest/AuroraUserGuide/USER_UpgradeDBInstance.PostgreSQL.MajorVersion.html)). Blue/green switchover drops logical slots ([blog](https://aws.amazon.com/blogs/database/migrate-amazon-aurora-postgresql-across-major-versions-with-active-debezium-cdc-connectors-using-native-logical-replication/)). Toggling `aurora.enhanced_logical_replication` invalidates all slots ([doc](https://docs.aws.amazon.com/AmazonRDS/latest/AuroraUserGuide/zero-etl.setting-up.html)).
  - `max_slot_wal_keep_size` settable on RDS ([doc](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/USER_PostgreSQL.Replication.ReadReplicas.Mechanisms-versions.html)), listed for Aurora ([doc](https://docs.aws.amazon.com/AmazonRDS/latest/AuroraUserGuide/AuroraPostgreSQL.Reference.ParameterGroups.html)). RDS 18 has `idle_replication_slot_timeout` ([blog](https://aws.amazon.com/blogs/database/logical-replication-improvements-in-amazon-rds-for-postgresql-18/)).
- **FDWs:**
  - postgres_fdw and dblink on RDS and Aurora 17/18 (RDS 18.6 ships postgres_fdw 1.2, Aurora 18 lists 1.1) ([RDS](https://docs.aws.amazon.com/AmazonRDS/latest/PostgreSQLReleaseNotes/postgresql-extensions.html), [Aurora](https://docs.aws.amazon.com/AmazonRDS/latest/AuroraPostgreSQLReleaseNotes/AuroraPostgreSQL.Extensions.html)). Creating needs `rds_superuser` or delegated extension support ([doc](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/RDS_delegated_ext.html)). Reach = plain VPC routing, security groups, NACLs; no host allowlist ([guidance](https://docs.aws.amazon.com/pdfs/prescriptive-guidance/latest/aurora-postgresql-integration/aurora-postgresql-integration.pdf)). Set `log_statement=none` in session or mapping password gets logged ([doc](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/postgresql-commondbatasks-fdw.html)).
  - mysql_fdw 2.9.3 pushes down aggregates ([repo](https://github.com/EnterpriseDB/mysql_fdw)); tds_fdw, oracle_fdw document no aggregate pushdown. No ClickHouse FDW, no pg_duckdb.
- **Custom code:** pg_tle 1.5.2 (hooks: passcheck, clientauth only) ([doc](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/PostgreSQL_trusted_language_extension.html)). PL/Rust RDS 17 only, removed from RDS 18. Fixed `shared_preload_libraries` list ([doc](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/Appendix.PostgreSQL.CommonDBATasks.Parameters.html)). No custom planner hook.
- **Barrier / sync standby:** both U. Test `SELECT has_function_privilege('pg_logical_emit_message(boolean,text,text)','EXECUTE')`; check `synchronous_standby_names` IsModifiable via `describe-db-parameters`.
- **AWS's own offload:** zero-ETL to Redshift (Aurora since 2024-10 ([news](https://aws.amazon.com/about-aws/whats-new/2024/10/amazon-aurora-postgresql-zero-etl-integration-redshift-generally-available/)), RDS since 2025-07 ([news](https://aws.amazon.com/about-aws/whats-new/2025/07/amazon-rds-zero-etl-redshift-generally-available/))); zero-ETL to SageMaker lakehouse ([news](https://aws.amazon.com/about-aws/whats-new/2025/10/amazon-aurora-postgresql-zero-etl-integration-sagemaker/)). **aurora_analytics**, GA 2026-09-30 on Aurora 17.11+/18.6+: embedded DuckDB over read-only Parquet/Iceberg foreign tables, pushes whole plans incl. GROUP BY and joins w/ local tables; self-serve (IAM role, dynamic parameter, CREATE EXTENSION) ([how it works](https://docs.aws.amazon.com/AmazonRDS/latest/AuroraUserGuide/aurora-analytics-how-it-works.html), [prerequisites](https://docs.aws.amazon.com/AmazonRDS/latest/AuroraUserGuide/aurora-analytics-prerequisites.html), [news](https://aws.amazon.com/about-aws/whats-new/2026/09/aurora-postgresql-query-apache-iceberg-and-parquet/)). Covers lake files, not live tables.

### Google Cloud: Cloud SQL and AlloyDB

- **Versions:** Cloud SQL PG18 GA 2025-11-20, now 18.6 ([release notes](https://docs.cloud.google.com/sql/docs/postgres/release-notes)). AlloyDB PG18 GA 2026-03-25, default for new clusters since 2026-11-03 ([release notes](https://docs.cloud.google.com/alloydb/docs/release-notes)). No PG19 on either; AlloyDB aims "within five months" of community GA ([policy](https://docs.cloud.google.com/alloydb/docs/db-version-policies)).
- **Ingest:**
  - Cloud SQL: `cloudsql.logical_decoding=on` (restart), pgoutput supported ([doc](https://docs.cloud.google.com/sql/docs/postgres/replication/configure-logical-replication)); user `WITH REPLICATION IN ROLE cloudsqlsuperuser` ([flags](https://docs.cloud.google.com/sql/docs/postgres/flags)). Plain table owner creating `FOR TABLE` publication: U. Read replicas publish from PG16, but primary vacuum can invalidate their slots and unhealthy replicas get recreated ([doc](https://docs.cloud.google.com/sql/docs/postgres/replication)).
  - AlloyDB: `alloydb.logical_decoding` (restart) ([flags](https://docs.cloud.google.com/alloydb/docs/reference/database-flags)), `ALTER USER ... WITH REPLICATION` ([doc](https://docs.cloud.google.com/datastream/docs/configure-alloydb-psql)). No logical replication from standby ([policy](https://docs.cloud.google.com/alloydb/docs/db-version-policies)).
- **Slot survival:**
  - Cloud SQL regional HA failover: undocumented; shared regional disk suggests slots survive (I). Switchover and DR keep slots only w/ logical failover slots: PG17+, Enterprise Plus, private services access, `sync_replication_slots=on`, `cloudsql.synchronized_standby_replicas`, `failover=true` ([doc](https://docs.cloud.google.com/sql/docs/postgres/advanced-dr-logical-failover-slot)). Major upgrade "deletes all existing logical replication slots" ([doc](https://docs.cloud.google.com/sql/docs/postgres/upgrade-major-db-version-inplace)). Backup restore doesn't restore replication state.
  - AlloyDB HA failover: U; AlloyDB Omni docs say slot recreated, stream position lost ([Omni](https://docs.cloud.google.com/alloydb/omni/containers/15.7.0/docs/create-replication-slots)). Major upgrade requires dropping all slots ([doc](https://docs.cloud.google.com/alloydb/docs/upgrade-db-inplace-major-version)). `idle_replication_slot_timeout` settable.
  - `max_slot_wal_keep_size`: Cloud SQL flags list it w/ min 102400 MB, but Datastream docs say Cloud SQL and AlloyDB don't support it ([Datastream](https://docs.cloud.google.com/datastream/docs/work-with-postgresql-database-wal-log-files)). Not in AlloyDB flag list.
- **FDWs:**
  - Cloud SQL postgres_fdw 1.1 (PG14+): "works for two Cloud SQL private IP instances within the same VPC network", or across databases in one instance; only `cloudsqlsuperuser` can own FDW; PSC instances need PSC outbound connectivity ([extensions](https://docs.cloud.google.com/sql/docs/postgres/extensions)). PSC outbound reaches only RFC 1918 addresses, not public IPs, `100.64.0.0/10` or IPv6; enabling costs ~3 min downtime ([PSC](https://docs.cloud.google.com/sql/docs/postgres/about-private-service-connect)). **Reaching non-Cloud SQL host like weir: U. Test before relying.**
  - AlloyDB postgres_fdw 1.1: documented outbound public IP (pair of static IPs) for postgres_fdw and oracle_fdw ([connections](https://docs.cloud.google.com/alloydb/docs/connection-overview)); PSC outbound exists, documented for migrations ([PSC](https://docs.cloud.google.com/alloydb/docs/configure-private-service-connect)).
  - Others: oracle_fdw, tds_fdw on both; AlloyDB lakehouse federation (`bigquery_fdw`) pushes SUM/COUNT/AVG/MIN/MAX ([doc](https://docs.cloud.google.com/alloydb/docs/access-real-time-data-overview)). No mysql_fdw, ClickHouse FDW, pg_duckdb.
- **Custom code:** "You cannot create your own extensions in Cloud SQL"; anything needing SUPERUSER and custom background workers unsupported ([features](https://docs.cloud.google.com/sql/docs/postgres/features)). `shared_preload_libraries` system-managed on both. plv8 only. No pg_tle, no PL/Rust, no planner hook.
- **Barrier / sync standby:** barrier U (no revocation documented). `synchronous_standby_names` not in either flag list, so not settable (I).
- **Google's own offload:** AlloyDB columnar engine (in-memory, planner uses it when all columns of fragment loaded, DML invalidates blocks, results patched from row store) ([about](https://docs.cloud.google.com/alloydb/docs/columnar-engine/about), [freshness](https://docs.cloud.google.com/alloydb/docs/columnar-engine/maintain-content-freshness)); Datastream CDC into BigQuery; BigQuery `EXTERNAL_QUERY` pushes filters, not aggregates ([doc](https://docs.cloud.google.com/bigquery/docs/federated-queries-intro)).

### Azure Database for PostgreSQL Flexible Server

- **Versions:** PG18 GA, 18.6 ([versions](https://learn.microsoft.com/en-us/azure/postgresql/configure-maintain/concepts-supported-versions)). PG19 not offered; only preview REST API lists it ([API notes](https://learn.microsoft.com/en-us/azure/postgresql/release-notes/release-notes-api)).
- **Ingest:** `wal_level=logical` (static, restart), then `ALTER ROLE <admin> WITH REPLICATION` ([logical](https://learn.microsoft.com/en-us/azure/postgresql/configure-maintain/concepts-logical), [WAL params](https://learn.microsoft.com/en-us/azure/postgresql/parameters/parameters-write-ahead-log-settings)). Admin NOSUPERUSER; `azure_pg_admin` pseudo-superuser ([access](https://learn.microsoft.com/en-us/azure/postgresql/security/security-access-control)). `FOR TABLE` publications shown in docs; `FOR ALL TABLES` blocked (superuser).
- **Slot survival:**
  - PG17+: native slot sync w/ `sync_replication_slots=on` and `hot_standby_feedback=on`, only for slots created with `failover`; PG≤16 uses `pg_failover_slots` ([HA](https://learn.microsoft.com/en-us/azure/postgresql/high-availability/concepts-high-availability), [standby params](https://learn.microsoft.com/en-us/azure/postgresql/parameters/parameters-replication-standby-servers)).
  - Unused slots auto-dropped when storage passes unpublished threshold ([logical](https://learn.microsoft.com/en-us/azure/postgresql/configure-maintain/concepts-logical), [limits](https://learn.microsoft.com/en-us/azure/postgresql/configure-maintain/concepts-limits)). Major upgrades check for slots; survival not stated ([MVU](https://learn.microsoft.com/en-us/azure/postgresql/configure-maintain/concepts-major-version-upgrade)).
  - `max_slot_wal_keep_size` read-only `-1`; `max_replication_slots` and `max_wal_senders` static; HA uses 4 of each ([sending params](https://learn.microsoft.com/en-us/azure/postgresql/parameters/parameters-replication-sending-servers)). `logical_decoding_work_mem` dynamic.
- **FDWs:** postgres_fdw (1.2 on PG18) and dblink, self-serve via `azure.extensions` ([versions](https://learn.microsoft.com/en-us/azure/postgresql/extensions/concepts-extensions-versions), [allowlist](https://learn.microsoft.com/en-us/azure/postgresql/extensions/how-to-allow-extensions)). Public-access servers have unrestricted outbound; VNet servers need NSG rules or peering to reach weir ([considerations](https://learn.microsoft.com/en-us/azure/postgresql/extensions/concepts-extensions-considerations), [limits](https://learn.microsoft.com/en-us/azure/postgresql/configure-maintain/concepts-limits)). oracle_fdw, tds_fdw document no aggregate pushdown; no mysql_fdw or ClickHouse FDW.
- **Custom code:** no uploads; fixed `shared_preload_libraries` list (age, anon, auto_explain, azure_storage, credcheck, pg_cron, pg_hint_plan, pg_ivm, pg_partman_bgw, pg_prewarm, pg_squeeze, pg_stat_statements, pgaudit, pglogical, plpgsql_check, timescaledb, wal2json) ([preload](https://learn.microsoft.com/en-us/azure/postgresql/parameters/parameters-client-connection-defaults-shared-library-preloading)). plv8 only. No planner hook.
- **Barrier / sync standby:** barrier U. `synchronous_standby_names` read-only ([params](https://learn.microsoft.com/en-us/azure/postgresql/parameters/parameters-replication-master-server)).
- **Microsoft's own offload:** Fabric mirroring GA (proprietary `azure_cdc` over pgoutput into OneLake Delta) ([doc](https://learn.microsoft.com/en-us/azure/postgresql/integration/concepts-fabric-mirroring)); pg_duckdb preview ([blog](https://techcommunity.microsoft.com/blog/adforpostgresql/whats-new-with-postgres-at-microsoft-2026-edition/4526963)); HorizonDB public preview, PG17 only ([extensions](https://learn.microsoft.com/en-us/azure/horizondb/extensions/concepts-extensions-versions)).

## Other providers

| Provider | Logical replication self-serve | postgres_fdw to an external host | Custom planner hook | PG18 |
|---|---|---|---|---|
| Supabase | yes | yes | no | no (U) |
| Neon | yes (one-time restart, irreversible) | yes | no | yes |
| Databricks Lakebase | yes since 2026-10-15 (docs conflict) | U | no | yes |
| Crunchy Bridge | yes (real superuser) | yes | no | yes |
| Snowflake Postgres | yes | yes (egress rule) | no | yes |
| DigitalOcean Standard | yes (`FOR TABLE` only) | yes | no | yes |
| DigitalOcean Advanced | U | no | no | yes |
| Aiven | yes | yes | no | yes |
| Heroku Advanced | yes (CLI) | U | no | yes |
| Heroku Essential/Standard/Premium | no | yes (not Essential) | no | yes |
| PlanetScale Postgres | yes (`failover=true` required) | yes (egress U) | no | yes |
| Tiger Cloud | likely no | yes | no | yes |
| ClickHouse Managed Postgres | U | yes | no | yes (I) |

Notes:
- **Supabase:** `postgres` has REPLICATION; direct connection only, IPv6 or IPv4 add-on ([doc](https://supabase.com/docs/guides/database/postgres/setup-replication-external)). postgres_fdw to external hosts documented ([doc](https://supabase.com/docs/guides/database/extensions/postgres_fdw)). Wrappers' ClickHouse FDW pushes down aggregates ([doc](https://supabase.com/docs/guides/database/extensions/wrappers/clickhouse)). Wasm FDWs load from URL but HTTP-only, no aggregate hook ([WIT](https://github.com/supabase/wrappers/tree/main/wasm-wrappers/wit/v2)). Major upgrades require dropping slots ([doc](https://supabase.com/docs/guides/platform/upgrading)). PG18 not on hosted as of October 2026 ([discussion](https://github.com/orgs/supabase/discussions/42681)).
- **Neon:** enabling logical replication irreversible, restarts all computes; REPLICATION only for console-created roles; 10 slots, 10 senders; slots w/o flush progress ~40 hours dropped; connected subscriber prevents scale-to-zero ([doc](https://neon.com/docs/guides/logical-replication-neon)). postgres_fdw to external hosts documented ([doc](https://neon.com/docs/extensions/postgres_fdw)). PG18 default since 2026-05-01 ([changelog](https://neon.com/docs/changelog/2026-05-01)).
- **Databricks Lakebase:** logical replication added 2026-10-15 ([release notes](https://docs.databricks.com/aws/en/release-notes/lakebase/)) while compatibility page still says "not yet available" ([compat](https://docs.databricks.com/aws/en/oltp/projects/compatibility)); public networking only ([doc](https://docs.databricks.com/aws/en/oltp/projects/logical-replication)).
- **Crunchy Bridge:** `wal_level=logical` default, real superuser, failover slots PG17+ ([doc](https://docs.crunchybridge.com/how-to/logical-replication), [changelog](https://docs.crunchybridge.com/changelog)); postgres_fdw, mysql_fdw, clickhouse_fdw listed ([extensions](https://docs.crunchybridge.com/extensions-and-languages)).
- **Snowflake Postgres:** `snowflake_admin` creates publications and slots, failover slots PG17+ ([doc](https://docs.snowflake.com/en/user-guide/snowflake-postgres/postgres-logical-replication)); ingress/egress need network rules ([network](https://docs.snowflake.com/en/user-guide/snowflake-postgres/postgres-network), [extensions](https://docs.snowflake.com/en/user-guide/snowflake-postgres/postgres-extensions)).
- **DigitalOcean:** Standard (Aiven-based) has `doadmin` w/ REPLICATION and postgres_fdw; `max_replication_slots` 8–64 via API ([API](https://docs.digitalocean.com/reference/pydo/reference/databases/patch_config/)). From 2026-11-30 new Standard clusters capped at 4 GiB RAM ([limits](https://docs.digitalocean.com/products/databases/postgresql/details/limits/)). Advanced: 25 extensions, no postgres_fdw ([extensions](https://docs.digitalocean.com/products/databases/postgresql/details/supported-extensions/)).
- **Aiven:** `avnadmin` has REPLICATION; `aiven_extras` for all-tables publications ([doc](https://aiven.io/docs/products/kafka/kafka-connect/howto/debezium-source-connector-pg)); proprietary slot recreation on failover, slots created <30 s before failover can be lost, slots don't survive major upgrades ([doc](https://github.com/aiven/aiven-docs/blob/main/docs/products/postgresql/concepts/upgrade-failover.md)); postgres_fdw and dblink ([extensions](https://aiven.io/docs/products/postgresql/reference/list-of-extensions)).
- **Heroku:** classic tiers no logical replication ([help](https://help.heroku.com/TVS8OHTR/does-heroku-postgres-support-logical-replication)); Advanced (GA 2026-10-06) enables publishing via CLI ([doc](https://devcenter.heroku.com/articles/logical-replication-on-heroku-postgres-advanced)).
- **PlanetScale Postgres:** `wal_level=logical` and REPLICATION roles via dashboard; slots need `failover=true`, else lost at next planned failover after 3-day warning ([doc](https://planetscale.com/docs/postgres/integrations/logical-cdc)); postgres_fdw listed ([extensions](https://planetscale.com/docs/postgres/extensions)); pg_duckdb available.
- **Tiger Cloud:** "Timescale Cloud doesn't support enabling logical replication" per ClickHouse docs ([doc](https://clickhouse.com/docs/integrations/clickpipes/postgres/source/timescale)); postgres_fdw on by default ([extensions](https://www.tigerdata.com/docs/use-timescale/latest/extensions)).
- **ClickHouse Managed Postgres:** beta ([overview](https://clickhouse.com/docs/products/managed-postgres/overview)); pg_clickhouse preinstalled w/ aggregate pushdown, plus postgres_fdw, mysql_fdw ([extensions](https://clickhouse.com/docs/products/managed-postgres/extensions)). Outbound logical replication to external subscriber: U.

## Options without talking to a vendor

1. **weir as postgres_fdw remote.** Works wherever postgres_fdw reaches weir: AWS, AlloyDB, Azure, Supabase, Neon, Crunchy, Snowflake (egress rule), Aiven, DigitalOcean Standard, PlanetScale. Queries name foreign tables (e.g. `weir.orders`), so not transparent. Cloud SQL needs live test (PSC outbound to RFC 1918 address).
2. **Direct endpoint.** BI tools, dashboards, services connect to weir over wire protocol. Needs nothing from DB beyond ingest, avoids DB-to-weir network path. Same pgwire server as option 1.
3. **Wire proxy.** Transparent to apps; DB untouched. Apps change connection string. All traffic flows through; weir must parse and route every statement.
4. **Client-side routing (I).** Small library or ORM hook in app sends eligible aggregate queries to weir, rest to PostgreSQL. App change, no proxy in hot path.
5. **Second FDW protocols (I).** Emulating MySQL protocol lets RDS/Aurora `mysql_fdw` push aggregates to weir; emulating ClickHouse reaches Supabase Wrappers, Crunchy `clickhouse_fdw`, ClickHouse Managed Postgres `pg_clickhouse`. Wider reach, not needed first.
6. **Write Iceberg/Parquet for vendor's engine (I).** Aurora `aurora_analytics`, AlloyDB lakehouse federation, pg_lake read lake files. weir feeds them; freshness drops to lake table commit interval. Not weir core.

## Design implications (I)

- Create slots with `failover => true` on PG17+ wherever provider syncs failover slots; confirm flush progress often: Neon and Lakebase drop slots stalled ~40 hours, PlanetScale drops slots w/o failover.
- Use direct connections for replication, never provider's pooler.
- Expect default cap ~10 replication slots, 10 WAL senders, shared with provider's own HA and CDC.
- Make "slot gone, resync" fast, routine path: upgrades, blue/green switchovers, storage-pressure drops all end there.

## Tests to run on live instances

Need account on each service, not vendor:
1. `has_function_privilege('pg_logical_emit_message(boolean,text,text)', 'EXECUTE')` for ordinary role on RDS, Aurora, Cloud SQL, AlloyDB, Azure.
2. Cloud SQL postgres_fdw to GCE VM, over PSA and PSC outbound.
3. `CREATE PUBLICATION ... FOR TABLE` as plain table owner on Cloud SQL and RDS.
4. Logical slot survival across HA failover on Aurora, AlloyDB, Cloud SQL (regional HA), RDS Multi-AZ instance.
5. Whether `synchronous_standby_names` modifiable on RDS (`describe-db-parameters`).
