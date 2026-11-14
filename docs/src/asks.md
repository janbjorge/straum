# Asking

An ask is a JSON object sent to `POST /ask`: a query and its parameters.
The examples ask the `usage` view of [Writing a spec](spec.md#synopsis).
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
WHERE [condition AND ...] bucket >= $n AND bucket < $n
[GROUP BY key, ...]

condition  key = $n
           key IN ($n, $n, ...)
```

The notation is that of the [spec's synopsis](spec.md#synopsis), and
this is the whole language: anything it does not show is an error that
names the construct and says what to write. The shortest ask:

```sql
SELECT events, amount FROM usage WHERE bucket >= $1 AND bucket < $2
```

- A key or a measure goes by the name the view gives it with `AS`:
  `region`, `kind`, `account_id`, `amount`. `a.region` is not a name an
  ask can use; neither is `a`, the join, nor a table name.
  `bucket` is the view's time bucket, and a key like any other in
  `SELECT` and `GROUP BY`.
- Each name is selected at most once, since the names become the
  fields of each answer row. `GROUP BY` names exactly the selected
  keys.
- `bucket >= $n` and `bucket < $n` bound the range, once each, and both
  are written: an ask says what it reads. They and the conditions come
  in any order. No condition is put in parentheses.

## Parameters

Every value is a parameter: a value written into the query, such as
`region = 'eu'`, is an error, so a value can never change what the
query says. Parameters are integers, strings or booleans, `$1` first.
Each must be used, and one can be used more than once.

A parameter must have its column's type, with no conversion: an integer
for an integer column or a `numeric(p, 0)`, a string for text, a boolean
for a boolean or a null-ness key. `account_id = $1` with `"7"` is an
error, not an answer with no rows. A `numeric` with a scale above 0
can't be filtered on: at scale 2, `1` could mean 1 or 0.01.

The `bucket` bounds are RFC 3339 times, such as `"2026-11-13T00:00:00-05:00"`,
or Unix seconds. Both must fall on an edge of the view's `date_bin`
buckets: Straum never answers for part of a bucket.

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
wall clock readings formatted with `Z`. Measures print as the
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
[`straum ask`](running.md#straum-ask) prints it. Its errors are plain
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
| 503 | `{"errors": ["not ready"]}`: Straum is [loading](running.md#states). Retry after `/readyz` returns 200. |

```json
{"errors": ["unknown measure x", "zip is not a key of the view"]}
{"errors": ["line 1: an ask writes both bounds; bucket < $n is missing"]}
{"errors": ["unknown field \"tz\""]}
{"errors": ["line 1: value, use a parameter such as $1, `'eu'` is not in Straum SQL, the PostgreSQL subset made only for Straum"]}
{"errors": ["bound 1794558600 is not on a 3600s bucket edge"]}
{"errors": ["account_id is an integer, so it can't be the text \"7\""]}
```
