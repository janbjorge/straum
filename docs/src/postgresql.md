# Setting up PostgreSQL

Straum supports PostgreSQL 17 and newer, self-hosted or managed; its
tests run on 17, 18 and the 19 release candidate. It uses logical
replication with `pgoutput`, a temporary slot and an exported snapshot,
with no extension and no superuser. Setup is three steps, done once.
Where `wal_level = logical` can't be had, Straum can read the physical
WAL instead: see [Without logical decoding](#without-logical-decoding).

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
| Neon | [Enable logical replication](https://neon.com/docs/guides/logical-replication-neon); it can't be turned off again |
| Crunchy Bridge | [Logical replication is enabled by default](https://docs.crunchybridge.com/how-to/logical-replication) |

## 2. Run the setup SQL

`straum plan FILE` prints the SQL that lets Straum read the spec's
tables. With `STRAUM_DSN` or `--dsn-file` set, the role is the DSN's
user, and its password goes in as a SCRAM-SHA-256 verifier, so the
password itself appears in neither the SQL nor the server's log. Without
a DSN the role is `straum` and the SQL says how to give it a password.
For the `usage` view of [Writing a spec](spec.md#synopsis):

```sql
-- Straum setup: run once, as an owner of the tables.
-- wal_level takes a restart. On Azure, set the server parameter instead.
ALTER SYSTEM SET wal_level = logical;
CREATE ROLE "straum" LOGIN REPLICATION PASSWORD 'SCRAM-SHA-256$4096:...';
GRANT USAGE ON SCHEMA "public" TO "straum";
GRANT SELECT ON "public"."accounts", "public"."events" TO "straum";
CREATE PUBLICATION "straum" FOR TABLE "public"."accounts", "public"."events";
-- fact usage: Straum needs the old row of every update and delete; inserts cost no more.
ALTER TABLE "public"."events" REPLICA IDENTITY FULL;
-- dim accounts: Straum holds its key and the columns the views read, and follows it.
ALTER TABLE "public"."accounts" REPLICA IDENTITY FULL;
```

Run it as an owner of the tables, since only an owner can add a table to a
publication. Straum never asks for `FOR ALL TABLES`, which needs a
superuser. The setup lives in the catalog and survives restarts.

- Some managed services grant replication through a role of their own,
  such as
  [`rds_replication`](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/PostgreSQL.Concepts.General.FeatureSupport.LogicalReplication.html)
  on Amazon RDS. `--role NAME` names the role.
- `--publication NAME` names the publication; give `straum run` the same
  name.
- Dims are published and get `REPLICA IDENTITY FULL` like facts, since
  Straum holds and follows them.

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
| omitted, `require`, `verify-ca`, `verify-full` | TLS, checking the certificate and the host name |
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
after a wait. Some of the checks:

```text
wal_level is replica; Straum needs logical, which takes a restart
publication straum does not exist; run the SQL straum plan prints
usage: publication straum leaves out public.events: ALTER PUBLICATION straum ADD TABLE public.events
usage: Straum's role can't read public.events: GRANT SELECT ON public.events
usage: column amount of public.events is double precision; Straum needs an integer type or numeric(p, s) with p at most 18 there
usage: column kind of public.events is event_kind, an enum; write kind::text
usage: column region of public.accounts is text, not an enum; drop the ::text
view usage: measure purchases compares kind with 'purchse', which is no label of event_kind
fact usage: Straum needs the whole old row of every update and delete: ALTER TABLE public.events REPLICA IDENTITY FULL
dim accounts: Straum needs the whole old row of every update and delete: ALTER TABLE public.accounts REPLICA IDENTITY FULL
view usage: join a joins account_id (an integer) to id (text); they must read alike
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
  database and makes `ALTER TABLE` on its tables wait; Straum ends it
  right after the last table loads.
- One plain connection beside the replication connection, for the WAL
  position every second. Nothing else is read after the load.
- WAL decoding per replication connection. Run one Straum per database; a
  second needs another slot and load.

## Without logical decoding

PostgreSQL 17 and 18 refuse a logical slot under `wal_level = replica`,
the default, and changing it takes a restart. `straum run --wal physical`
needs neither: it streams the physical WAL through a temporary physical
slot and rebuilds each change from the pages it holds (ADR-0049). The
commits it yields equal the logical stream's; the tests check that on a
server with both.

`straum plan FILE --wal physical` prints the setup: the role and its
grants, with no `wal_level` change, publication or `REPLICA IDENTITY
FULL`:

```sql
-- Straum setup: run once, as an owner of the tables.
-- --wal physical: wal_level replica and full_page_writes on, the defaults.
CREATE ROLE "straum" LOGIN REPLICATION PASSWORD 'SCRAM-SHA-256$4096:...';
GRANT USAGE ON SCHEMA "public" TO "straum";
GRANT SELECT ON "public"."accounts", "public"."events" TO "straum";
-- Lets Straum start without waiting for the next checkpoint; a superuser grants it.
GRANT EXECUTE ON FUNCTION pg_log_standby_snapshot() TO "straum";
```

The server needs `full_page_writes = on` and a `wal_compression` of
`off`, `pglz` or `lz4`; the start says so when it isn't:

```text
full_page_writes is off; --wal physical needs on
wal_compression is zstd; --wal physical reads off, pglz and lz4
```

A start waits until every transaction open when it began has ended, as a
logical slot's creation does, and for PostgreSQL to log which
transactions run. The last checkpoint logs that, so the wait is short,
except on a server that just restarted, where `pg_log_standby_snapshot()`
logs it at once if the role may run it. After a minute Straum says what
it waits for and tries again:

```text
--wal physical waited 60 s for PostgreSQL to log which transactions run; GRANT EXECUTE ON FUNCTION pg_log_standby_snapshot() TO Straum's role
--wal physical waited 60 s for transactions open before it started to end: xid 7512; a long or prepared one (pg_prepared_xacts) holds the start back
--wal physical waited 60 s for transactions it read the commits of to end: xid 7512; a synchronous standby that doesn't answer holds them
```

The last one: PostgreSQL logs a commit, waits for its synchronous
standbys, then lets snapshots see it. Straum's load must see every
commit it read before its start, so it takes the snapshot again until
then.

What it costs beyond the logical stream:

- The stream carries the whole server's WAL, every database, though
  Straum reads only its tables' pages.
- Memory: the rows of every page of the spec's tables changed since the
  last checkpoint, and the changes of transactions not yet ended. A
  page goes at the next checkpoint, since its next change brings it
  whole. A shorter `checkpoint_timeout` lowers it and raises the WAL.
  Past `--max-page-memory` (1 GiB by default) Straum starts over.

What starts Straum over in physical mode, besides what starts the
logical stream over:

- A value of a column the spec reads that PostgreSQL stored out of line
  in its TOAST table: the page holds only a pointer. Values over about
  2 KB go there. Each change to such a row starts Straum over.
- `TRUNCATE`, `VACUUM FULL`, `CLUSTER` and an `ALTER TABLE` that
  rewrites one of the spec's tables: its rows move to a new file.
  `ADD COLUMN`, `DROP COLUMN` and other DDL that keeps the file are
  followed.
