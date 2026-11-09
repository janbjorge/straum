# Setting up PostgreSQL

Straum needs PostgreSQL 17 or newer, self-hosted or managed; its tests
run on 17, 18 and the 19 release candidate. It uses logical replication
with `pgoutput`, a temporary slot and an exported snapshot, with no
extension and no superuser. Setup is three steps, done once.

## 1. Turn on logical decoding

PostgreSQL 17 and 18 need `wal_level = logical`, which takes a restart.
On your own server, run `ALTER SYSTEM SET wal_level = logical;` and
restart. From PostgreSQL 19, `replica`, the default, is enough: a logical
slot turns logical decoding on while it exists. On a managed service, set
the provider's parameter and restart there:

| Service | Setting |
| --- | --- |
| Amazon RDS and Aurora | [`rds.logical_replication = 1`](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/PostgreSQL.Concepts.General.FeatureSupport.LogicalReplication.html) |
| Google Cloud SQL | [`cloudsql.logical_decoding = on`](https://docs.cloud.google.com/sql/docs/postgres/replication/configure-logical-replication) |
| Google AlloyDB | [`alloydb.logical_decoding = on`](https://docs.cloud.google.com/alloydb/docs/reference/database-flags) |
| Azure Database for PostgreSQL | [`wal_level = logical`](https://learn.microsoft.com/en-us/azure/postgresql/configure-maintain/concepts-logical) |
| Neon | [Logical replication in the project settings](https://neon.com/docs/guides/logical-replication-neon); it can't be turned off again |
| Crunchy Bridge | [On by default](https://docs.crunchybridge.com/how-to/logical-replication) |

These are the providers' documented settings as of November 2026.

## 2. Run the setup SQL

`straum plan --spec FILE` prints the SQL that lets Straum read the spec's
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

Run it as an owner of the tables, since only an owner can add a table to a
publication. Straum never asks for `FOR ALL TABLES`, which needs a
superuser.

- `REPLICATION` lets the role open a replication connection. Some managed
  services grant their own role for it instead, such as
  [`rds_replication`](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/PostgreSQL.Concepts.General.FeatureSupport.LogicalReplication.html)
  on Amazon RDS. `--role NAME` renames the role.
- The grants let it read the tables, for the load.
- The publication names the tables whose changes Straum receives.
  `--publication NAME` renames it; give `straum run` the same name.
- `REPLICA IDENTITY FULL` makes PostgreSQL write the old row of every
  update and delete to the WAL, so Straum can take the row out of its
  cell. Inserts cost no more.
- Dims need their key as replica identity: a primary key on the key
  alone, `REPLICA IDENTITY USING INDEX` on a unique index of it, or
  `REPLICA IDENTITY FULL` with a unique index on it. Only the schema can
  give that, so the SQL says it in a comment.

The setup lives in the catalog and survives restarts. Straum checks it on
every start.

## 3. Connect

Straum reads its DSN from `STRAUM_DSN` or from a file named by
`--dsn-file`, never from the command line, where other users could see
it:

```text
postgres://straum:PASSWORD@db.example.com:5432/app
```

The port defaults to 5432 and the database to the user's name.
Percent-encode reserved characters in the password, such as `@` or `/`.
Use the server's direct endpoint, not a pooler: the replication
connection needs a session of its own.

Straum connects with TLS and checks that the server's certificate chains
to a trusted root and names the host. Unlike `libpq`, it does this for
every `sslmode` but `disable`:

| `sslmode` | What Straum does |
| --- | --- |
| none, `require`, `verify-ca`, `verify-full` | TLS, checking the certificate and the host name |
| `disable` | Plain TCP. Use it only on a network you trust. |
| `allow`, `prefer` | Refused, since they fall back to plain TCP |

The built-in roots are Mozilla's. If your provider signs with a CA of its
own, download its bundle and name it with `sslrootcert`:

```text
postgres://straum:PASSWORD@db.example.com/app?sslrootcert=/etc/straum/ca.pem
```

## What Straum checks at start

Before it loads, Straum compares the server with the spec and reports
every problem at once, each with its fix, in its log, `/readyz` and
`straum status`. It stays not ready and [tries again](running.md#states)
after a wait. The checks:

```text
wal_level is replica; straum needs logical, which takes a restart
wal_level is minimal; straum needs replica or logical, which takes a restart
publication straum does not exist; run the SQL straum plan prints
publication straum must publish insert, update, delete and truncate
orders: table public.orders does not exist
orders: public.orders is not a plain table
orders: publication straum leaves out public.orders: ALTER PUBLICATION straum ADD TABLE public.orders
orders: straum's role can't read public.orders: GRANT SELECT ON public.orders
orders: publication straum filters the rows of public.orders; straum needs every row
orders: public.orders has no column placed_at
orders: publication straum leaves out column amount of public.orders
orders: column amount of public.orders is double precision; straum needs an integer type or numeric(p, s) with p at most 18 there
fact orders: straum needs the whole old row of every update and delete: ALTER TABLE public.orders REPLICA IDENTITY FULL
dim customers: public.customers needs a primary key or unique index on id alone
dim customers: the replica identity of public.customers must be id alone, so a delete names the row straum holds: a primary key on id, REPLICA IDENTITY USING INDEX on a unique index of id, or REPLICA IDENTITY FULL
fact orders: ref customer joins customer_id (an integer) to id (text); they must read alike
fact orders: measure paid compares status (text) with 1
```

## What Straum costs the server

- One replication connection and one slot per running Straum, out of
  `max_wal_senders` and `max_replication_slots`. Managed services often
  keep some of each for themselves.
- WAL, until Straum confirms it, which it does every second. If the server
  holds more than `--max-retained-wal` for the slot (4 GiB by default),
  Straum starts over, so a Straum that can't keep up can't fill the disk.
- Nothing after Straum is gone: the slot is temporary, so PostgreSQL drops
  it when the connection closes, crashes and deleted pods included.
  Temporary slots don't move to a standby either, so after a failover
  Straum loads again from the new primary.
- A snapshot during each load, which holds back vacuum for the whole
  database and makes `ALTER TABLE` on its tables wait. Each table loads
  with one binary `COPY`, a plain scan with no sort or hash to spill to
  disk, and Straum ends the snapshot right after the last table.
- WAL decoding per replication connection, so run one Straum per database
  unless you need more.
