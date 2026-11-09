# Asking

An ask is a JSON object sent to `POST /ask`. The examples use the spec
and data from the [quick start](quick-start.md), after step 7.

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

| Field | Required | Value |
| --- | --- | --- |
| `fact` | yes | The view's name. |
| `measures` | yes | Measure names, at least one. |
| `by` | no | Keys to group by, in order. |
| `time` | no | `"bucket"`, to group by the time bucket first. |
| `filter` | no | Key to value, or key to a list of values. |
| `from` | yes | Where the range starts, included. |
| `to` | yes | Where the range ends, left out. |

A key is a by column (`"status"`), a ref, which reads the fact column it
joins on (`"customer"`), or a ref's attribute (`"customer.country"`). The
bucket has a field of its own, so it can't clash with a column named
`bucket`.

`filter` values are integers, strings or booleans. A list matches any of
its values and can't be empty. Every condition must hold.

`from` and `to` are RFC 3339 times, such as `"2026-01-01T00:00:00Z"`, or
Unix seconds. Both are whole seconds on an edge of the view's `date_bin`
buckets: Straum never answers for part of a bucket.

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
  Straum applies a transaction whole, so an answer never sees half of
  one.
- `commit_time`: when that commit committed, by the server's clock. It is
  null until the first commit after a load, and on a quiet database it
  ages while the answer stays current.
- `columns`: the bucket if asked, then the `by` keys, then the measures.
- `rows`: one per group, sorted by key, nulls first.

Buckets are their start, in UTC. Counts are integers. `sum`, `min` and
`max` are exact decimals in their column's scale: `120.00` for a
`numeric(10, 2)`. `avg` is `{"sum": 139.99, "count": 2}`, so you pick the
rounding. A ref id with no dim row has null attributes, as a `LEFT JOIN`
gives. An ask without `by` or `time` answers one row even when nothing
matches, as SQL does without `GROUP BY`: counts are 0, the rest null.

Straum has no "read your writes" barrier yet. The client decides from
`lsn` whether an answer is fresh enough.

## Errors

| Status | When |
| --- | --- |
| 200 | The answer. |
| 400 | The ask is wrong; `errors` lists every problem. |
| 405 | Not a `POST`. |
| 413 | The ask is over 1 MiB. |
| 500 | Straum failed while answering; its log says why. |
| 503 | `{"errors": ["not ready"]}`: Straum is [loading](running.md#states). Ask again later. |

```json
{"errors": ["unknown measure x", "customer.zip is not a by column, ref or ref attribute"]}
{"errors": ["unknown field \"tz\""]}
{"errors": ["range end 1767227400 is not on a 3600s bucket edge"]}
```
