# Try it on your data

Does Straum answer the queries you run? Write a spec and run one
command, and Straum's shell opens on a copy of your tables. The copy
lives in Docker until you remove it. Your database is only read: it
gets no publication or role, and its `wal_level` stays as it is.

You need:

- Docker with Compose.
- A role that can `SELECT` the tables the spec reads. Read-only is
  enough.
- A database Docker can reach.

## 1. Write a spec

Make an empty directory and save this in it as `usage.sql`:

```sql
CREATE VIEW usage WITH (buckets = '1 hour, 1 day, 7 days') AS
SELECT date_bin('1 hour', e.at, timestamptz 'epoch') AS bucket,
       e.kind::text AS kind,
       e.account_id AS account_id,
       a.region AS region,
       count(*) AS events,
       sum(e.amount) AS amount
FROM public.events AS e
LEFT OUTER JOIN public.accounts AS a ON a.id = e.account_id
GROUP BY 1, 2, 3, 4;
```

Write the `GROUP BY` your app runs most: the time column in
`date_bin`, the columns it filters and groups by as keys, its
aggregates as measures ([Spec](spec.md)). Here `kind` is an enum,
which Straum reads as text through `::text`
([Enum columns](spec.md#enum-columns)). The file must be named
`usage.sql`, whatever its views are called.

## 2. Get the Compose file

In the same directory:

```sh
curl -fsSLO https://raw.githubusercontent.com/janbjorge/straum/main/docs/compose/try.yaml
```

## 3. Point it at your database

```sh
export DATABASE_URL='postgres://reader:secret@db.example.com:5432/app?sslmode=require'
```

Any connection string `psql` takes works, including
`host=... user=... password=... dbname=...`. Your `PG*` variables do
not reach the containers, so put everything in this one string. Inside
Docker, `localhost` is the container: for a database on your machine,
write `host.docker.internal`.

## 4. Start

```sh
docker compose -f try.yaml run --rm shell
```

This copies the tables, starts Straum on the copy and waits until it
has loaded. Large tables take a while. When the prompt shows, it is
ready:

```text
Straum at http://straum:8080. Tab completes; \? lists the commands, \q leaves.
straum>
```

`\d` lists the views, `\d usage` prints a view's `CREATE VIEW`, and
`\rows usage` prints its rows.

## 5. Ask and compare

Ask in the shell, or from another terminal. Events and amounts by
region over November:

```sh
curl --json '{
  "query": "SELECT region, events, amount FROM usage WHERE bucket >= $1 AND bucket < $2 GROUP BY region",
  "parameters": ["2026-11-01T00:00:00Z", "2026-12-01T00:00:00Z"]
}' http://127.0.0.1:8080/ask
```

Run the SQL anyone would write over the copy, and compare:

```sh
docker compose -p straum-try exec postgres psql -U postgres -c "
  SELECT a.region, count(*) AS events, sum(e.amount) AS amount
    FROM events e LEFT JOIN accounts a ON a.id = e.account_id
   WHERE e.at >= '2026-11-01T00:00:00Z' AND e.at < '2026-12-01T00:00:00Z'
   GROUP BY a.region"
```

Write to the copy through the same `psql` and ask again, and the
answer follows the write. [Asks](asks.md) explains the query and the answer.

## 6. Change the spec, or stop

`\q` leaves the shell. Straum keeps running on `127.0.0.1:8080`, so
stop a quick start on that port first. To open the shell again:

```sh
docker compose -p straum-try exec straum /straum shell
```

To load a changed spec, remove the copy and run step 4 again. This
reads your database again:

```sh
docker compose -p straum-try down --volumes
```

Run the same command when you are done.

## When it fails

The command stops at the first step that fails. Each step has a log:

```sh
docker compose -p straum-try logs export
```

`export` reads your database, `load` fills the copy, `straum` runs
Straum. Before it copies anything, the export names every column it
can't copy: a table or column the role can't see, a composite column,
a type an extension provides. A row security policy that would hide
rows stops it too.

Ctrl-C while it waits stops only the wait, and the containers keep
running. Remove them as in step 6.

The copy holds the rows, with only the columns the spec reads. It has
no indexes, keys or defaults.

## What the command does

```text
scripts ─► export ─► load ─► plan ─► apply ─► straum ─► shell
                     ▲
postgres ────────────┘
```

1. `scripts`: [`straum plan --copy`](cli.md#straum-plan) writes two
   psql scripts, `export.sql` and `load.sql`.
2. `export`: `psql` runs `export.sql` on `DATABASE_URL`. In one
   read-only snapshot it writes the columns the spec reads to CSV
   files.
3. `postgres`: a new PostgreSQL 18 with logical decoding on. `load`
   runs `load.sql` there, which creates the tables and loads the CSVs.
4. `plan` and `apply`: `straum plan` writes the setup SQL for the copy,
   and `psql` runs it.
5. `straum`: `straum run` loads the copy and follows it.
6. `shell`: `straum shell`, once Straum is ready.

The containers run as the Compose project `straum-try`. The copy is
the database `postgres` in it.

## From a clone: make the copy once

The Compose file reads your database on every start. From a clone of
the repository, `just try` loads a copy you made once instead, and can
try the [physical stream](postgresql.md#physical-wal):

```sh
straum plan usage.sql --copy copy
psql "$DATABASE_URL" -f copy/export.sql
just try usage.sql --data copy
```

The first line writes the scripts into `copy/`. The second fills it
with CSV files; `psql` reads your `PG*` variables here, so a
`DATABASE_URL` is optional. The third loads it into a throwaway
PostgreSQL, starts Straum and opens the shell. Leaving the shell
removes both containers; `copy/` stays.

One copy can serve several specs: give `straum plan --copy` all of
them, then `just try` each. A run first removes the containers a
killed run of the same stack left behind.
[`try_your_data.py`](https://github.com/janbjorge/straum/blob/main/bench/try_your_data.py)
is the script behind it:

| Flag | Default | |
| --- | --- | --- |
| `SPEC` or `--spec FILE` | required | the spec |
| `--data DIR` | required | the directory `straum plan --copy` wrote, after its `export.sql` ran |
| `--wal logical\|physical` | `logical` | the stream Straum reads |
| `--postgres N` | `18` | PostgreSQL major |
| `--image IMAGE` | `ghcr.io/janbjorge/straum` | Straum image; a local tag works |
| `--stack NAME` | `straum-try-` and the spec's name | Compose project to run in |
| `--port N` | a free one | local port Straum answers on |

## On the database itself

Once the answers hold, point Straum at the database:

1. `straum plan FILE` prints the setup SQL, or what is wrong with the
   spec. A table owner runs the SQL once. You do not edit it.
2. `straum run FILE` with `STRAUM_DSN` or libpq's `PGHOST` and the
   rest set, then ask.

[Setting up PostgreSQL](postgresql.md) is that SQL for either stream,
and what a failed line means.

Start with few keys. Memory follows cells, one per distinct
combination of keys and bucket ([Cells](how-it-works.md#cells)).

Next: [How Straum works](how-it-works.md), what Straum keeps and how it
stays current.
