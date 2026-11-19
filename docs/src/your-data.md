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

The file starts PostgreSQL 18 with logical decoding on. [`straum plan
--copy`](cli.md#straum-plan) writes two psql scripts. `psql` runs
`export.sql` on `DATABASE_URL`: in one read-only snapshot it reads the
columns the spec reads, and the enums they use. Then it runs `load.sql`
on the new PostgreSQL. `straum plan` writes the setup SQL, and `psql`
runs it. Straum starts once that has exited, and
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

The copy has the rows, with only the columns the spec reads, and not
the indexes, keys or defaults. The role needs `SELECT` on those
columns and nothing on the sequences. Before it writes anything, the
export names every column it can't copy: a composite column, a type an
extension provides, or a table or column the role can't see. A row
security policy that would hide rows stops it too. The log names them:
`docker compose -p straum-try logs export`.

From a clone, `just try` loads a copy made with [`straum plan
--copy`](cli.md#straum-plan) instead, and can try the [physical
stream](postgresql.md#physical-wal). psql makes the copy, so any
sign-in libpq takes works, and a read-only role is enough:

```sh
straum plan usage.sql --copy copy
psql "$DATABASE_URL" -f copy/export.sql
just try usage.sql --data copy
```

[`try_your_data.py`](https://github.com/janbjorge/straum/blob/main/bench/try_your_data.py)
runs the copy's `load.sql` in a throwaway PostgreSQL. Each table prints
as it starts, with its rows once loaded. The first error stops the load
and the run, and names the line. Then it applies `straum plan`, starts
Straum and opens the shell. One copy can serve several specs: give
`straum plan --copy` all of them. A run removes what a killed run of
its stack left. Leaving the shell removes both containers.

| Flag | Default | |
| --- | --- | --- |
| `SPEC` or `--spec FILE` | required | the spec |
| `--data DIR` | required | the directory `straum plan --copy` wrote, after its `export.sql` ran |
| `--wal logical\|physical` | `logical` | the stream Straum reads |
| `--postgres N` | `18` | PostgreSQL major |
| `--image IMAGE` | `ghcr.io/janbjorge/straum` | Straum image; a local tag works |
| `--stack NAME` | `straum-try-` and the spec's name | Compose project to run in |
| `--port N` | a free one | local port Straum answers on |

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
