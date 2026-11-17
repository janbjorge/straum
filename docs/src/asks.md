# Asks and endpoints

An ask is a JSON object sent to `POST /ask`: a query and its parameters.
The examples ask the `usage` view of [Spec](spec.md#synopsis).
This one asks for purchases in two regions per hour, from 9:00 to
12:00 UTC:

```sh
curl --json '{
  "query": "SELECT bucket, region, events, amount FROM usage WHERE kind = $1 AND region IN ($2, $3) AND bucket >= $4 AND bucket < $5 GROUP BY bucket, region",
  "parameters": ["purchase", "eu", "us", "2026-11-13T09:00:00Z", "2026-11-13T12:00:00Z"]
}' http://localhost:8080/ask
```

| Field | Required | Value |
| --- | --- | --- |
| `query` | yes | One `SELECT` over a view, in the subset below. |
| `parameters` | no | The values for `$1`, `$2` and on, in order. |

## The query

```text
SELECT [key, ...] measure, ...
FROM view
[WHERE condition [AND ...]]
[GROUP BY key, ...]
[HAVING comparison [AND ...]]

condition   key = $n
            key IN ($n, $n, ...)
            bucket >= $n
            bucket < $n
comparison  measure { = | <> | < | <= | > | >= } $n
```

The notation is that of the [spec's synopsis](spec.md#synopsis), and
this is the whole language: anything it does not show is an error that
names the construct and says what to write. The shortest ask reads
every bucket Straum holds:

```sql
SELECT events, amount FROM usage
```

- A key or a measure goes by the name the view gives it with `AS`:
  `region`, `kind`, `account_id`, `amount`. `a.region` is not a name an
  ask can use; neither is `a`, the join, nor a table name.
  `bucket` is the view's time bucket, and a key like any other in
  `SELECT` and `GROUP BY`.
- Each name is selected at most once, since the names become the
  fields of each answer row. `GROUP BY` names exactly the selected
  keys.
- `bucket >= $n` and `bucket < $n` bound the range, each at most once.
  Leave one out and that end is open: the ask reads from the first
  bucket held, or to the end of the last. With no `WHERE` it reads
  every bucket held; `WHERE true` is refused. The bounds and the
  conditions come in any order. No condition is put in parentheses.

## HAVING

`WHERE` picks the rows by their keys; `HAVING` picks the groups by their
measures, after Straum has added them up. This asks for the regions
with more than 40 purchases in an hour, and their amounts:

```sql
SELECT bucket, region, events, amount FROM usage WHERE kind = $1 AND bucket >= $2 AND bucket < $3 GROUP BY bucket, region HAVING events > $4
```

- A comparison names a measure the ask selects, so the answer shows
  every value a group was kept for. A key goes in `WHERE`, a measure in
  `HAVING`; each refuses the other and says where it goes.
- Not equal is `<>`; `!=` is refused. Comparisons join by `AND`.
- A measure that is null meets no comparison, `<>` included, as in
  PostgreSQL, so its group is left out.
- A count, sum, min, max, percentile or average compares with a number
  parameter, exactly: `avg_amount > 44.27` is true of 44.273058, and
  `amount = 1875.7` of 1875.70. Up to 38 digits may follow the point.
- `bool_and` and `bool_or` compare with a boolean, by `=` or `<>` only.
- An ask without keys answers its one row only if it meets every
  comparison; otherwise none.

## Parameters

Every value is a parameter: a value written into the query, such as
`region = 'eu'`, is an error, so a value can never change what the
query says. Parameters are numbers, strings or booleans, `$1` first.
Each must be used, and one can be used more than once. A number with a
fraction or exponent is read exactly as written, never as a float:
`44.27` is 4427 hundredths.

A parameter must have its column's type, with no conversion: an integer
for an integer column or a `numeric(p, 0)`, a string for text, a boolean
for a boolean or a null-ness key. `account_id = $1` with `"7"` is an
error, not an answer with no rows. A `numeric` key with a scale above 0
can't be filtered on: at scale 2, `1` could mean 1 or 0.01. A
[HAVING](#having) comparison takes a number of any scale.

The `bucket` bounds are RFC 3339 times, such as `"2026-11-13T00:00:00-05:00"`,
or Unix seconds. Each bound written must fall on an edge of the view's
`date_bin` buckets: Straum never answers for part of a bucket.

## The answer

```json
{
  "as_of": "2026-11-14T09:15:02.530117842Z",
  "rows": [
    {"bucket": "2026-11-13T09:00:00Z", "region": "eu", "events": 38, "amount": 1875.70},
    {"bucket": "2026-11-13T09:00:00Z", "region": "us", "events": 61, "amount": 2980.00},
    {"bucket": "2026-11-13T10:00:00Z", "region": "eu", "events": 44, "amount": 1530.20},
    {"bucket": "2026-11-13T10:00:00Z", "region": "us", "events": 57, "amount": 2210.45},
    {"bucket": "2026-11-13T11:00:00Z", "region": "eu", "events": 29, "amount": 980.00},
    {"bucket": "2026-11-13T11:00:00Z", "region": "us", "events": 70, "amount": 3125.60}
  ]
}
```

- `as_of`: every commit made before this time is in the answer. It is
  Straum's wall time when it last confirmed that it had applied all WAL
  PostgreSQL had written, so it keeps moving on a quiet database. A
  transaction is applied whole, so an answer never sees half of one.
  An answer can predate a client's commit ([Seeing your own
  write](#seeing-your-own-write)).
- `rows`: one object per group, keyed by the names in `SELECT`, sorted by
  key, nulls first. An ask without keys answers one row even when
  nothing matches, as SQL does without `GROUP BY`: counts are 0, the
  rest null.

`timestamptz` buckets are their start in UTC. `timestamp` buckets are
wall clock readings formatted with `Z`. PostgreSQL keeps times past
the year 9999, which RFC 3339 can't write; an open end reads them, and
such a bucket prints as `Unix second` and its number. Measures print as the
[measures table](spec.md#measures) says: `1875.70` for a
`numeric(12, 2)` sum, and `44.273058` for an `avg_amount` of 18240.50
over 412 events, at four places past the scale. A
row whose dim row does not exist has null dim columns, as a `LEFT OUTER
JOIN` gives, until the dim row arrives.

Two headers help debugging. `Straum-LSN` names the last commit the
answer reflects. `Server-Timing` says where the time went in Straum, in
milliseconds: `parse`, `answer`, `json` and `total`; what a client
measures beyond `total` is the network and the client itself.

### As a table

An ask sent with `Accept: text/plain` is answered in plain text, for
people: the same rows as a table, then the row count and `as_of`, as
[`straum ask`](cli.md#straum-ask) prints it. Its errors are plain
text too, a line each, under the same statuses. The layout is for
reading and may change; a program reads the JSON.

### Seeing your own write

`Straum-LSN` is written as PostgreSQL writes a `pg_lsn`, such as
`0/1A2B3C8`. A client that needs its own write in the answer can read
`pg_current_wal_lsn()` on the same connection right after its commit,
then ask until the header reaches it:

```sql
COMMIT;
SELECT pg_current_wal_lsn();  -- 0/1A2B3C8
```

```sql
SELECT '0/1A2B3D0'::pg_lsn >= '0/1A2B3C8'::pg_lsn;  -- Straum-LSN >= mine
```

Compare as `pg_lsn` or as two hexadecimal numbers, never as strings.
Once the header is at or past the LSN read, the answer contains the
write. Under `synchronous_commit = off` the commit may not be written
yet, so read `pg_current_wal_insert_lsn()` instead.

The test can only say yes. `pg_current_wal_lsn()` counts all WAL,
including tables Straum doesn't follow, while the header moves only
when Straum applies a commit. On a quiet database the header can stay
below the LSN read even though the write is in, so give up after a
timeout or fall back to `as_of`.

## Errors

| Status | When |
| --- | --- |
| 200 | The answer. |
| 400 | The ask is wrong; `errors` lists every problem. |
| 405 | Not a `POST`. |
| 413 | The ask is over 1 MiB. |
| 500 | Straum failed while answering; its log says why. |
| 503 | `{"errors": ["not ready"]}`: Straum is [loading](deploy.md#states). Retry after `/readyz` returns 200. |

```json
{"errors": ["unknown measure amout; did you mean amount?"]}
{"errors": ["zip is not a key of the view; the view's keys are bucket, kind, account_id, region"]}
{"errors": ["region is a key; add it to GROUP BY"]}
{"errors": ["line 1: bucket >= given twice"]}
{"errors": ["unknown field \"tz\""]}
{"errors": ["line 1: value, use a parameter such as $1, `'eu'` is not in Straum SQL, the PostgreSQL subset made only for Straum"]}
{"errors": ["bound 1794558600 is not on a 3600s bucket edge"]}
{"errors": ["account_id is an integer, so it can't be the text \"7\""]}
```

## Endpoints

| Endpoint | Answers |
| --- | --- |
| `POST /ask` | An ask, as JSON; as a table with `Accept: text/plain`. |
| `GET /readyz` | 200 `ready`, or 503 `not ready: ` and why ([States](deploy.md#states)). |
| `GET /livez` | 200 while the process serves. Lag never fails it. |
| `GET /status` | What [`straum status`](cli.md#straum-status) prints, in plain text. With `Accept: application/json`, the same facts as one JSON object: `ready`, `why_not`, `applied`, `committed`, `as_of`, `lag_ms`, `max_lag_ms`, `wal_written`, `wal_retained_bytes`, `max_retained_wal_bytes`, `starts`, `failures` and `last_failure` with its `at` and `error`. What is unknown until every table is loaded is null. |
| `GET /views` | What [`straum views`](cli.md#straum-views) prints, in plain text; 503 `not ready: ` and why until every table is loaded. With `Accept: application/json`, each view's names and range of buckets as JSON, which `straum shell` completes from. |
| `GET /views/NAME/spec` | The view's `CREATE VIEW` statement as the spec wrote it, with the comments right above it, in plain text; what `\d NAME` in [`straum shell`](cli.md#straum-shell) prints. It answers while the tables load. 404 for a name no view has. |
| `GET /views/NAME` | The rows PostgreSQL would give for the view, as a table in plain text: `count` of them from row `start`, as in `?start=-20&count=10`. `start` counts from 0, a negative one back from the end, and is 0 when not given; `count` is 0 to 1000, 1000 when not given. A start past either end is held at it. The last line says how many rows there are and, past the first, from which. 400 for any other parameter or value, 404 for a name no view has. |
