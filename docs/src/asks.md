# Asking

An ask is a JSON object sent to `POST /ask`. It names a fact, the measures
to read, what to group and filter by, and a time range. The examples use
the spec from the [quick start](quick-start.md).

```sh
curl --json '{
  "fact": "orders",
  "measures": ["orders", "revenue"],
  "by": ["customer.country"],
  "time": "bucket",
  "filter": {"status": "paid", "customer.country": ["NO", "SE"]},
  "from": "2026-01-01T00:00:00Z",
  "to": "2026-01-02T00:00:00Z"
}' http://localhost:8080/ask
```

## Fields

| Field | Required | Value |
| --- | --- | --- |
| `fact` | yes | The view's name. |
| `measures` | yes | Measure names, at least one. |
| `by` | no | Keys to group by, in order. |
| `time` | no | `"bucket"`, to group by the time bucket. |
| `filter` | no | An object of key to value, or key to a list of values. |
| `from` | yes | Where the range starts, included. |
| `to` | yes | Where the range ends, left out. |

A key in `by` or `filter` is one of:

- a by column: `"status"`;
- a ref, which reads the fact column it joins on: `"customer"`;
- an attribute of a ref: `"customer.country"`.

The time bucket is a field of its own, so it can't clash with a column
named `bucket`, and it groups before the `by` keys.

`filter` values are integers, strings or booleans. A list matches any of
its values; an empty list is an error, since it matches nothing. Every
condition must hold.

`from` and `to` are RFC 3339 times, such as `"2026-01-01T00:00:00Z"` or
`"2026-01-01T01:00:00+01:00"`, or Unix seconds. Both are whole seconds and
must fall on an edge of the view's `date_bin` buckets, so every bucket is
in the range or out of it. Straum never answers for part of a bucket.

## The answer

```json
{
  "lsn": "0/17B2958",
  "commit_time": "2026-11-09T17:12:57.346371Z",
  "columns": ["bucket", "customer.country", "orders", "revenue"],
  "rows": [
    ["2026-01-01T09:00:00Z", "NO", 1, 120.00],
    ["2026-01-01T09:00:00Z", "SE", 1, 80.50],
    ["2026-01-01T11:00:00Z", "SE", 1, 300.00]
  ]
}
```

- `lsn`: the answer reflects every commit up to this one and none after.
- `commit_time`: when that commit committed, by the server's clock. Null
  until Straum applies its first commit after a load, since the load's
  snapshot has no commit time of its own.
- `columns`: the bucket if asked, then the `by` keys, then the measures.
- `rows`: one per group, sorted by key, nulls first.

Values:

- Buckets are RFC 3339 in UTC, the start of the bucket.
- Counts are integers. `sum`, `min` and `max` are exact decimals in their
  column's scale: `120.00` for a `numeric(10, 2)`.
- `avg` is `{"sum": 139.99, "count": 2}`, so you pick the rounding.
- A ref id with no dim row has null attributes, as a `LEFT JOIN` gives.
- An ask without `by` or `time` answers one row even when no row matches,
  as SQL does without `GROUP BY`: counts are 0, the rest null.

Answers never see half a transaction. Straum applies a transaction whole,
at its commit.

## Errors

An ask Straum can't answer gets status 400 and every problem it found:

```json
{"errors": [
  "unknown measure x",
  "customer.zip is not a by column, ref or ref attribute"
]}
```

Straum also rejects a field it doesn't know, a value of the wrong type and
a range end inside a bucket:

```json
{"errors": ["unknown field \"tz\""]}
{"errors": ["range end 1767227400 is not on a 3600s bucket edge"]}
```

| Status | When |
| --- | --- |
| 200 | The answer. |
| 400 | The ask is wrong; `errors` says how. |
| 405 | Not a `POST`. |
| 413 | The ask is over 1 MiB. |
| 500 | Straum failed while answering; its log says why. |
| 503 | `{"errors": ["not ready"]}`: Straum is loading. Ask again later. |

Asks answer while Straum is behind PostgreSQL, with the `lsn` they
reflect. Only a load makes them wait. See [Running
Straum](running.md#readiness-and-lag).

## Freshness

Straum has no "read your writes" barrier yet. Every answer says which
commit it reflects, and the client decides whether that is fresh enough.
`commit_time` is the server's time at that commit, so on a quiet database
it gets old while the answer stays current. `straum status` and `/readyz`
say how far behind PostgreSQL Straum is.
