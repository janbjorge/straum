# Try it on your data

Does Straum answer the queries you run? Write a spec, run one command,
and Straum's shell opens on a copy of your tables in Docker. Straum
only reads your database.

You need Docker with Compose, and a role that can `SELECT` the tables
the spec reads.

## 1. Write a spec

In an empty directory, save this as `usage.sql`:

```sql
CREATE VIEW usage AS
SELECT buckets(e.at, '1 hour', '1 day', '7 days') AS bucket,
       e.kind AS kind,
       e.account_id AS account_id,
       a.region AS region,
       (e.device_id IS NOT NULL) AS has_device,
       count(*) AS events,
       sum(e.amount) AS amount,
       avg(e.amount) AS avg_amount,
       max(e.amount) AS top_amount,
       percentile_disc(0.5) WITHIN GROUP (ORDER BY e.amount) AS median_amount,
       percentile_disc(0.99) WITHIN GROUP (ORDER BY e.amount) AS p99_amount,
       count(*) FILTER (WHERE e.kind = 'purchase') AS purchases,
       count(*) FILTER (WHERE e.device_id IS NOT NULL) AS from_devices
FROM public.events AS e
LEFT OUTER JOIN public.accounts AS a ON a.id = e.account_id
GROUP BY 1, 2, 3, 4, 5;
```

Change the tables, keys and measures to match the `GROUP BY` your app
runs most; [Spec](spec.md) has every rule. The file must be named
`usage.sql`, whatever its views are called.

## 2. Run

In the same directory:

```sh
curl -fsSLO https://raw.githubusercontent.com/janbjorge/straum/main/docs/compose/try.yaml
export DATABASE_URL='postgres://reader:secret@db.example.com:5432/app?sslmode=require'
docker compose -f try.yaml run --rm shell
```

`DATABASE_URL` is any connection string `psql` takes. Your `PG*`
variables don't reach the containers. For a database on your own
machine, write `host.docker.internal`, not `localhost`.

The command copies the columns the spec reads, starts Straum on the
copy and opens the shell once it has loaded:

```text
Straum at http://straum:8080. Tab completes; \? lists the commands, \q leaves.
straum>
```

`\d` lists the views and `\rows usage` prints a view's rows.

## 3. Ask and compare

Ask in the shell, or with `curl`:

```sh
curl --json '{
  "query": "SELECT region, events, amount FROM usage WHERE bucket >= $1 AND bucket < $2 GROUP BY region",
  "parameters": ["2026-11-01T00:00:00Z", "2026-12-01T00:00:00Z"]
}' http://127.0.0.1:8080/ask
```

Then ask the copy in SQL, and compare:

```sh
docker compose -p straum-try exec postgres psql -U postgres -c "
  SELECT a.region, count(*) AS events, sum(e.amount) AS amount
    FROM events e LEFT JOIN accounts a ON a.id = e.account_id
   WHERE e.at >= '2026-11-01T00:00:00Z' AND e.at < '2026-12-01T00:00:00Z'
   GROUP BY a.region"
```

Write to the copy through that `psql`, ask again, and the answer
follows the write. [Asks](asks.md) explains the query and the answer.

## 4. Stop

`\q` leaves the shell. Straum keeps running on `127.0.0.1:8080`;
`docker compose -p straum-try exec straum /straum shell` opens it
again. This removes everything:

```sh
docker compose -p straum-try down --volumes
```

To try a changed spec, run that, then step 2 again.

## When it fails

The command stops at the first step that fails. The export's log names
every column it can't copy and why
([`straum plan --copy`](cli.md#straum-plan) lists the reasons):

```sh
docker compose -p straum-try logs export
```

The copy has the rows of the columns the spec reads, without indexes,
keys or defaults. The file in the repository is
`docs/compose/try.yaml`, with a comment on each step.

## Copy once, from a clone

Step 2 reads your database each time. From a clone of the repository,
make the copy once and load it as often as you like. You need the
[`straum` binary](deploy.md#get-straum), `psql`, `just`,
[uv](https://docs.astral.sh/uv/) and Docker with Compose:

```sh
straum plan usage.sql --copy copy
psql "$DATABASE_URL" -f copy/export.sql
just try usage.sql --data copy
```

[`straum plan --copy`](cli.md#straum-plan) writes the scripts into
`copy/`, and `just try` loads `copy/` into a new PostgreSQL, starts
Straum and opens the shell. It prints the port Straum answers on,
which Docker picks unless `--port` names one. Leaving the shell
removes both containers.

`--wal physical` has Straum read the [physical write-ahead log
(WAL)](postgresql.md#physical-wal) in place of logical decoding. `just
try --help` lists the other flags.

## On the database itself

Once the answers hold:

1. `straum plan usage.sql` prints the setup SQL. A table owner runs it
   once ([Setting up PostgreSQL](postgresql.md)).
2. `straum run usage.sql`, with `STRAUM_DSN` or libpq's `PGHOST` and
   the rest set, then ask.

Start with few keys: memory grows with each distinct combination of
keys and bucket ([Cells](how-it-works.md#cells)).

Next: [How Straum works](how-it-works.md), what Straum keeps and how it
stays current.
