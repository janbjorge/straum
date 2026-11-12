# Straum on AKS next to Azure Database for PostgreSQL Flexible Server

jb, 2026-11-08: ship Straum as a Helm chart into the same Kubernetes cluster as
the apps. First target: AKS, with PostgreSQL 18 on Azure Flexible Server
outside the cluster. Design in `docs/src/how-it-works.md`; PostgreSQL
mechanics in ARCHITECTURE.md. Tags as in README.md (R, U, I).

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

## Chart shape

Built in M7 as `deploy/helm/straum`; `docs/src/kubernetes.md` describes
what it renders and its values. Two choices made here: a Deployment
with no PVC, since Straum keeps no durable state and uses a temporary
slot per pod, and one replica by default, since each replica is its own
slot and walsender and the Azure HA standby uses slots and walsenders
too. Liveness is the process only: failing it on lag would turn a slow
catch-up into a restart loop.

## One-time database setup on Flexible Server

Set the server parameter `wal_level = logical` (Terraform or portal)
and restart; slot sync settings aren't needed, since temporary slots
can't fail over (ARCHITECTURE.md §6). Then the steps of
`docs/src/postgresql.md`. Often the app's own role owns its tables (I).

**Production risk (R).** Once a table is in a publication that publishes
updates and deletes, PostgreSQL checks its replica identity on every UPDATE and
DELETE on the primary:
- A table without a primary key (default replica identity) fails those writes
  with `cannot update table "%s" because it does not have a replica identity
  and publishes updates` (`executor/execReplication.c:1149`).
- A column list that leaves out a replica identity column fails the same
  writes with "Column list used by the publication does not cover the replica
  identity." (`execReplication.c:1107`, `:1125`).

A wrong publication breaks the app's writes, not only Straum. `straum plan`
can't see the schema, so check each dim's key before running its SQL.
`straum run` checks `pg_index` and `relreplident` on every start and names
what is missing.

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
2. Every fact has REPLICA IDENTITY FULL (PLAN.md, 2026-11-09), which writes
   the old row to WAL on every UPDATE and DELETE. Measure that on the
   primary at the real write rate (ARCHITECTURE.md §2).
3. `helm template` output committed to the deploy repo, or a `HelmDeploy`
   step?
4. HA: with temporary slots, a failover means a full resync. How long does the
   load take on the real tables?
