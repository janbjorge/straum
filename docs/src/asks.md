# Asking

An ask is a JSON object sent to `POST /ask`: a query and its parameters.
The examples use the spec and data from the
[quick start](quick-start.md), after step 7.

```sh
curl --json '{
  "query": "SELECT bucket, customer.country, orders, revenue FROM orders WHERE status = $1 AND customer.country IN ($2, $3) AND bucket >= $4 AND bucket < $5 GROUP BY bucket, customer.country",
  "parameters": ["paid", "NO", "SE", "2026-01-01T00:00:00Z", "2026-01-02T00:00:00Z"]
}' http://localhost:8080/ask
```

| Field | Required | Value |
| --- | --- | --- |
| `query` | yes | One `SELECT` over a view, in Straum SQL. |
| `parameters` | no | The values for `$1`, `$2` and on, in order. |

## The query

The query is Straum SQL, the same PostgreSQL subset made only for Straum
that [specs](spec.md) are written in. Straum never runs it, and anything
outside the subset is an error that names the construct.

```sql
SELECT <keys>, <measures>
FROM <view>
WHERE <conditions> AND bucket >= $n AND bucket < $m
GROUP BY <keys>
```

- `FROM` names one view, with no schema, alias or join.
- A key is a by column (`status`), a ref, which reads the fact column it
  joins on (`customer`), a ref's attribute (`customer.country`), or
  `bucket`, the view's time bucket. A spec can't name anything else
  `bucket`.
- Keys come first in `SELECT`, then measures, by name. `GROUP BY` names
  exactly the selected keys. Without keys there is no `GROUP BY`.
- `WHERE` joins conditions with `AND`. A condition is `key = $n` or
  `key IN ($n, $m)`, and every one must hold. `bucket >= $n` and
  `bucket < $m` are required, once each.
- No `ORDER BY`, `LIMIT`, `HAVING`, `OR`, `NOT`, expressions or aliases.

## Parameters

Every value is a parameter: a value written into the query, such as
`status = 'paid'`, is an error. Values never become query text, so a
value can't change what the query says.

Parameters are integers, strings or booleans, `$1` first. Each must be
used, and one can be used more than once. The `bucket` bounds are
RFC 3339 times, such as `"2026-01-01T00:00:00Z"`, or Unix seconds. Both
are whole seconds on an edge of the view's `date_bin` buckets: Straum
never answers for part of a bucket.

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
- `columns`: the selected keys, then the measures, as in `SELECT`.
- `rows`: one per group, sorted by key, nulls first.

Buckets are their start, in UTC. Counts are integers. `sum`, `min` and
`max` are exact decimals in their column's scale: `120.00` for a
`numeric(10, 2)`. `avg` is `{"sum": 139.99, "count": 2}`, so you pick the
rounding. A ref id with no dim row has null attributes, as a `LEFT JOIN`
gives. An ask without keys answers one row even when nothing matches,
as SQL does without `GROUP BY`: counts are 0, the rest null.

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
{"errors": ["line 1: value, use a parameter such as $1, `'paid'` is not in straum SQL, the PostgreSQL subset made only for straum"]}
{"errors": ["range end 1767227400 is not on a 3600s bucket edge"]}
```
