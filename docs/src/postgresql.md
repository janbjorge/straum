# Setting up PostgreSQL

Straum needs PostgreSQL 17 or newer, self-hosted or managed, and no
extension; its tests run on 17, 18 and 19. It connects as a role that
is not a superuser; a superuser runs a few setup lines once.

`--wal` picks the stream; `straum run` and `straum plan` both need it.
Logical decoding reads changes through a publication; the physical
write-ahead log (WAL) reads the WAL itself. The server's `wal_level`
decides which it can serve:

```sql
SHOW wal_level;
```

Pick a stream it serves, or change the setting, which takes a restart.
A managed service sets it through its own parameter ([Managed
services](#managed-services)):

| `wal_level` | `--wal logical` | `--wal physical` |
| --- | --- | --- |
| `logical` | Yes | Yes |
| `replica`, the default | From 19. On 17 and 18, set `logical` first | Yes |
| `minimal` | Set `logical` first | Set `replica` first |

What else each stream needs:

| | `--wal logical` | `--wal physical` |
| --- | --- | --- |
| Setup SQL | A role, `SELECT`, a publication and `REPLICA IDENTITY FULL` | A role, `SELECT` and one `GRANT EXECUTE` |
| Where | Any server, managed ones included | Not RDS, Aurora or Cloud SQL, which don't hand out the physical stream |
| Generated columns | Stored ones from 18, once the publication sets `publish_generated_columns = stored`; none on 17 | Stored ones |

Both give the same commits; the tests check that on a server running
both.

## 1. Run the setup SQL

`straum plan` prints the SQL to run once, as an owner of the tables;
its comments mark the superuser lines. For `usage`:

```sh
straum plan usage.sql --wal logical
```

```sql
-- Straum setup: run once, as an owner of the tables; a superuser runs the lines said so.
-- A superuser: wal_level takes a restart. On Azure, set the server parameter instead.
ALTER SYSTEM SET wal_level = logical;
-- A superuser creates the role Straum connects as.
-- REPLICATION is a privilege of that role, not another user.
CREATE ROLE "straum" LOGIN REPLICATION;
-- Then give "straum" a password: \password "straum"
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

With `--wal physical`, the `ALTER SYSTEM`, publication and `REPLICA
IDENTITY` lines go, and one comes in:

```sql
-- Lets Straum start without waiting for the next checkpoint; a superuser grants it.
GRANT EXECUTE ON FUNCTION pg_log_standby_snapshot() TO "straum";
```

[`straum plan`](cli.md#straum-plan) lists its flags, such as `--role`
and `--publication`.

### Managed services

Where a service doesn't give out the physical stream, logical decoding
is the only one. On 17 and 18, set the provider's parameter instead of
`ALTER SYSTEM` and restart there:

| Service | Setting |
| --- | --- |
| Amazon RDS and Aurora | [`rds.logical_replication = 1`](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/PostgreSQL.Concepts.General.FeatureSupport.LogicalReplication.html) |
| Google Cloud SQL | [`cloudsql.logical_decoding = on`](https://docs.cloud.google.com/sql/docs/postgres/replication/configure-logical-replication) |
| Google AlloyDB | [`alloydb.logical_decoding = on`](https://docs.cloud.google.com/alloydb/docs/reference/database-flags) |
| Neon | [Enable logical replication](https://neon.com/docs/guides/logical-replication-neon) |

Some grant replication through a role of their own, such as
[`rds_replication`](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/PostgreSQL.Concepts.General.FeatureSupport.LogicalReplication.html)
on Amazon RDS; grant it to Straum's role.

### Physical WAL

Straum reads the WAL of PostgreSQL 17 to 19. The server needs
`full_page_writes = on` (the default), `wal_compression` set to `off`,
`pglz` or `lz4`, and a `UTF8` database. A spec table must not be
`UNLOGGED`, and a column the spec reads must not be a virtual
generated column (the default from 18): the WAL holds neither.

Straum stops on a row inserted, updated or deleted whose value in a
column the spec reads is stored out of line in the table's TOAST (The
Oversized-Attribute Storage Technique) table, since the page holds
only a pointer. PostgreSQL moves a row's largest values there until
the row fits in about 2 KB, so a short value can go too. The log
says:

```text
straum: a change no longer fits trips: a value of a column Straum reads is stored out of line, and the stream holds only a pointer to it, not the value
```

It stops too on a rewrite of a spec table: `TRUNCATE`, `VACUUM FULL`,
`CLUSTER`, or an `ALTER TABLE` that moves the rows to a new file. It
follows `ADD COLUMN`, `DROP COLUMN` and other data definition language
(DDL) that keeps the file once a snapshot sees the new columns, and
stops after 10 seconds without one. [The physical
stream](how-it-works.md#the-physical-stream) says why.

## 2. Connect

Straum reads its data source name (DSN) from the file `--dsn-file`
names, else from `STRAUM_DSN`, never from the command line:

```text
postgres://straum:PASSWORD@db.example.com:5432/app
```

The scheme is `postgres://` or `postgresql://`. The port defaults to
5432 and the database to the user's name. Percent-encode `@`, `?` and
`/` in the password; a raw `?` or `/` is refused. Only the parameters
`sslmode` and `sslrootcert` are taken. An error about a parameter
never repeats its text, which could be a password's.

With neither the file nor `STRAUM_DSN`, Straum reads libpq's
variables: `PGHOST` or `PGHOSTADDR`, `PGPORT`, `PGUSER`, `PGPASSWORD`,
`PGDATABASE`, `PGSSLMODE` and `PGSSLROOTCERT`. They default as the URL
does, and `PGUSER` to the OS user. `PGHOST` names one host.

Use the server's direct endpoint, not a pooler: the replication
connection needs a session of its own.

Unlike `libpq`, Straum checks that the certificate chains to a trusted
root and names the host for every `sslmode` but `disable`:

| `sslmode` | What Straum does |
| --- | --- |
| omitted, `require`, `verify-ca`, `verify-full` | TLS, checking the certificate and the host name |
| `disable` | Plain TCP |
| `allow`, `prefer` | Refused, since they fall back to plain TCP |

The built-in roots are Mozilla's, and `sslrootcert=system` (libpq's
word for the system's roots) means them. If your provider signs with
its own certificate authority (CA), as Amazon RDS and Cloud SQL do,
download its bundle and name it with `sslrootcert`. The
[chart](deploy.md#kubernetes) mounts its bundle at
`/etc/straum-tls/ca.pem`:

```text
postgres://straum:PASSWORD@db.example.com/app?sslrootcert=/etc/straum-tls/ca.pem
```

## What Straum checks at start

Before it loads, Straum compares the server with the spec, logs every
problem at once with its fix, and [exits with 1](deploy.md#states) so
whatever runs it starts it again. A spec table must be a plain table
no table inherits from; a partitioned table, a view or a TimescaleDB
hypertable is refused. A line starts with the view, or with the dim,
such as `accounts:`.

Straum connects with `application_name` `straum` and confirms each
commit it reads. A `synchronous_standby_names` holding `*` or `straum`
could count it as a synchronous standby: a commit only Straum
confirmed is then lost on failover, and a Straum stopped holds up
commits. Straum refuses to start under one; list your standbys by
name instead.

```text
the server is a standby in recovery; give Straum the primary's DSN
PostgreSQL 16 is too old; Straum needs 17 or newer
synchronous_standby_names is 'ANY 1 (*)', which names Straum's connection, application_name straum: a commit only Straum confirmed could be lost on failover; name your standbys there, not * or straum
usage: Straum's role can't read public.events: GRANT SELECT ON public.events
usage: tables inherit from public.events; Straum reads a table without children
usage: row-level security filters what straum reads of public.events; Straum needs every row: ALTER ROLE straum BYPASSRLS, as a superuser
usage: column amount of public.events is double precision; Straum reads no float type, as its sums can't equal PostgreSQL's exactly: make it numeric(p, s) with p at most 18
usage: column kind of public.events is event_kind, an enum; write kind::text
view usage: join a joins account_id (an integer) to id (text); they must read alike
```

Logical decoding adds:

```text
wal_level is replica; Straum needs logical, which takes a restart
publication straum does not exist; run the SQL straum plan prints
usage: publication straum leaves out public.events: ALTER PUBLICATION straum ADD TABLE public.events
usage: column amount of public.events is generated, which pgoutput on PostgreSQL 17 can't publish: read the columns it is computed from, or follow it with --wal physical
usage: publication straum leaves out generated column amount of public.events: ALTER PUBLICATION straum SET (publish_generated_columns = stored), or name it in the publication's column list
fact usage: Straum needs the whole old row of every update and delete: ALTER TABLE public.events REPLICA IDENTITY FULL
dim accounts: Straum needs the whole old row of every update and delete: ALTER TABLE public.accounts REPLICA IDENTITY FULL
```

The physical WAL adds:

```text
full_page_writes is off; --wal physical needs on
wal_compression is zstd; --wal physical reads off, pglz and lz4
--wal physical reads the WAL of PostgreSQL 17 to 19, not 20; read it through logical decoding
the database's encoding is LATIN1; --wal physical reads UTF8 text
usage: public.events is unlogged, so the WAL holds none of its changes: ALTER TABLE public.events SET LOGGED
usage: column kind of public.events is virtual, so the WAL holds no value of it; --wal physical needs it STORED
```

A start waits for every writing transaction open when it began, in any
database on the server, to end. The physical WAL also waits for
PostgreSQL to log which transactions run (the `GRANT EXECUTE` speeds
that up) and for the commits it read to reach the load's snapshot.
After a minute Straum names what holds it back and stops:

```text
the start waited 60 s for transactions open on the server to end before its slot could be made: xid 7512; a long or prepared one (pg_prepared_xacts) holds the start back
--wal physical waited 60 s for PostgreSQL to log which transactions run; GRANT EXECUTE ON FUNCTION pg_log_standby_snapshot() TO Straum's role
--wal physical waited 60 s for transactions open before it started to end: xid 7512; a long or prepared one (pg_prepared_xacts) holds the start back
--wal physical waited 60 s for transactions it read the commits of to end: xid 7512; a synchronous standby that doesn't answer holds them
```

The last line says 10 s when it is the wait after DDL ([Physical
WAL](#physical-wal)).

The load holds every spec table in `ACCESS SHARE` mode until the last
one is loaded, so a rewrite waits for it. A rewrite that ended just
before would read empty in the load's snapshot, so Straum stops and
the next start loads the table whole. Straum waits at most 5 seconds
for each lock, so a session holding or queued for `ACCESS EXCLUSIVE`,
as an `ALTER TABLE` behind a long transaction is, stops the start
rather than stalls it:

```text
usage: public.events was rewritten as Straum started
the start waited 5 s for a lock on public.events; another session holds or waits for one that blocks a read, as ALTER TABLE takes
```

## What Straum costs the server

Each Straum takes one replication connection and one slot, out of
`max_wal_senders` and `max_replication_slots`, of which managed
services often keep some. It also opens plain connections, one for
the WAL position and one for the watchdog, and with `--wal physical`
two more, for the load and for table layouts; a role `CONNECTION
LIMIT` needs room for them. Run one Straum per database: a second needs
another slot and load, and with logical decoding the server decodes
the WAL once for each.

The server keeps WAL until Straum confirms it, every second. Past
`--max-retained-wal` (4 GiB by default) Straum stops, so one that
cannot keep up cannot fill the disk. A watchdog on its own connection
checks that every second, during loads too. Past the limit, or when it
cannot read the slot for 10 seconds, it ends the slot's session, even
if the rest of Straum is stuck. Many managed services fix
`max_slot_wal_keep_size` at -1, so the flag is the only limit there. A
physical slot keeps WAL from the last checkpoint's redo point, which
the server keeps until its next checkpoint anyway; the watchdog counts
from where the slot was made.

With logical decoding, `REPLICA IDENTITY FULL` makes each update and
delete of a spec table log its whole old row, so they write more WAL;
inserts don't.

The physical stream carries the whole server's WAL, every database.
Straum reads only its tables' pages and skips other records by their
headers. It holds the rows of pages changed since the last checkpoint
and the changes of open transactions, and stops past
`--max-page-memory` (1 GiB by default). A shorter `checkpoint_timeout`
lowers that memory and raises the WAL.

Each load's snapshot holds back vacuum for the whole database and
makes `ALTER TABLE` on its tables wait. Straum ends it right after the
last table loads.

The slot is temporary, so PostgreSQL drops it when the connection
closes, crashes and deleted pods included. It does not move to a
standby, so after a failover the next start loads from the new primary.

Next: [Deploying Straum](deploy.md), to run it next to your apps.
