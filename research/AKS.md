# Straum on AKS next to Azure Database for PostgreSQL Flexible Server

jb, 2026-11-08: ship Straum as a Helm chart into the same Kubernetes cluster as
the apps. First target: AKS, with PostgreSQL 18 on Azure Flexible Server
outside the cluster. Design in ARCHITECTURE.md. Tags as in README.md (R, U,
I).

## Assumed setup

- PostgreSQL runs on Flexible Server, not in the cluster. AKS runs the apps.
- AKS pods and the server share a VNet (Azure CNI, delegated subnet, private
  DNS zone). Apps already reach PostgreSQL.
- `wal_level` is Azure's default, `replica` (U). PostgreSQL 18 needs
  `wal_level = logical` and a restart.
- `pg_stat_statements` or Query Store is on, so the heavy aggregate queries can
  be found from data instead of guessed.
- Apps may ship as plain manifests without Helm. Secrets come from Key Vault
  through the Secrets Store CSI driver.

## Consequences for Straum (I)

- No custom extension is needed, which Azure wouldn't allow anyway
  (MANAGED-POSTGRES.md). Logical replication needs one server parameter and a
  restart.
- Straum reaches PostgreSQL over the network path the apps already use. Apps
  reach Straum over HTTP inside the cluster. No new network path in either
  direction.
- The chart must render cleanly with `helm template`, so teams without Helm can
  commit the output and apply it with their existing pipeline. Azure Pipelines
  also has a `HelmDeploy` task (U).

## Chart shape (I)

Straum keeps no durable state and uses a temporary slot per pod, so the chart
has a Deployment and no PVC. Each replica is its own slot and walsender, and
the Azure HA standby uses slots and walsenders too, so the default is one
replica.

- **Deployment**, 1 replica, `maxSurge: 1`, `maxUnavailable: 0`: the new pod
  loads before the old one stops.
- **ConfigMap**: the spec. A checksum annotation rolls the Deployment when it
  changes.
- **Service**, ClusterIP: asks over HTTP/JSON, plus `/livez` and `/readyz`.
- **Secret or SecretProviderClass** (`existingSecret`): the DSN of the
  replication role; on Azure from Key Vault through Secrets Store CSI.

Probes:
- Startup and readiness: Straum loads every fact before it turns ready, so
  readiness stays off through the load and catch-up. Clients must handle a
  Straum that isn't ready.
- Liveness: process only. Failing liveness on lag would turn a slow catch-up
  into a restart loop.

Defaults: `runAsNonRoot`, uid 1001, drop ALL capabilities, no
`imagePullSecrets` (ACR pulls by managed identity). Metrics go out over OTLP to
the cluster's collector.

Values sketch:

```yaml
image:
  repository: <acr>.azurecr.io/straum
  tag: ""
source:
  existingSecret: straum-source      # key: dsn (role with REPLICATION)
  publication: straum                # Straum creates a TEMPORARY slot per pod
spec: |                              # Straum SQL (Straum-only subset), read at start
  CREATE VIEW trips WITH (key = id, append = true) AS
  SELECT date_bin('15 minutes', f.pickup_at, timestamptz 'epoch') AS bucket,
         f.pu_location_id, pickup.borough, pickup.zone,
         count(*) AS trips
  FROM public.yellow_trips f
  LEFT JOIN public.taxi_zones pickup ON pickup.location_id = f.pu_location_id
  GROUP BY 1, 2, 3, 4;
maxRetainedWal: 4Gi                # past this, drop the connection and resync
replicas: 1
service:
  port: 8080
resources:
  requests: {cpu: 500m, memory: 1Gi}
  limits: {memory: 2Gi}
```

## One-time database setup on Flexible Server

1. Set the server parameter `wal_level = logical` (Terraform or portal) and
   restart. Slot sync settings aren't needed: temporary slots can't fail over,
   and Straum resyncs after an HA failover (ARCHITECTURE.md §8).
2. Run `straum plan` on the spec (ARCHITECTURE.md §4d). It checks keys and
   replica identity and prints `CREATE ROLE straum LOGIN REPLICATION`, `GRANT
   SELECT` on the spec's tables (the load reads them), and `CREATE PUBLICATION
   Straum FOR TABLE ...` with the fact and dim tables.
3. The table owner runs the printed SQL. Often the app's own role owns its
   tables (I). `FOR ALL TABLES` needs superuser, which Azure doesn't give.
4. Straum creates its own `TEMPORARY` slot on every start; PostgreSQL drops it
   when the session ends.

**Production risk (R).** Once a table is in a publication that publishes
updates and deletes, PostgreSQL checks its replica identity on every UPDATE and
DELETE on the primary:
- A table without a primary key (default replica identity) fails those writes
  with `cannot update table "%s" because it does not have a replica identity
  and publishes updates` (`executor/execReplication.c:1149`).
- A column list that leaves out a replica identity column fails the same
  writes with "Column list used by the publication does not cover the replica
  identity." (`execReplication.c:1107`, `:1125`).

A wrong publication breaks the app's writes, not only Straum. `straum plan` checks
`pg_index` and `relreplident` before it prints `CREATE PUBLICATION`, and Straum
rechecks on every start.

**WAL growth (R, Azure docs via MANAGED-POSTGRES.md).** Azure fixes
`max_slot_wal_keep_size` at `-1`, so a stalled Straum holds WAL until storage
fills; Azure drops unused slots past an unpublished storage threshold. The
temporary slot limits this to the time Straum is connected but behind. Straum
watches its own retained WAL and resyncs past `maxRetainedWal`. Also alert on
slot lag in `pg_replication_slots`.

## Baseline to beat

Azure allows `pg_ivm` in `shared_preload_libraries` (MANAGED-POSTGRES.md,
Azure section, R). It maintains GROUP BY views with count, sum, avg, min and
max inside PostgreSQL, synchronously on every write, with no new service. For a
handful of fixed aggregates that may be enough. The cost lands on the write
path; how much at the target write rate is unknown (U). Measure it on a test
system against the same aggregates before committing to Straum.

## Open points

1. Which aggregates: the top ones from `pg_stat_statements`, and how much of
   their execution time a spec can cover.
2. Mutable facts with REPLICA IDENTITY FULL write the old row to WAL on every
   UPDATE and DELETE. Measure that on the primary if FULL wins
   (ARCHITECTURE.md §4b).
3. `helm template` output committed to the deploy repo, or a `HelmDeploy`
   step?
4. HA: with temporary slots, a failover means a full resync. How long does the
   load take on the real tables?
