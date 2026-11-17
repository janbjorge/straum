# Setting up PostgreSQL

How do you connect your own database? Straum supports PostgreSQL 17 and
newer, self-hosted or managed; its tests run on 17, 18 and the 19
release candidate. It uses logical replication with `pgoutput`, a
temporary slot and an exported snapshot, with no extension and no
superuser. Setup is three steps, done once. Where `wal_level = logical`
can't be had, Straum can read the physical WAL instead: see [Without
logical decoding](#without-logical-decoding).

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
For the `usage` view of [Spec](spec.md#synopsis):

```sql
-- Straum setup: run once, as an owner of the tables; a superuser runs the lines said so.
-- A superuser: wal_level takes a restart. On Azure, set the server parameter instead.
ALTER SYSTEM SET wal_level = logical;
-- A superuser: only one may give a role REPLICATION.
CREATE ROLE "straum" LOGIN REPLICATION PASSWORD 'SCRAM-SHA-256$4096:...';
GRANT USAGE ON SCHEMA "public" TO "straum";
GRANT SELECT ON "public"."accounts", "public"."events" TO "straum";
-- If a policy of row-level security hides rows of a table, a superuser runs:
-- ALTER ROLE "straum" BYPASSRLS;
CREATE PUBLICATION "straum" FOR TABLE "public"."accounts", "public"."events";
-- fact usage: Straum needs the old row of every update and delete; inserts cost no more.
ALTER TABLE "public"."events" REPLICA IDENTITY FULL;
-- dim accounts: Straum holds its key and the columns the views read, and follows it.
ALTER TABLE "public"."accounts" REPLICA IDENTITY FULL;
```

Run it as an owner of the tables, since only an owner can add a table to a
publication. The lines said so need a superuser: `ALTER SYSTEM`, a role
with `REPLICATION`, and `BYPASSRLS` where a table needs it. Straum never
asks for `FOR ALL TABLES`, which needs a superuser too. The setup lives in the catalog and survives restarts.

- Some managed services grant replication through a role of their own,
  such as
  [`rds_replication`](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/PostgreSQL.Concepts.General.FeatureSupport.LogicalReplication.html)
  on Amazon RDS: grant it to Straum's role.
- `--role NAME` and `--publication NAME` pick other names
  ([flags](cli.md#straum-plan)); give `straum run` the same publication.
- Dims are published and get `REPLICA IDENTITY FULL` like facts, since
  Straum holds and follows them.

## 3. Connect

Straum reads its DSN from `STRAUM_DSN` or from a file named by
`--dsn-file`, never from the command line, where other users could see
it:

```text
postgres://straum:PASSWORD@db.example.com:5432/app
```

The port defaults to 5432 and the database to the user's name. An IPv6
address goes in brackets, `postgres://straum:PASSWORD@[2001:db8::1]/app`.
Percent-encode reserved characters in the password, such as `@`, `?`
or `/` as `%40`, `%3F` and `%2F`: a raw one is refused, and an error
about a parameter never repeats its text, which could be a password's.
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

`sslrootcert=system`, libpq's word for the system's roots, means the
built-in ones.

## What Straum checks at start

Before it loads, Straum compares the server with the spec and reports
every problem at once, each with its fix, in its log. Then it
[exits with 1](deploy.md#states), for whatever runs it to start it
again. A spec table must be a plain table that no table inherits
from: a partitioned table, a view or a TimescaleDB hypertable is
refused. Some of the checks:

```text
wal_level is replica; Straum needs logical, which takes a restart
the server is a standby in recovery; give Straum the primary's DSN
publication straum does not exist; run the SQL straum plan prints
usage: publication straum leaves out public.events: ALTER PUBLICATION straum ADD TABLE public.events
usage: Straum's role can't read public.events: GRANT SELECT ON public.events
usage: tables inherit from public.events; Straum reads a table without children
usage: row-level security filters what straum reads of public.events; Straum needs every row: ALTER ROLE straum BYPASSRLS, as a superuser
usage: column amount of public.events is double precision; Straum reads no float type, as its sums can't equal PostgreSQL's exactly: make it numeric(p, s) with p at most 18
usage: column kind of public.events is event_kind, an enum; write kind::text
usage: column region of public.accounts is text, not an enum; drop the ::text
view usage: measure purchases compares kind with 'purchse', which is no label of event_kind
fact usage: Straum needs the whole old row of every update and delete: ALTER TABLE public.events REPLICA IDENTITY FULL
dim accounts: Straum needs the whole old row of every update and delete: ALTER TABLE public.accounts REPLICA IDENTITY FULL
view usage: join a joins account_id (an integer) to id (text); they must read alike
```

Once the checks pass, the load holds each spec table in `ACCESS SHARE`
mode until its last `COPY`, so a rewrite (`VACUUM FULL`, `CLUSTER`, an
`ALTER TABLE` that rewrites) waits for the load. A rewrite that ended
before Straum held the tables would read empty in the load's snapshot;
Straum finds it and stops, and the next start loads it whole:

```text
usage: public.events was rewritten as Straum started
```

PostgreSQL makes a logical slot once every transaction that writes and
was open when the slot began has ended, on any table of the database. A
start waits a minute for that, then names them and stops:

```text
the start waited 60 s for transactions open on the server to end before its slot could be made: xid 7512; a long or prepared one (pg_prepared_xacts) holds the start back
```

## What Straum costs the server

- One replication connection and one slot per running Straum, out of
  `max_wal_senders` and `max_replication_slots`. Managed services often
  keep some of each for themselves.
- WAL, until Straum confirms it, which it does every second. If the server
  holds more than `--max-retained-wal` for the slot (4 GiB by default),
  Straum stops, so a Straum that can't keep up can't fill the disk.
  A watchdog on a connection of its own checks this every second, during
  loads too, and has PostgreSQL end the slot's session, which drops the
  slot, even if the rest of Straum is stuck. A watchdog that can't read
  the slot for 10 seconds, for whatever reason, ends the stream too. Many managed services
  fix `max_slot_wal_keep_size` at -1, so this is the only limit there.
  A physical slot (`--wal physical`) keeps WAL from the last
  checkpoint's redo point, which the server keeps until its next
  checkpoint anyway; the watchdog counts that slot's WAL from where it
  was made.
- Nothing after Straum is gone: the slot is temporary, so PostgreSQL drops
  it when the connection closes, crashes and deleted pods included.
  Temporary slots don't move to a standby either, so after a failover
  the next start loads from the new primary.
- A snapshot during each load, which holds back vacuum for the whole
  database and makes `ALTER TABLE` on its tables wait; Straum ends it
  right after the last table loads.
- Plain connections beside the replication connection: one for the WAL
  position every second and one for the watchdog, and with `--wal
  physical` one for the load and one for table layouts. Nothing else is
  read after the load. A role `CONNECTION LIMIT` needs room for them all.
- WAL decoding per replication connection. Run one Straum per database; a
  second needs another slot and load.

## Without logical decoding

PostgreSQL 17 and 18 refuse a logical slot under `wal_level = replica`,
the default, and changing it takes a restart. `straum run --wal physical`
needs neither: it streams the physical WAL through a temporary physical
slot and rebuilds each change from the pages it holds (ADR-0008). The
commits it yields equal the logical stream's; the tests check that on a
server with both.

`straum plan FILE --wal physical` prints the setup: the role and its
grants, with no `wal_level` change, publication or `REPLICA IDENTITY
FULL`:

```sql
-- Straum setup: run once, as an owner of the tables; a superuser runs the lines said so.
-- --wal physical: wal_level replica and full_page_writes on, the defaults.
-- A superuser: only one may give a role REPLICATION.
CREATE ROLE "straum" LOGIN REPLICATION PASSWORD 'SCRAM-SHA-256$4096:...';
GRANT USAGE ON SCHEMA "public" TO "straum";
GRANT SELECT ON "public"."accounts", "public"."events" TO "straum";
-- If a policy of row-level security hides rows of a table, a superuser runs:
-- ALTER ROLE "straum" BYPASSRLS;
-- Lets Straum start without waiting for the next checkpoint; a superuser grants it.
GRANT EXECUTE ON FUNCTION pg_log_standby_snapshot() TO "straum";
```

The server needs `full_page_writes = on`, a `wal_compression` of `off`,
`pglz` or `lz4`, and a database in `UTF8`, since the stream reads text
from the pages. A spec table must not be `UNLOGGED`, since such
a table writes no WAL, and a column the spec reads must not be a virtual
generated column, the default from 18, since no page holds its value.
The start says so:

```text
full_page_writes is off; --wal physical needs on
wal_compression is zstd; --wal physical reads off, pglz and lz4
--wal physical reads the WAL of PostgreSQL 17 to 19, not 20; read it through logical decoding
the database's encoding is LATIN1; --wal physical reads UTF8 text
usage: public.events is unlogged, so the WAL holds none of its changes: ALTER TABLE public.events SET LOGGED
usage: column kind of public.events is virtual, so the WAL holds no value of it; --wal physical needs it STORED
```

A start waits until every transaction open when it began has ended, as a
logical slot's creation does, and for PostgreSQL to log which
transactions run. The last checkpoint logs that, so the wait is short,
except on a server that just restarted, where `pg_log_standby_snapshot()`
logs it at once if the role may run it. After a minute Straum says what
it waits for and exits, to be started again:

```text
--wal physical waited 60 s for PostgreSQL to log which transactions run; GRANT EXECUTE ON FUNCTION pg_log_standby_snapshot() TO Straum's role
--wal physical waited 60 s for transactions open before it started to end: xid 7512; a long or prepared one (pg_prepared_xacts) holds the start back
--wal physical waited 60 s for transactions it read the commits of to end: xid 7512; a synchronous standby that doesn't answer holds them
```

The last wait comes after DDL too, while Straum streams: it reads a
changed table's columns once a snapshot sees the DDL. That one gives up
after 10 s, says so as above, and Straum stops.

[The physical stream](how-it-works.md#the-physical-stream) says why
the start waits.

What it costs beyond the logical stream:

- The stream carries the whole server's WAL, every database, though
  Straum reads only its tables' pages: it passes the other records by
  from their headers, without copying or checksumming them.
- Memory: the rows of the spec's pages changed since the last
  checkpoint, and the changes of open transactions. A shorter
  `checkpoint_timeout` lowers it and raises the WAL. Past
  `--max-page-memory` (1 GiB by default) Straum stops.

What stops Straum in physical mode, besides what stops the logical
stream:

- A value of a column the spec reads that PostgreSQL stored out of line
  in its TOAST table: the page holds only a pointer. A row over about
  2 KB has its largest values moved there, one by one, until it fits,
  so a short value can go too, beside long ones. Each change to such a
  row stops Straum.
- `TRUNCATE`, `VACUUM FULL`, `CLUSTER` and an `ALTER TABLE` that
  rewrites one of the spec's tables: its rows move to a new file.
  `ADD COLUMN`, `DROP COLUMN` and other DDL that keeps the file are
  followed.

Next: [Deploying Straum](deploy.md), to run it next to your apps.
