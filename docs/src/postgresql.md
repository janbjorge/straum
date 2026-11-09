# Setting up PostgreSQL

Straum needs PostgreSQL 18, self-hosted or managed. It uses only what
stock PostgreSQL ships: logical replication with `pgoutput`, a temporary
slot and an exported snapshot. It needs no extension and no superuser.

Setting up takes three steps: turn on logical decoding, run the SQL that
`straum plan` prints, and give Straum a DSN.

## 1. Turn on logical decoding

PostgreSQL needs `wal_level = logical`, which takes a restart. On a server
you run yourself:

```sql
ALTER SYSTEM SET wal_level = logical;
```

Then restart PostgreSQL. On a managed service, set the provider's
parameter instead and restart there:

| Service | Setting |
| --- | --- |
| Amazon RDS and Aurora | [`rds.logical_replication = 1`](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/PostgreSQL.Concepts.General.FeatureSupport.LogicalReplication.html) |
| Google Cloud SQL | [`cloudsql.logical_decoding = on`](https://docs.cloud.google.com/sql/docs/postgres/replication/configure-logical-replication) |
| Google AlloyDB | [`alloydb.logical_decoding = on`](https://docs.cloud.google.com/alloydb/docs/reference/database-flags) |
| Azure Database for PostgreSQL | [`wal_level = logical`](https://learn.microsoft.com/en-us/azure/postgresql/configure-maintain/concepts-logical) |
| Neon | [Logical replication in the project settings](https://neon.com/docs/guides/logical-replication-neon); it can't be turned off again |
| Crunchy Bridge | [On by default](https://docs.crunchybridge.com/how-to/logical-replication) |

These are the providers' documented settings as of November 2026. Their
pages have the details.

## 2. Run the setup SQL

`straum plan` reads the spec and prints the SQL that lets Straum read its
tables. For the [quick start](quick-start.md) spec:

```sql
-- straum setup: run once, as an owner of the tables.
-- wal_level takes a restart. On Azure, set the server parameter instead.
ALTER SYSTEM SET wal_level = logical;
CREATE ROLE "straum" LOGIN REPLICATION;
-- Then give "straum" a password: \password "straum"
GRANT USAGE ON SCHEMA "public" TO "straum";
GRANT SELECT ON "public"."customers", "public"."orders" TO "straum";
CREATE PUBLICATION "straum" FOR TABLE "public"."customers", "public"."orders";
-- fact orders: straum needs the old row of every update and delete; inserts cost no more.
ALTER TABLE "public"."orders" REPLICA IDENTITY FULL;
-- dim customers: "public"."customers" needs a primary key on id alone, or REPLICA IDENTITY FULL and a unique index on it.
```

Run it as a role that owns the tables, since only an owner can add a table
to a publication. Straum never asks for `FOR ALL TABLES`, which needs a
superuser.

- The role logs in and has `REPLICATION`, which replication connections
  need. Some managed services don't let you create a role with
  `REPLICATION` and grant their own role for it instead, such as
  [`rds_replication`](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/PostgreSQL.Concepts.General.FeatureSupport.LogicalReplication.html)
  on Amazon RDS. `--role NAME` changes the role's name from `straum`.
- The grants let it read the tables, for the load.
- The publication names the tables whose changes Straum receives.
  `--publication NAME` changes its name from `straum`; give `straum run`
  the same name.
- Facts get `REPLICA IDENTITY FULL`. PostgreSQL then writes the old row of
  every update and delete to the WAL, which Straum needs to take the row
  out of its cell. Inserts cost no more.
- Dims need their key as replica identity: a primary key on the key column
  alone, `REPLICA IDENTITY USING INDEX` on a unique index of it, or
  `REPLICA IDENTITY FULL` with a unique index on it. Only the schema can
  give that, so the SQL says it in a comment.

The setup lives in the catalog. It survives restarts of Straum and
PostgreSQL, and Straum checks it again on every start.

## 3. Connect

Straum reads its DSN from the `STRAUM_DSN` environment variable or from a
file named by `--dsn-file`, never from the command line, where other users
could see it:

```text
postgres://straum:PASSWORD@db.example.com:5432/app
```

The port defaults to 5432 and the database to the user's name. Percent-encode
characters in the password that a URL reserves, such as `@` or `/`.

Use the server's direct endpoint, not a pooled one: the replication
connection needs a session of its own on the server.

### TLS

Straum connects with TLS and checks that the server's certificate chains
to a root it trusts and names the host. Unlike `libpq`, it does this for
every `sslmode` but one:

| `sslmode` | What Straum does |
| --- | --- |
| none, `require`, `verify-ca`, `verify-full` | TLS, checking the certificate and the host name |
| `disable` | Plain TCP. Use it only on a network you trust. |
| `allow`, `prefer` | Refused, since they fall back to plain TCP |

The roots built in are Mozilla's. If your provider signs its servers'
certificates with a CA of its own, download its bundle and name it with
`sslrootcert`:

```text
postgres://straum:PASSWORD@db.example.com/app?sslrootcert=/etc/straum/ca.pem
```

## What Straum checks at start

Before it loads, Straum compares the server with the spec and reports
every problem at once, each with its fix. It stays not ready until they
are fixed, and tries again after a wait. The checks:

- `wal_level is replica; straum needs logical, which takes a restart`
- `publication straum does not exist; run the SQL straum plan prints`
- `publication straum must publish insert, update, delete and truncate`
- `orders: publication straum leaves out public.orders: ALTER PUBLICATION straum ADD TABLE public.orders`
- `orders: straum's role can't read public.orders: GRANT SELECT ON public.orders`
- `orders: publication straum filters the rows of public.orders; straum needs every row`
- `orders: public.orders has no column placed_at`
- `orders: column amount of public.orders is double precision; straum needs an integer type or numeric(p, s) with p at most 18 there`
- `fact orders: straum needs the whole old row of every update and delete: ALTER TABLE public.orders REPLICA IDENTITY FULL`
- `dim customers: public.customers needs a primary key or unique index on id alone`

`straum status` and `GET /readyz` show them, and so does Straum's log.

## What Straum costs the server

- Each running Straum takes one replication connection and one slot, out
  of `max_wal_senders` and `max_replication_slots`. Managed services often
  keep some of each for themselves.
- PostgreSQL keeps WAL for the slot until Straum confirms it. Straum
  confirms every second, and starts over if the server holds more than
  `--max-retained-wal` for it (4 GiB by default), so a Straum that can't
  keep up can't fill the server's disk.
- The slot is temporary, so PostgreSQL drops it when the connection
  closes, including when Straum crashes or its pod is deleted.
- While Straum loads, its snapshot holds back vacuum for the whole
  database, and `ALTER TABLE` on its tables waits. Straum ends the
  snapshot right after the last table loads. A load runs one `GROUP BY`
  per fact, so PostgreSQL sends one row per group.
- Each replication connection decodes the server's WAL, so run one Straum
  per database unless you need more.
- Temporary slots don't move to a standby. After a failover Straum
  connects to the new primary and loads again.
