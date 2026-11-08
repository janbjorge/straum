# weir on AKS next to Azure Database for PostgreSQL Flexible Server

jb, 2026-11-08: ship weir as Helm chart, deploy into same Kubernetes cluster as apps, answer heavy count/sum/GROUP BY queries. First target: AKS with PostgreSQL 18 on Azure Flexible Server, outside cluster. Tags as in README.md (R, U, I).

## Assumed setup

- PostgreSQL on Flexible Server, not in cluster. AKS runs apps only.
- AKS pods and server in same VNet (Azure CNI, delegated subnet, private DNS zone). Apps reach PostgreSQL today.
- `wal_level` = Azure default `replica` (U). PostgreSQL 18 needs `wal_level = logical` + restart.
- `pg_stat_statements` or Query Store on → heavy aggregate queries findable from data, not guessed.
- Apps may ship as plain manifests without Helm. Secrets from Key Vault via Secrets Store CSI driver.

## Consequences for weir (I)

- Custom-extension path closed: managed case from MANAGED-POSTGRES.md, on Azure. Ingest by logical replication works: `wal_level = logical` = one server parameter + restart.
- weir → PostgreSQL (replication + COPY) uses same network path apps use. Nothing new except parameter.
- PostgreSQL → weir (postgres_fdw) needs weir behind internal LoadBalancer IP, `postgres_fdw` in `azure.extensions`, VNet routing that allows it (U). Clients connecting to weir directly avoid all that.
- Helm chart fits weir as product. Teams without Helm for apps need chart to render cleanly with `helm template`, output committable, applied by existing pipeline. Azure Pipelines also has `HelmDeploy` task (U).

## Chart shape (I)

Partly superseded by ARCHITECTURE.md (2026-11-08): weir keeps no durable state, temporary slot per pod → chart has Deployment, no PVC. Each replica = own slot + walsender; Azure HA standby uses slots and walsenders too; default 1 replica.

| Object | Purpose |
|---|---|
| Deployment, 1 replica, `maxSurge: 1`, `maxUnavailable: 0` | weir process; pgwire on 5432, metrics, health. New pod syncs before old stops |
| ConfigMap | overrides: deny list, `append_only` tables. No spec: weir learns queries from app SQL (ARCHITECTURE.md §2c) |
| Service, ClusterIP | in-cluster clients (`weir.weir.svc:5432`) |
| Service, internal LoadBalancer (optional) | postgres_fdw from Flexible Server: `service.beta.kubernetes.io/azure-load-balancer-internal: "true"` |
| Secret or SecretProviderClass (`existingSecret`) | source DSN for replication role; on Azure, from Key Vault via Secrets Store CSI |

Probes:
- Startup: short. Tables load per query shape after start (ARCHITECTURE.md §2c).
- Readiness: setup check passed, stream lag under bound. Shapes still loading answer 57P03. Clients must fall back to PostgreSQL when weir not ready.
- Liveness: process only. Never fail liveness on lag, else slow catch-up → restart loop.

Defaults: `runAsNonRoot`, uid 1001, drop ALL, no `imagePullSecrets` (ACR pulls by managed identity). Metrics out over OTLP to cluster's collector.

Values sketch:

```yaml
image:
  repository: <acr>.azurecr.io/weir
  tag: ""
source:
  existingSecret: weir-source      # key: dsn (role with REPLICATION)
  publication: weir                # weir creates a TEMPORARY slot per pod
replicas: 1
overrides:                         # optional; rollups come from app SQL
  denyTables: []                   # never published, never loaded
  appendOnlyTables: []             # skip the row store
freshness:
  mode: barrier                    # or max_lag
  maxLagMs: 1000
  maxRetainedWal: 4Gi              # past this, drop the connection and resync
service:
  port: 5432
  internalLoadBalancer: false
resources:
  requests: {cpu: 500m, memory: 1Gi}
  limits: {memory: 2Gi}
```

## One-time database setup on Flexible Server

1. Set server parameter `wal_level = logical` (Terraform or portal), restart. Slot sync settings not needed: temporary slots can't fail over, weir resyncs after HA failover (ARCHITECTURE.md §5).
2. Run `weir plan <tables>` (ARCHITECTURE.md §2c). Checks PK and replica identity, prints `CREATE ROLE weir LOGIN REPLICATION`, `GRANT SELECT` (COPY reads tables), `CREATE PUBLICATION weir FOR TABLE ...`: whole tables, no column lists.
3. Table owner runs printed SQL. Often app's own role owns tables (I). `FOR ALL TABLES` needs superuser, blocked on Azure.
4. weir creates own `TEMPORARY` slot each start; PostgreSQL drops it when session ends.

**Production risk (R).** Once table in publication publishing updates + deletes, PostgreSQL checks replica identity on every UPDATE and DELETE on primary:
- Table without primary key (default replica identity) fails with `cannot update table "%s" because it does not have a replica identity and publishes updates` (`executor/execReplication.c:1149`).
- Column list omitting replica identity column fails same writes with "Column list used by the publication does not cover the replica identity." (`execReplication.c:1107`, `:1125`).

Wrong publication → breaks app writes, not just weir. `weir plan` checks `pg_index` / `relreplident` before printing `CREATE PUBLICATION`; weir rechecks every start.

**WAL growth (R, Azure docs via MANAGED-POSTGRES.md).** `max_slot_wal_keep_size` read-only `-1` on Azure → stalled weir holds WAL until storage fills; Azure drops unused slots past unpublished storage threshold. Temporary slot limits this to time weir connected but behind: weir checks own retained WAL, resyncs past `maxRetainedWal`. Also alert on slot lag (`pg_replication_slots`).

## Baseline to beat

Azure allows `pg_ivm` in `shared_preload_libraries` (MANAGED-POSTGRES.md, Azure section, R). Maintains GROUP BY views with count, sum, avg, min, max inside PostgreSQL, synchronously per write, no new service. For handful of fixed queries may suffice. Cost lands on write path; how bad for target write rate = U. Measure on test system against same queries before committing to weir.

## Open points

1. Which queries: top aggregates from `pg_stat_statements`, share of execution time weir's class covers.
2. HA: with temporary slots, failover = weir resync.
3. VNet routing Flexible Server → internal LB? Only matters for postgres_fdw.
4. `helm template` output committed to deploy repo, or `HelmDeploy` step?
5. `pg_logical_emit_message` EXECUTE for ordinary role on Azure (MANAGED-POSTGRES.md test 1).
