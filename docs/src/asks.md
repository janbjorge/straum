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
- A key is a by column (`status`), a null-ness key by its name, a ref's
  attribute the view selects (`customer.country`), or `bucket`, the
  view's time bucket. A ref has no value of its own: ask for the fact
  column it joins on (`customer_id`) or its attributes. A spec can't name
  anything else `bucket`.
- Keys come first in `SELECT`, then measures, by name, each once, since
  they name the fields of each answer row. `GROUP BY` names exactly the
  selected keys. Without keys there is no `GROUP BY`.
- `WHERE` joins conditions with `AND`. A condition is `key = $n` or
  `key IN ($n, $m)`, and every one must hold. `bucket >= $n` and
  `bucket < $m` are required, once each.
- No `ORDER BY`, `LIMIT`, `HAVING`, `OR`, `NOT`, expressions or aliases.

## Parameters

Every value is a parameter: a value written into the query, such as
`status = 'paid'`, is an error. Values never become query text, so a
value can't change what the query says.

Parameters are integers, strings or booleans, `$1` first. Each must be
used, and one can be used more than once.

A condition's parameters must have the column's type, with no
conversion: an integer for an integer column or a `numeric(p, 0)`, a
string for text, a boolean for a boolean or a null-ness key. `status = $1`
with `1`, or
`customer_id = $1` with `"7"`, is an error, not an answer with no rows. A
`numeric` with a scale above 0 takes no parameter at all: at scale 2,
`1` would read as 0.01. The `bucket` bounds are
RFC 3339 times, such as `"2026-01-01T00:00:00Z"`, or Unix seconds. Both
are whole seconds on an edge of the view's `date_bin` buckets: Straum
never answers for part of a bucket.

## The answer

```json
{
  "as_of": "2026-11-09T17:12:57.861204Z",
  "rows": [
    {"bucket": "2026-01-01T09:00:00Z", "customer.country": "NO", "orders": 1, "revenue": 120.00},
    {"bucket": "2026-01-01T09:00:00Z", "customer.country": "SE", "orders": 1, "revenue": 80.50},
    {"bucket": "2026-01-01T11:00:00Z", "customer.country": "SE", "orders": 1, "revenue": 300.00}
  ]
}
```

- `as_of`: every commit made before this time, by Straum's clock, is in
  the answer. It is the last time Straum knew it had applied all the WAL
  PostgreSQL had written, so it keeps moving on a quiet database. Straum
  applies a transaction whole, so an answer never sees half of one.
- `rows`: one object per group, keyed by the names in `SELECT`, sorted by
  key, nulls first.

Buckets are their start, in UTC. Counts are integers. `sum`, `min` and
`max` are exact decimals in their column's scale: `120.00` for a
`numeric(10, 2)`. `avg` is rounded half to even at four places past that
scale, with trailing zeros dropped: `69.995` for 139.99 over 2. A row
whose dim row did not exist when it came in has null attributes, as a
`LEFT JOIN` gives. An ask without keys answers one row even when nothing
matches, as SQL does without `GROUP BY`: counts are 0, the rest null.

The `Straum-LSN` header names the last commit the answer reflects, for
debugging. Straum has no "read your writes" barrier yet, so `as_of` is
how a client judges whether an answer is fresh enough.

The `Server-Timing` header says where an answer's time went in Straum, in
milliseconds: `parse` reads the ask, `answer` reads the cells, `json`
writes the rows, and `total` runs from the request to the reply. What a
client measures beyond `total` is the network and the client itself.
Errors carry no timing.

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
{"errors": ["unknown measure x", "customer.zip is not read by the view, which reads customer.country"]}
{"errors": ["customer is a join and has no value of its own; ask for customer.country"]}
{"errors": ["unknown field \"tz\""]}
{"errors": ["line 1: value, use a parameter such as $1, `'paid'` is not in straum SQL, the PostgreSQL subset made only for straum"]}
{"errors": ["range end 1767227400 is not on a 3600s bucket edge"]}
{"errors": ["customer_id is an integer, so it can't be the text \"7\""]}
```
