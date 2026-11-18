# Try it on your data

Does Straum answer the queries you run? Write a spec. Compose copies
the tables it names into a throwaway PostgreSQL and opens the shell.
Their database serves that copy and nothing else: no publication, and
no change to `wal_level`. You need Docker, `curl`, and bash or zsh.
Docker has to reach the database.

## 1. Write a spec

Save as `usage.sql`:

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
([Enum columns](spec.md#enum-columns)).

## 2. Run

In the directory that holds `usage.sql`, with `DATABASE_URL` set to a
role that can `SELECT` the spec's tables:

```bash
docker compose -f <(curl -fsSL https://raw.githubusercontent.com/janbjorge/straum/main/docs/compose/try.yaml) \
  run --rm shell
```

The URL is the DSN `straum run` takes
([Connect](postgresql.md#2-connect)). From inside Docker, `localhost`
is the container. On Docker Desktop, write `host.docker.internal` as
the host.

The file starts PostgreSQL 18 with logical decoding on. `straum copy`
reads the spec and copies each table it names: the columns, the enums
they use, and the rows, in one snapshot. `straum plan` writes the
setup SQL, and `psql` runs it. Straum starts once that has exited, and
is healthy once it can answer. The shell waits for that, for up to an
hour, then takes the terminal. `<(...)` is bash or zsh, so the
terminal stays the shell's input. The file in the repository is
`docs/compose/try.yaml`.

```text
Straum at http://straum:8080. Tab completes; \? lists the commands, \q leaves.
straum>
```

`\d` lists the views, `\d usage` prints a view's `CREATE VIEW`, and
`\rows usage` prints its rows.

`\q` leaves the shell and removes that container. The copy keeps
running, on `127.0.0.1:8080`. Stop a quick start on that port first.
This opens the shell again:

```sh
docker compose -p straum-try exec straum /straum shell
```

An interrupt during the wait stops the wait and leaves the containers
running. The log is `docker compose -p straum-try logs`.
`docker compose -p straum-try down --volumes` removes the copy. Run
that and the command again to load a change to the spec. The file
is not saved. The copy is the database `postgres`.

The copy has the rows, and not the indexes, keys or defaults. A
composite column, or a type an extension provides, stops the copy and
names the column. A table the spec names that the role cannot see
stops it and names the table. So does a row security policy.

From a clone, the spec alone copies its tables from the database the
libpq variables of your shell name (`PGHOST`, `PGUSER`, `PGPASSWORD`,
and the rest), with a read-only role too: the definitions come from
`pg_dump --schema-only` and the rows from `COPY`, into a temporary file
that is restored and removed when the run ends. A dump you already have
can be loaded instead, and the physical stream can be tried:

```sh
just try usage.sql
just try --spec usage.sql --data data.dump --schema schema.sql --port 8080
```

[`try_your_data.py`](https://github.com/janbjorge/straum/blob/main/bench/try_your_data.py)
restores only the data of the tables the spec reads, so the dump's
other tables stay out. It drops the spec tables' indexes, other than
primary keys and unique constraints, for the load and builds them
after, `--jobs` at a time. `--wal physical` tries the [physical
stream](postgresql.md#physical-wal). A run removes what a killed run
of its stack left. Leaving the shell removes both containers. A
statement the restore can't run is printed and skipped. A table the
spec reads but the restore missed stops the run at `straum plan`,
which names it ([Checking a spec](spec.md#checking-a-spec)).

| Flag | Default | |
| --- | --- | --- |
| `SPEC` or `--spec FILE` | required | the spec |
| `--data DUMP` | none: copy from the libpq source | `pg_dump` output to restore |
| `--schema FILE` | none | `pg_dump --schema-only` output, restored first; needs `--data` |
| `--postgres N` | `18` | PostgreSQL major; the dump's or newer |
| `--image IMAGE` | `ghcr.io/janbjorge/straum` | Straum image; a local tag works |
| `--stack NAME` | `straum-try-` and the spec's name | Compose project to run in |
| `--port N` | a free one | local port Straum answers on |
| `--jobs N` | `4` | `pg_restore` jobs and index builds at a time |

`--data` may be a directory, a custom or tar archive, or a plain `.sql`
file. A dump of the whole database needs no `--schema`. The copy from
the source dumps the definitions of the spec's schemas, so their types
and extensions come along, and their other tables are created empty.
Only the spec's tables get rows.

## 3. Ask and compare

In another terminal, ask for events and amounts by region over
November:

```sh
curl --json '{
  "query": "SELECT region, events, amount FROM usage WHERE bucket >= $1 AND bucket < $2 GROUP BY region",
  "parameters": ["2026-11-01T00:00:00Z", "2026-12-01T00:00:00Z"]
}' http://127.0.0.1:8080/ask
```

Run the SQL anyone would write over the raw tables in the copy, and
compare:

```sh
docker compose -p straum-try exec postgres psql -U postgres -c "
  SELECT a.region, count(*) AS events, sum(e.amount) AS amount
    FROM events e LEFT JOIN accounts a ON a.id = e.account_id
   WHERE e.at >= '2026-11-01T00:00:00Z' AND e.at < '2026-12-01T00:00:00Z'
   GROUP BY a.region"
```

Write to the copy through the same `psql` and ask again; the answer
moves. [Asks](asks.md) explains the query and the answer.

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
