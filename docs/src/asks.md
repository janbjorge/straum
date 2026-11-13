# Asking

An ask is a JSON object sent to `POST /ask`: a query and its parameters.
The examples use the [quick start](quick-start.md) after step 8.
This one asks for card trips from Brooklyn and Queens per hour, from
8:00 to 11:00 in New York:

```sh
curl --json '{
  "query": "SELECT bucket, borough, trips, fare FROM trips WHERE payment_type = $1 AND borough IN ($2, $3) AND bucket >= $4 AND bucket < $5 GROUP BY bucket, borough",
  "parameters": [1, "Brooklyn", "Queens", "2024-01-17T08:00:00-05:00", "2024-01-17T11:00:00-05:00"]
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
SELECT trips, fare FROM trips WHERE bucket >= $1 AND bucket < $2
```

- A key or a measure goes by the name the view gives it with `AS`:
  `borough`, `payment_type`, `pu_location_id`, `fare`. `pickup.borough`
  is not a name an ask can use; neither is `pickup`, the join.
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
`borough = 'Queens'`, is an error, so a value can never change what the
query says. Parameters are integers, strings or booleans, `$1` first.
Each must be used, and one can be used more than once.

A parameter must have its column's type, with no conversion: an integer
for an integer column or a `numeric(p, 0)`, a string for text, a boolean
for a boolean or a null-ness key. `payment_type = $1` with `"1"` is an
error, not an answer with no rows. A `numeric` with a scale above 0
can't be filtered on: at scale 2, `1` could mean 1 or 0.01.

The `bucket` bounds are RFC 3339 times, such as `"2024-01-17T00:00:00-05:00"`,
or Unix seconds. Both must fall on an edge of the view's `date_bin`
buckets: Straum never answers for part of a bucket.

## The answer

```json
{
  "as_of": "2026-11-13T07:58:16.834219708Z",
  "rows": [
    {"bucket": "2024-01-17T13:00:00Z", "borough": "Brooklyn", "trips": 63, "fare": 1875.70},
    {"bucket": "2024-01-17T13:00:00Z", "borough": "Queens", "trips": 41, "fare": 1429.90},
    {"bucket": "2024-01-17T14:00:00Z", "borough": "Brooklyn", "trips": 51, "fare": 1728.30},
    {"bucket": "2024-01-17T14:00:00Z", "borough": "Queens", "trips": 62, "fare": 2589.80},
    {"bucket": "2024-01-17T15:00:00Z", "borough": "Brooklyn", "trips": 38, "fare": 1177.90},
    {"bucket": "2024-01-17T15:00:00Z", "borough": "Queens", "trips": 75, "fare": 3118.70}
  ]
}
```

- `as_of`: every commit made before this time is in the answer. It is
  Straum's wall time when it last confirmed that it had applied all WAL
  PostgreSQL had written, so it keeps moving on a quiet database. A
  transaction is applied whole, so an answer never sees half of one.
  An answer can predate a client's commit. Retry until it contains the
  change if the client needs to see its own write.
- `rows`: one object per group, keyed by the names in `SELECT`, sorted by
  key, nulls first. An ask without keys answers one row even when
  nothing matches, as SQL does without `GROUP BY`: counts are 0, the
  rest null.

`timestamptz` buckets are their start in UTC. `timestamp` buckets are
wall clock readings formatted with `Z`. Measures print as the
[measures table](spec.md#measures) says: `1875.70` for a
`numeric(12, 2)` sum, and `29.854221` for the quick start's Brooklyn
`avg_fare`, 27585.30 over 924 trips, at four places past the scale. A
row whose dim row does not exist has null dim columns, as a `LEFT OUTER
JOIN` gives, until the dim row arrives.

Two headers help debugging. `Straum-LSN` names the last commit the
answer reflects. `Server-Timing` says where the time went in Straum, in
milliseconds: `parse`, `answer`, `json` and `total`; what a client
measures beyond `total` is the network and the client itself.

### As a table

An ask sent with `Accept: text/plain` is answered in plain text, for
people: the same rows as a table, then the row count and `as_of`.
[`straum ask`](running.md#straum-ask) sends one from the command line.

```text
bucket               | borough  | trips | fare
---------------------+----------+-------+--------
2024-01-17T13:00:00Z | Brooklyn |    63 | 1875.70
2024-01-17T13:00:00Z | Queens   |    41 | 1429.90
2024-01-17T14:00:00Z | Brooklyn |    51 | 1728.30
2024-01-17T14:00:00Z | Queens   |    62 | 2589.80
2024-01-17T15:00:00Z | Brooklyn |    38 | 1177.90
2024-01-17T15:00:00Z | Queens   |    75 | 3118.70
6 rows, as of 2026-11-13T07:58:12.818010833Z
```

Its errors are plain text too, a line each, under the same statuses,
and `Server-Timing` says `text` where it said `json`. The layout is
for reading and may change; a program reads the JSON.

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
{"errors": ["line 1: value, use a parameter such as $1, `'Queens'` is not in Straum SQL, the PostgreSQL subset made only for Straum"]}
{"errors": ["bound 1705469400 is not on a 3600s bucket edge"]}
{"errors": ["payment_type is an integer, so it can't be the text \"1\""]}
```
