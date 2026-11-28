# Asks and endpoints

An ask is JSON sent to `POST /ask`. The examples ask the `usage` view
of [Spec](spec.md#synopsis):

```sh
curl --json '{
  "query": "SELECT bucket, region, events, amount FROM usage WHERE kind = $1 AND region IN ($2, $3) AND bucket >= $4 AND bucket < $5 GROUP BY bucket, region",
  "parameters": ["purchase", "eu", "us", "2026-11-13T09:00:00Z", "2026-11-13T12:00:00Z"]
}' http://localhost:8080/ask
```

An ask without `Content-Type: application/json` is refused with 415,
so a page on another site can't make a browser run one ([Who can
ask](deploy.md#who-can-ask)).

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

key         name
            buckets(time, 'width')
condition   key = $n
            key IN ($n, $n, ...)
            time >= $n
            time < $n
comparison  measure { = | <> | < | <= | > | >= } $n
```

The notation is the [spec's synopsis](spec.md#synopsis); `time` is the
name the view gives its [time](spec.md#time), `bucket` in `usage`.
Anything the synopsis does not show is an error that names the
construct and says what to write. The shortest ask reads every bucket
Straum holds:

```sql
SELECT events, amount FROM usage
```

A key or a measure, the time included, goes by the name the view gives
it with `AS`: `region`, `kind`, `account_id`, `amount`, `bucket`.
`a.region`, `a` (the join) and table names are not names an ask can
use. The time is a key like any other in `SELECT` and `GROUP BY`.

`SELECT` lists the keys first, then one measure or more, each name at
most once, since the names become the fields of each answer row.
`GROUP BY` names exactly the selected keys. `SELECT` and `GROUP BY`
each list at most 256 names.

`WHERE` takes `=` and `IN` on keys; `IN` lists two parameters or more,
each once, and for one, write `=`: `kind IN ($1, $1)` is refused,
naming `$1`. Only the time takes `>=` and `<`, each at most
once, to bound the range; the start must be before the end. Leave one
out and that end is open: the ask reads from the first bucket held, or
to the end of the last. With no `WHERE` it reads every bucket held;
`WHERE true` is refused. Bounds and conditions come in any order. No
condition is put in parentheses, and none, in `WHERE` or `HAVING`, is
written twice: `kind = $1 AND kind = $1` is refused, naming
`kind = $1`.

Names are lowercase, `buckets` too; keywords such as `SELECT` and
`GROUP BY` take any case. An ask holds no comment and no `;` before or
after it.

An ask holds at most 512 operators (`=`, `>`, `AND`, `OR`, `IS`, `IN`,
`+`, `::`, `OPERATOR`, `UNION` and the like), as does each statement of
a spec: each is a level of the tree Straum reads it into.

## Rows a day or a week apart

`GROUP BY bucket` answers a row per bucket of the view's narrowest
width, an hour in `usage`. `buckets(bucket, '1 day')`, the view's own
`buckets` at one of its wider widths, answers a row per day, read from
the day buckets the view keeps: a month of days reads 31 buckets, not
744 hours:

```sql
SELECT buckets(bucket, '1 day'), region, events FROM usage WHERE bucket >= $1 AND bucket < $2 GROUP BY buckets(bucket, '1 day'), region
```

The width is one the view's `buckets` item lists after its first,
written as the spec writes widths: `'1 day'`, never `'24 hours'`. Any
other width is refused, naming the widths the view keeps: a width
built from narrower buckets would cost as much as asking by those. At
the narrowest width, write `bucket`. The width is part of the query,
not a parameter, since it sets the answer's shape, as the view's own
width does.

`SELECT` and `GROUP BY` write the same `buckets`. The answer names the
column by the time, `bucket`, and each row's value is the start of its
day or week. Each bound written falls on an edge of that width, so no
row covers part of a day. An open end reaches the buckets held, so the
first or last row may cover part of its day: nothing lies beyond it.

PostgreSQL's `date_bin` is refused, naming the `buckets` to write. Days
are UTC days and `7 days` runs Thursday to Thursday, since every width
is a grid from the epoch ([Time](spec.md#time)).

## Widths kept for a time

`usage` keeps every bucket. A view whose `buckets` item keeps a width
for a time ([Keeping a width for a
time](spec.md#keeping-a-width-for-a-time)) holds that width's buckets
only from its horizon on. An ask that would read them before it is
refused, naming the width and the time its buckets are kept from:

```text
1 hour buckets are kept only from 2024-03-01T00:00:00Z; bound the view's time from there, or read a wider width
```

An ask reads a width before its horizon when it groups by that width
over a range that starts before the horizon, or that is open at the
start, since an open start reads from the first bucket held. It also
does when a bound before the horizon falls on no edge of a wider width
still held there, since the ends of a range are read from the narrowest
buckets that fit them. The same range with its bounds on the edges of a
width still held answers, read from those wider buckets, as does an ask
with no time key and both ends open. Each width read too far back is
named once, the narrowest first. [`straum views`](cli.md#straum-views)
and `GET /views` show each width's horizon before an ask meets it.
`GET /views/NAME` and `\rows` start a view's rows at the horizon of
its width, so Straum does not refuse them.

## HAVING

```sql
SELECT bucket, region, events, amount FROM usage WHERE kind = $1 AND bucket >= $2 AND bucket < $3 GROUP BY bucket, region HAVING events > $4
```

A comparison names a measure the ask selects, so the answer shows every
value a group was kept for. A key goes in `WHERE`, a measure in
`HAVING`; each refuses the other and says where it goes. Comparisons
join by `AND`. Not equal is `<>`, SQL's standard spelling. PostgreSQL
reads `!=` as an alias of it, but Straum SQL keeps one spelling per
meaning, so `!=` is refused. PostgreSQL has no `==`, so `WHERE` and
`HAVING` refuse it.

A null measure meets no comparison, `<>` included, as in PostgreSQL,
so its group is left out. An ask without keys answers its one row
only if it meets every comparison; otherwise none.

`bool_and` and `bool_or` compare with a boolean, by `=` or `<>` only.
Every other measure, `DISTINCT` ones included, answers a number and
compares exactly with a number parameter: `avg_amount > 44.27` is true
of 44.273058, and `amount = 1875.7` of 1875.70. A variance or a
standard deviation compares before the answer rounds it:
`amount_sd > 0.7071` is true of 0.70710678..., which prints as 0.7071.

## Parameters

Every value is a parameter: a value written into the query, such as
`region = 'eu'`, is an error, so a value can never change what the
query says. Parameters are numbers, strings or booleans, `$1` first;
`null` is refused. A parameter is `$1`, never `$01`. Each must be
used, and one can be used more than once, as long as the text
parameters, counted at each use, come to at most 1 MiB, what one ask's
body holds.

A number with a fraction or exponent is read exactly as written, never
as a float: `44.27` is 4427 hundredths, and a whole one, `1.0`,
`10e-1` or `1e3`, is an integer. At most 38 digits may follow the
point, and the number's digits, read as one integer, must fit a signed
128-bit integer; a longer number is an error.

A parameter must have its column's type, with no conversion: an integer
for an integer column or a `numeric(p, 0)`, a string for text, a boolean
for a boolean or a null-ness key. `account_id = $1` with `"7"` is an
error, not an answer with no rows. A `date` or `uuid` key takes a
string spelled as PostgreSQL prints the value and as the answer shows
it: `"2024-01-15"`, `"0044-03-15 BC"` or `"infinity"` for a date,
lowercase hex in groups of 8-4-4-4-12 for a uuid. `"2024-1-15"` or an
uppercase uuid is an error that says how to write it. A `numeric` key
with a scale above 0 can't be filtered on: at scale 2, `1` could mean
1 or 0.01. A [HAVING](#having) comparison takes a number of any scale.

The time's bounds are RFC 3339 times, such as
`"2026-11-13T00:00:00-05:00"`, or Unix seconds. A time is written
`YYYY-MM-DDTHH:MM:SS`, then `Z` or an offset `+HH:MM`, with no fraction
and no leap second: `2026-11-13T09Z` and `2026-11-13T09:00:00.000Z` are
refused. A zero offset is `Z`, never `+00:00` or `-00:00`. If the
view's time is a `timestamp`, write each bound as the wall clock time
with `Z`, or in Unix seconds. PostgreSQL does not use the offset when
it reads text as a `timestamp`, so Straum refuses a bound with an
offset. Each bound must fall on an edge of the ask's rows: the view's
narrowest buckets, or the wider ones a
[`buckets` key](#rows-a-day-or-a-week-apart) names. Straum never
answers for part of a bucket, and the error names the edges on either
side.

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

`as_of` says every commit made before this time is in the answer. It
is Straum's wall time when it last knew it had applied all the
write-ahead log (WAL) PostgreSQL had written, so it keeps moving on a
quiet database. A transaction is applied whole, so an answer never
sees half of one. An answer can predate a client's commit ([Seeing
your own write](#seeing-your-own-write)).

`rows` holds one object per group, keyed by the names in `SELECT`,
sorted by key, nulls first. An ask without keys answers one row even
when nothing matches: counts are 0, the rest null.

`timestamptz` buckets are their start in UTC; `timestamp` buckets are
wall clock readings formatted with `Z`. PostgreSQL keeps times past
the year 9999, which RFC 3339 can't write; an open end reads them, and
such a bucket prints as `Unix second` and its number. A `date` or
`uuid` key is a string as PostgreSQL prints it: `"2024-01-15"`,
`"0044-03-15 BC"`, `"infinity"`,
`"a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"`. Under `LEFT OUTER JOIN`, a
row whose dim row does not exist has null dim columns until the dim
row arrives.

Measures print as the [measures table](spec.md#measures) says:
`1875.70` for a `numeric(12, 2)` sum, and `44.273058` for an
`avg_amount` of 18240.50 over 412 events. Each is a bare JSON number,
as exact as Straum keeps it; a client that reads numbers as doubles
rounds those past 2^53.

Two headers help debugging. `Straum-LSN` names the last commit the
answer reflects. `Server-Timing` says where the time went in Straum, in
milliseconds: `parse`, `answer`, `json` and `total`.

### As a table

An ask sent with `Accept: text/plain` is answered in plain text, as
[`straum ask`](cli.md#straum-ask) prints it: the rows as a table, then
the row count and `as_of`. Its errors are plain text too, a line each,
under the same statuses. Its `Server-Timing` has `text` where the
JSON's has `json`; [`straum shell`](cli.md#straum-shell) prints it
under the table. The layout may change; a program reads the JSON.

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
Once the header is at or past the log sequence number (LSN) read, the
answer contains the write. Under `synchronous_commit = off` the commit
may not be written yet, so read `pg_current_wal_insert_lsn()` instead.

A header below the LSN read does not show that the write is missing.
`pg_current_wal_lsn()` counts all WAL, including tables Straum doesn't
follow, while the header moves only when Straum applies a commit. On a
quiet database the header can stay below the LSN read even though the
write is in, so give up after a timeout or fall back to `as_of`.

## Errors

| Status | When |
| --- | --- |
| 200 | The answer. |
| 400 | The ask is wrong; `errors` lists the problems, the first 50 and then how many more. An error quotes at most the first 60 characters of a name, a value or a key of the ask, then `...`. An answer of more rows than [`--max-answer-rows`](cli.md#straum-run) allows, 100,000 by default, is refused too, since it is written whole in memory: narrow the ask. So is an answer larger than all the bytes that unread answers may hold, or of 4 GiB or more: `the answer has at least N bytes, past the M that one answer may hold` ([Connections](deploy.md#connections)). |
| 405 | Not a `POST`. |
| 408 | The ask's body did not come whole within 10 s of its head. |
| 413 | The ask is over 1 MiB. |
| 415 | `{"errors": ["an ask is JSON, sent with Content-Type: application/json"]}`: the request has no `Content-Type`, or another media type than `application/json`. Its parameters, such as `; charset=utf-8`, and its case don't matter. |
| 421 | The `Host` is none `--allow-hosts` lists, or Straum listens on loopback and the `Host` names another machine or port, on every endpoint ([Who can ask](deploy.md#who-can-ask)). |
| 503 | `{"errors": ["not ready"]}`: Straum is [loading](deploy.md#states). Retry after `/readyz` returns 200. |
| 503 | `{"errors": ["busy: every core is answering an ask; ask again shortly"]}`: Straum answers as many asks at once as it has cores, and `GET /views` and `GET /views/NAME` count as asks. Retry. |
| 503 | `{"errors": ["busy: unread answers hold the memory that this answer of N bytes needs; ask again shortly, or narrow the ask"]}`: the answers that Straum has not sent yet hold all the bytes that they may hold ([Connections](deploy.md#connections)). `GET /views/NAME` counts too. Retry. |
| 503 | `{"errors": ["busy: asks and unread answers hold the memory that this ask of N bytes needs; ask again shortly"]}`: an ask's body holds its bytes of the same budget while Straum reads it ([Connections](deploy.md#connections)). Retry. |

No ask is answered 500: a panic in Straum ends the process rather than
answering from a store it may have left half written, and the client
sees the connection close.

```json
{"errors": ["unknown measure amout; did you mean amount?"]}
{"errors": ["zip is not a key of the view; the view's keys are bucket, kind, account_id, has_device, region"]}
{"errors": ["region is a key; add it to GROUP BY"]}
{"errors": ["line 1: bucket >= given twice"]}
{"errors": ["unknown field \"tz\""]}
{"errors": ["line 1: value, use a parameter such as $1, `'eu'` is not in Straum SQL, the PostgreSQL subset made only for Straum"]}
{"errors": ["bound 1794558600 is not on a 1 hour bucket edge; the nearest are 1794556800 and 1794560400"]}
{"errors": ["empty range [1794560400, 1794556800)"]}
{"errors": ["the view's time is a timestamp, so PostgreSQL does not use the offset of a bound; write the wall clock time of the bound with Z"]}
{"errors": ["bucket width '2 days' is not kept by the view; it keeps 1 hour, 1 day, 7 days"]}
{"errors": ["1 hour buckets are kept only from 2024-03-01T00:00:00Z; bound the view's time from there, or read a wider width"]}
{"errors": ["account_id is an integer, so it can't be the text \"7\""]}
{"errors": ["the answer has 105120 rows, past the 100000 an ask may get; narrow the ask: a shorter range, fewer keys or a filter"]}
```

## Endpoints

| Endpoint | Answers |
| --- | --- |
| `POST /ask` | An ask sent with `Content-Type: application/json`, as JSON; as a table with `Accept: text/plain`. |
| `GET /readyz` | 200 `ready`, or 503 `not ready: ` and why ([States](deploy.md#states)). |
| `GET /livez` | 200 `live` while the process serves. Lag never fails it. |
| `GET /openapi.json` | The OpenAPI 3.1 document of these endpoints. Its JSON schemas are the contract; plain text is for people and may change. |
| `GET /status` | What [`straum status`](cli.md#straum-status) prints, in plain text. With `Accept: application/json`, the same facts as one JSON object: `ready`, `why_not`, `applied`, `committed`, `as_of`, `lag_ms`, `max_lag_ms`, `caught_up`, `commits_per_second`, `changes_per_second`, `wal_written`, `wal_retained_bytes`, `max_retained_wal_bytes`, `resident_bytes`, `max_memory_bytes` and `version`. What is unknown until every table is loaded is null, `resident_bytes` is null where the system gives no figure, as on macOS, and `max_memory_bytes` without `--max-memory`. In both forms, the `Straum-Ready` header is `true` or `false`, as `/readyz` says. |
| `GET /views` | What [`straum views`](cli.md#straum-views) prints, in plain text; 503 `not ready: ` and why until every table is loaded. With `Accept: application/json`, each view's names, the name of its time, range of buckets and each width kept for a time with its horizon as JSON, which `straum shell` completes from. |
| `GET /views/NAME/spec` | The view's `CREATE VIEW` statement as the spec wrote it, with the comments right above it, in plain text; what `\d NAME` in [`straum shell`](cli.md#straum-shell) prints. It answers while the tables load. 404 for a name no view has. |
| `GET /views/NAME` | The rows PostgreSQL would give for the view, as a table in plain text: `count` of them from row `start`, as in `?start=-20&count=10`. `start` counts from 0, a negative one back from the end, and is 0 when not given; `count` is 0 to 1000, 1000 when not given. A start past either end is held at it. The last line says how many rows there are and, past the first, from which. Its headers are those of [an answer as a table](#as-a-table). 400 for any other parameter or value, 404 for a name no view has, 503 while Straum is [loading](deploy.md#states). |

Every reply carries `Cache-Control: no-store`. Each `GET` endpoint
also answers `HEAD`, with the status and headers of `GET` and no body.
A path not listed here is 404, and a listed path asked with another
method is 405, its `Allow` header naming the methods to use, as in
`Allow: GET, HEAD`. The port of `--health-listen` serves `GET /livez`,
`GET /readyz` and `GET /status` alone, and gives 404 to every other
path ([Connections](deploy.md#connections)).
