# Setting up PostgreSQL

How do you connect your own database? Two steps, done once. Straum
needs PostgreSQL 17 or newer, self-hosted or managed. Its tests run on
17, 18 and the 19 release candidate. It needs no extension, and it
connects as a role that is not a superuser. A superuser runs a few
lines of the setup once.

Pick a stream first: logical decoding, the default, or the physical
WAL, which leaves `wal_level` alone.

| | Physical WAL | Logical decoding |
| --- | --- | --- |
| Run | `straum run FILE --wal physical` | `straum run FILE` |
| Server | `wal_level = replica` and `full_page_writes = on`, both defaults. No restart | PostgreSQL 17 and 18 need `wal_level = logical`, then a restart. From 19, `replica` is enough |
| SQL | A role and `SELECT` | Also a publication and `REPLICA IDENTITY FULL` |

Both streams give the same commits. The tests check that on a server
that runs both. RDS, Aurora and Cloud SQL do not hand out the physical
stream. Use logical decoding there.

## 1. Run the setup SQL

`straum plan FILE --wal physical` prints the physical setup. `straum
plan FILE` prints the logical one. With a DSN set, from `--dsn-file`,
`STRAUM_DSN` or libpq's variables ([Connect](#2-connect)), the role is
the DSN's user and its password goes in as a SCRAM-SHA-256 verifier,
so the password itself is in neither the SQL nor the server's log.
Without a DSN the role is `straum` and the SQL says how to give it a
password. `--role NAME` picks another role
([flags](cli.md#straum-plan)).

`REPLICATION` beside `LOGIN` is a privilege of that role, not another
user. It lets the role follow commits.

Run the SQL as an owner of the tables. A superuser runs the lines the
comments name: creating the role, `wal_level` for logical decoding,
and `BYPASSRLS` where a policy of row-level security hides rows. The
setup lives in the catalog and survives restarts.

### Physical WAL

The physical WAL needs no change to `wal_level`, no publication and no
`REPLICA IDENTITY FULL`, since the pages hold the old row.

```sql
-- Straum setup: run once, as an owner of the tables; a superuser runs the lines said so.
-- --wal physical: wal_level replica and full_page_writes on, the defaults.
-- A superuser creates the role Straum connects as.
-- REPLICATION is a privilege of that role, not another user.
CREATE ROLE "straum" LOGIN REPLICATION PASSWORD 'SCRAM-SHA-256$4096:...';
GRANT USAGE ON SCHEMA "public" TO "straum";
GRANT SELECT ON "public"."accounts", "public"."events" TO "straum";
-- If a policy of row-level security hides rows of a table, a superuser runs:
-- ALTER ROLE "straum" BYPASSRLS;
-- Lets Straum start without waiting for the next checkpoint; a superuser grants it.
GRANT EXECUTE ON FUNCTION pg_log_standby_snapshot() TO "straum";
```

The server needs `full_page_writes = on`, a `wal_compression` of
`off`, `pglz` or `lz4`, and a database in `UTF8`. Straum reads the WAL
of PostgreSQL 17 to 19. A spec table must not be `UNLOGGED`. A column
the spec reads must not be a virtual generated column, the default
from 18, since no page holds its value.

The `GRANT EXECUTE` lets a start proceed without waiting for the next
checkpoint. A start still waits until every transaction open when it
began has ended. After a minute Straum says what it waits for and
exits. After DDL, while streaming, it waits until a snapshot sees the
changed table's columns. That wait gives up after 10 seconds and
Straum stops.
[The physical stream](how-it-works.md#the-physical-stream) says why.

Straum also stops on an insert, update or delete of a row whose value,
in a column the spec reads, is stored out of line in the table's TOAST
table: the page holds only a pointer. PostgreSQL moves the largest
values of a row over about 2 KB there, one by one, until the row fits,
so a short value can go too. A value out of line in a column no view
reads is passed over.

It stops too on a rewrite of a spec table: `TRUNCATE`, `VACUUM FULL`,
`CLUSTER`, or an `ALTER TABLE` that moves the rows to a new file. It
follows `ADD COLUMN`, `DROP COLUMN` and other DDL that keeps the file.

The stream carries the whole server's WAL, every database. Straum
reads only its tables' pages and passes the other records by from
their headers. It keeps the rows of pages changed since the last
checkpoint, and the changes of open transactions. A shorter
`checkpoint_timeout` lowers that memory and raises the WAL. Past
`--max-page-memory` (1 GiB by default) Straum stops.

### Logical decoding

The publication names the tables. `REPLICA IDENTITY FULL` puts the old
row of an update or delete into the stream. Inserts cost no more.
Dims are published and get `FULL` like facts, since Straum holds and
follows them. `--publication NAME` picks another name; give `straum
run` the same one. Straum never asks for `FOR ALL TABLES`, which needs
a superuser too.

```sql
-- Straum setup: run once, as an owner of the tables; a superuser runs the lines said so.
-- A superuser: wal_level takes a restart. On Azure, set the server parameter instead.
ALTER SYSTEM SET wal_level = logical;
-- A superuser creates the role Straum connects as.
-- REPLICATION is a privilege of that role, not another user.
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

PostgreSQL 17 and 18 refuse a logical slot while `wal_level` is
`replica`, and changing it takes a restart. On your own server the
`ALTER SYSTEM` line sets it; restart after. From 19, `replica` is
enough: a logical slot turns decoding on while it exists. On a managed
service, set the provider's parameter instead of `ALTER SYSTEM`, then
restart there:

| Service | Setting |
| --- | --- |
| Amazon RDS and Aurora | [`rds.logical_replication = 1`](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/PostgreSQL.Concepts.General.FeatureSupport.LogicalReplication.html) |
| Google Cloud SQL | [`cloudsql.logical_decoding = on`](https://docs.cloud.google.com/sql/docs/postgres/replication/configure-logical-replication) |
| Google AlloyDB | [`alloydb.logical_decoding = on`](https://docs.cloud.google.com/alloydb/docs/reference/database-flags) |
| Azure Database for PostgreSQL | [`wal_level = logical`](https://learn.microsoft.com/en-us/azure/postgresql/configure-maintain/concepts-logical) |
| Neon | [Enable logical replication](https://neon.com/docs/guides/logical-replication-neon); it can't be turned off again |
| Crunchy Bridge | [Logical replication is enabled by default](https://docs.crunchybridge.com/how-to/logical-replication) |

Some of those grant replication through a role of their own, such as
[`rds_replication`](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/PostgreSQL.Concepts.General.FeatureSupport.LogicalReplication.html)
on Amazon RDS. Grant it to Straum's role.

## 2. Connect

Straum reads its DSN from a file named by `--dsn-file`, else from
`STRAUM_DSN`, never from the command line, where other users could see
it:

```text
postgres://straum:PASSWORD@db.example.com:5432/app
```

The scheme is `postgres://` or `postgresql://`. The port defaults to
5432 and the database to the user's name. An IPv6 address goes in
brackets, `postgres://straum:PASSWORD@[2001:db8::1]/app`.
Percent-encode `@`, `?` and `/` in the password as `%40`, `%3F` and
`%2F`; a raw `?` or `/` is refused. The only parameters Straum takes
are `sslmode` and `sslrootcert`, and it refuses any other. An error
about a parameter never repeats its text, which could be a password's.

With neither the file nor `STRAUM_DSN`, Straum reads libpq's
variables: `PGHOST` or `PGHOSTADDR`, `PGPORT`, `PGUSER`, `PGPASSWORD`,
`PGDATABASE`, `PGSSLMODE` and `PGSSLROOTCERT`. They default as the URL
does, and `PGUSER` to the OS user. `PGHOST` names one host.

Use the server's direct endpoint, not a pooler. The replication
connection needs a session of its own.

Straum connects with TLS and checks that the server's certificate
chains to a trusted root and names the host. Unlike `libpq`, it does
this for every `sslmode` but `disable`:

| `sslmode` | What Straum does |
| --- | --- |
| omitted, `require`, `verify-ca`, `verify-full` | TLS, checking the certificate and the host name |
| `disable` | Plain TCP. Use it only on a network you trust. |
| `allow`, `prefer` | Refused, since they fall back to plain TCP |

The built-in roots are Mozilla's. If your provider signs with a CA of
its own, as Amazon RDS and Cloud SQL do, download its bundle and name
it with `sslrootcert`. The [chart](deploy.md#kubernetes) mounts
its bundle at `/etc/straum-tls/ca.pem`:

```text
postgres://straum:PASSWORD@db.example.com/app?sslrootcert=/etc/straum-tls/ca.pem
```

`sslrootcert=system`, libpq's word for the system's roots, means the
built-in ones.

## What Straum checks at start

Before it loads, Straum compares the server with the spec and reports
every problem at once, each with its fix, in its log. Then it
[exits with 1](deploy.md#states), for whatever runs it to start it
again. A spec table must be a plain table that no table inherits from.
A partitioned table, a view or a TimescaleDB hypertable is refused.

With either stream:

```text
the server is a standby in recovery; give Straum the primary's DSN
PostgreSQL 16 is too old; Straum needs 17 or newer
usage: Straum's role can't read public.events: GRANT SELECT ON public.events
usage: tables inherit from public.events; Straum reads a table without children
usage: row-level security filters what straum reads of public.events; Straum needs every row: ALTER ROLE straum BYPASSRLS, as a superuser
usage: column amount of public.events is double precision; Straum reads no float type, as its sums can't equal PostgreSQL's exactly: make it numeric(p, s) with p at most 18
usage: column kind of public.events is event_kind, an enum; write kind::text
view usage: join a joins account_id (an integer) to id (text); they must read alike
```

A line starts with the view, or with the dim, such as `accounts:`, for
a problem in a dim table.

Once the checks pass, the load holds every spec table in `ACCESS
SHARE` mode until the last table is loaded, so a rewrite (`VACUUM
FULL`, `CLUSTER`, an `ALTER TABLE` that rewrites) waits for the load.
A rewrite that ended before Straum held the tables would read empty in
the load's snapshot. Straum finds it and stops, and the next start
loads the table whole:

```text
usage: public.events was rewritten as Straum started
```

Reading the publication and holding the tables each take a lock on a
spec table. A session that holds `ACCESS EXCLUSIVE` on one, or waits
for it, as an `ALTER TABLE` behind a long transaction does, would make
the start wait as long. Straum waits at most 5 seconds for each lock,
then names the tables and stops:

```text
the start waited 5 s for a lock on public.events; another session holds or waits for one that blocks a read, as ALTER TABLE takes
```

Logical decoding also needs the publication and `REPLICA IDENTITY
FULL`. PostgreSQL makes the logical slot once every writing
transaction that was open when the slot began has ended, in any
database on the server. A start waits a minute for that, then names
them and stops:

```text
wal_level is replica; Straum needs logical, which takes a restart
publication straum does not exist; run the SQL straum plan prints
usage: publication straum leaves out public.events: ALTER PUBLICATION straum ADD TABLE public.events
fact usage: Straum needs the whole old row of every update and delete: ALTER TABLE public.events REPLICA IDENTITY FULL
dim accounts: Straum needs the whole old row of every update and delete: ALTER TABLE public.accounts REPLICA IDENTITY FULL
the start waited 60 s for transactions open on the server to end before its slot could be made: xid 7512; a long or prepared one (pg_prepared_xacts) holds the start back
```

With the physical WAL, the last three lines below are the waits under
[Physical WAL](#physical-wal); the last says 10 s when it is the wait
after DDL:

```text
full_page_writes is off; --wal physical needs on
wal_compression is zstd; --wal physical reads off, pglz and lz4
--wal physical reads the WAL of PostgreSQL 17 to 19, not 20; read it through logical decoding
the database's encoding is LATIN1; --wal physical reads UTF8 text
usage: public.events is unlogged, so the WAL holds none of its changes: ALTER TABLE public.events SET LOGGED
usage: column kind of public.events is virtual, so the WAL holds no value of it; --wal physical needs it STORED
--wal physical waited 60 s for PostgreSQL to log which transactions run; GRANT EXECUTE ON FUNCTION pg_log_standby_snapshot() TO Straum's role
--wal physical waited 60 s for transactions open before it started to end: xid 7512; a long or prepared one (pg_prepared_xacts) holds the start back
--wal physical waited 60 s for transactions it read the commits of to end: xid 7512; a synchronous standby that doesn't answer holds them
```

## What Straum costs the server

Each running Straum takes one replication connection and one slot, out
of `max_wal_senders` and `max_replication_slots`. Managed services
often keep some of each for themselves.

The server keeps WAL until Straum confirms it, which Straum does every
second. Past `--max-retained-wal` (4 GiB by default) Straum stops, so
one that cannot keep up cannot fill the disk.

A watchdog on a connection of its own checks that every second, during
loads too. Past the limit it ends the slot's session, even if the rest
of Straum is stuck. A watchdog that cannot read the slot for 10 seconds
ends the stream too. Many managed services fix `max_slot_wal_keep_size` at -1,
so the flag above is the only limit there. A physical slot keeps WAL
from the last checkpoint's redo point, which the server keeps until its
next checkpoint anyway. The watchdog counts that slot's WAL from where
it was made.

Nothing stays after Straum is gone. The slot is temporary, so
PostgreSQL drops it when the connection closes, crashes and deleted
pods included. Temporary slots do not move to a standby, so after a
failover the next start loads from the new primary.

Each load takes a snapshot. It holds back vacuum for the whole database
and makes `ALTER TABLE` on its tables wait. Straum ends it right after
the last table loads.

Beside the replication connection, Straum opens plain connections: one
for the WAL position every second and one for the watchdog, and with
`--wal physical` one for the load and one for table layouts. Nothing
else is read after the load. A role `CONNECTION LIMIT` needs room for
them all.

The server decodes the WAL once per replication connection. Run one
Straum per database; a second needs another slot and load.

What `--wal physical` adds, the whole server's WAL and the page
memory, is under [Physical WAL](#physical-wal).

Next: [Deploying Straum](deploy.md), to run it next to your apps.
