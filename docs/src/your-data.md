# Try it on your data

Does Straum answer the queries you run? Copy your tables into a
throwaway PostgreSQL in Docker and run Straum on the copy. The
database they came from serves the dump and nothing else. You need
Docker, [uv](https://docs.astral.sh/uv/) and `pg_dump`.

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

```sh
uv run https://raw.githubusercontent.com/janbjorge/straum/main/bench/try_your_data.py \
  usage.sql data.dump --schema schema.sql
```

From a clone of the repository, `just try usage.sql data.dump --schema
schema.sql` runs the same.
[`try_your_data.py`](https://github.com/janbjorge/straum/blob/main/bench/try_your_data.py)
starts PostgreSQL with `wal_level = logical`, restores the schema,
then the data with foreign keys and triggers off, and runs `straum
plan`'s SQL. Then it starts `straum run` from the image, waits until
Straum has loaded, and opens [`straum shell`](cli.md#straum-shell):

```text
try: straum ready in 0 s

Ask:        curl --json '{"query": "SELECT <keys>, <measures> FROM <view>
              WHERE bucket >= $1 AND bucket < $2 GROUP BY <keys>",
              "parameters": ["<from>", "<to>"]}' http://127.0.0.1:8080/ask
PostgreSQL: docker exec -it straum-try-pg psql -U postgres

Opening straum shell. Leaving it removes both containers.
```

`\d` lists the views, `\rows usage` prints a view's rows. Both
containers listen on `127.0.0.1` only.

A statement the restore can't run, such as an extension the image
lacks, is printed and skipped. A table the spec reads but the restore
missed stops the run at `straum plan`, which names it
([Checking a spec](spec.md#checking-a-spec)).

| Flag | Default | |
| --- | --- | --- |
| `--schema FILE` | none | `pg_dump --schema-only` output, restored first |
| `--postgres N` | `18` | PostgreSQL major; the dump's or newer |
| `--image IMAGE` | `ghcr.io/janbjorge/straum` | Straum image; a local tag works |
| `--port N` | `8080` | local port Straum answers on |
| `--jobs N` | `4` | `pg_restore` jobs for a directory or custom dump |

The dump may be a directory, a custom or tar archive, or a plain `.sql`
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
docker exec straum-try-pg psql -U postgres -c "
  SELECT a.region, count(*) AS events, sum(e.amount) AS amount
    FROM events e LEFT JOIN accounts a ON a.id = e.account_id
   WHERE e.at >= '2026-11-01T00:00:00Z' AND e.at < '2026-12-01T00:00:00Z'
   GROUP BY a.region"
```

Write to the copy through the same `psql` and ask again; the answer
moves. [Asks](asks.md) explains the query and the answer.

## On the database itself

Once the answers hold, the same steps run against the database you
dumped:

1. `straum plan FILE` prints the setup SQL, or what is wrong with the
   spec. A table owner runs the SQL once. PostgreSQL 17 and 18 also
   need `wal_level = logical`, which takes a restart, or
   `--wal physical` ([Setting up PostgreSQL](postgresql.md)).
2. `straum run FILE` with `STRAUM_DSN` set, then ask.

Start with few keys. Memory follows cells, one per distinct
combination of keys and bucket ([Cells](how-it-works.md#cells)).

Next: [How Straum works](how-it-works.md), what Straum keeps and how it
stays current.
