# Try it on your data

Does Straum answer the queries you run? Copy your tables into a
throwaway PostgreSQL in Docker and run Straum on the copy. The
database they came from serves the dump and nothing else. You need
Docker, `curl` and `pg_dump`, and bash or zsh.

The dump holds your rows. Keep it where your data may be.

## 1. Dump the tables

Take the whole schema, then the tables the spec reads:

```sh
pg_dump "$DATABASE_URL" --schema-only --no-owner --no-privileges -f schema.sql
pg_dump "$DATABASE_URL" --format=directory --jobs=4 \
  -t public.events -t public.accounts -f data.dump
```

`-t` leaves out what the tables depend on: their types, extensions
and the tables their foreign keys name. The schema dump brings those.
`pg_dump` takes one snapshot, so the tables agree with each other. It
must be the server's major version or newer.

## 2. Write a spec

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

## 3. Run

In the directory that holds `schema.sql`, `data.dump` and `usage.sql`:

```bash
docker compose -f <(curl -fsSL https://raw.githubusercontent.com/janbjorge/straum/main/docs/compose/try.yaml) \
  run --rm shell
```

The file starts PostgreSQL 18 with logical decoding on. `psql` runs
`schema.sql`. `pg_restore` loads the rows in `data.dump`, with foreign
keys and triggers off, because a `-t` dump can omit a table a key
names. `straum plan` writes the setup SQL for `usage.sql`, and `psql`
runs it. Straum starts once that has exited, and is healthy once it
can answer. The shell waits for that, for up to an hour, then takes
the terminal. `<(...)` is bash or zsh, so the terminal stays the
shell's input. The file in the repository is `docs/compose/try.yaml`.

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
that and the command again to load a change to the three files. The
file is not saved. The copy is the database `postgres`.

A statement in `schema.sql` that the image cannot run, such as an
extension it lacks, stops the run and prints the line. So does a table
the spec names that the schema restore missed, when `psql` runs the
setup. The indexes in the schema stay. On a large dump that is slow.

From a clone, this is faster, and it can try the physical stream:

```sh
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
| `--spec FILE` | required | the spec |
| `--data DUMP` | required | `pg_dump` output to restore |
| `--schema FILE` | none | `pg_dump --schema-only` output, restored first |
| `--postgres N` | `18` | PostgreSQL major; the dump's or newer |
| `--image IMAGE` | `ghcr.io/janbjorge/straum` | Straum image; a local tag works |
| `--stack NAME` | `straum-try-` and the spec's name | Compose project to run in |
| `--port N` | a free one | local port Straum answers on |
| `--jobs N` | `4` | `pg_restore` jobs and index builds at a time |

`--data` may be a directory, a custom or tar archive, or a plain `.sql`
file. A dump of the whole database needs no `--schema`.

## 4. Ask and compare

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

Once the answers hold, point Straum at the database you dumped:

1. `straum plan FILE` prints the setup SQL, or what is wrong with the
   spec. A table owner runs the SQL once. You do not edit it.
2. `straum run FILE` with `STRAUM_DSN` set, then ask.

[Setting up PostgreSQL](postgresql.md) is that SQL for either stream,
and what a failed line means.

Start with few keys. Memory follows cells, one per distinct
combination of keys and bucket ([Cells](how-it-works.md#cells)).

Next: [How Straum works](how-it-works.md), what Straum keeps and how it
stays current.
