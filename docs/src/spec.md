# Writing a spec

A spec is a file of `CREATE VIEW` statements, one per table you
aggregate, in a small subset of PostgreSQL's syntax. PostgreSQL would
give the same rows for each view. Straum never creates it, and nothing
in the spec runs in PostgreSQL: to load a view, Straum copies only the
columns it reads. Anything outside the subset is an error that names the
construct.

This is the [quick start](quick-start.md) view with everything the
subset adds: a wider bucket, a null-ness key and two filtered measures.

```sql
CREATE VIEW orders WITH (buckets = '1 day') AS
SELECT date_bin('1 hour', placed_at, timestamptz 'epoch') AS hour,
       status,
       customer_id,
       customers.country,
       (shipped_at IS NOT NULL) AS shipped,
       count(*) AS orders,
       sum(amount) AS revenue,
       avg(amount) AS avg_amount,
       count(*) FILTER (WHERE status = 'paid') AS paid,
       count(*) FILTER (WHERE shipped_at IS NOT NULL) AS shipped_orders
FROM orders
LEFT JOIN customers ON customers.id = customer_id
GROUP BY 1, 2, 3, 4, 5;
```

| Line | What it declares | Where |
| --- | --- | --- |
| `CREATE VIEW orders` | The name asks read `FROM`. | [The tables](#the-tables) |
| `WITH (buckets = '1 day')` | Day buckets kept beside the hours. | [Time](#time) |
| `date_bin('1 hour', placed_at, ...)` | The time bucket, one per view. | [Time](#time) |
| `status, customer_id` | Keys: columns of the fact table. | [Keys](#keys) |
| `customers.country` | A key from the joined table. | [Joining a lookup table](#joining-a-lookup-table) |
| `(shipped_at IS NOT NULL) AS shipped` | A boolean key: does the row have a `shipped_at`? | [Null-ness keys](#null-ness-keys) |
| `count(*)` to `avg(amount)` | Measures, and what each cell holds. | [Measures](#measures) |
| `count(*) FILTER (WHERE ...)` | Measures over some rows only. | [Measures](#measures) |
| `FROM orders` | The fact table, whose rows are counted. | [The tables](#the-tables) |
| `LEFT JOIN customers ON ...` | A dim: a lookup table joined N:1. | [Joining a lookup table](#joining-a-lookup-table) |
| `GROUP BY 1, 2, 3, 4, 5` | The keys again, as PostgreSQL wants. | [Keys](#keys) |

Straum keeps one cell per distinct hour, status, customer, country and
shipped-ness, holding what the measures need. An ask merges cells and
never reads a row.

## The tables

`FROM orders` names the fact table, the table whose rows the view
counts: `public.orders`, since a table without a schema is in `public`.
A table the view joins is a dim, a lookup table. `straum plan` and the
start checks call them fact and dim.

The fact table needs no primary key. Straum takes the old row of an
update or delete from `REPLICA IDENTITY FULL`, which the [setup
SQL](postgresql.md#2-run-the-setup-sql) gives every fact table.

A bare column is the fact's. A dim's columns go through its alias, which
is its name unless the join gives one, as `customers.country` does.
Identifiers are unquoted: lowercase letters, digits and `_`, at most 63
bytes.

## Keys

Every selected column that is not a measure is a key, and each cell is
one distinct combination of the keys. A key is a column of the fact
table (`status`, `customer_id`), a column of a dim (`customers.country`),
a [null-ness key](#null-ness-keys) or the [time bucket](#time). `GROUP
BY` names exactly the keys, by position or by expression, so the view
stays one PostgreSQL would accept.

A key column is an integer type, `numeric(p, s)`, `boolean` or `text`.
Fewer keys, or keys with fewer values, mean fewer cells; see
[Memory](#memory).

## Time

Every view has exactly one bucket:

```sql
date_bin('1 hour', placed_at, timestamptz 'epoch')
```

- The column is a `timestamptz` or a `timestamp`. The origin is `'epoch'`
  of the column's type, so buckets start on midnight. Straum refuses an
  origin of the other type, since PostgreSQL would then cast the column
  through the session zone and the view would not mean what Straum
  computes.
- The width is `<n> seconds`, `minutes`, `hours` or `days`, at most
  `1 day`, and divides a day: `15 minutes` does, `7 minutes` doesn't.
- The alias is free; asks call it `bucket`, so nothing else in the view
  may be named `bucket`.
- A `timestamp` has no zone, and Straum never gives it one. Its buckets
  and ask bounds are wall clock, and answers print them with a `Z` that
  means nothing.
- A row whose time is null, `infinity` or `-infinity` is kept but never
  answered. Any other value Straum can't read, such as a `NaN`, stops
  the load or the stream with the table and column named.

`WITH (buckets = '1 day')` is the only view option. It keeps wider
buckets beside the `date_bin` width: each listed width is a whole
multiple of the one before and starts at the epoch, so `7 days` buckets
start on Thursdays. Answers don't change. An ask over a long range reads
its middle from the widest buckets that fit and its ends from narrower
ones, so it reads far fewer cells. Each width costs memory, under
[Memory](#memory).

## Joining a lookup table

```sql
LEFT JOIN customers ON customers.id = customer_id
```

The join points from a fact column to the key of a dim, N:1. The dim
columns the view selects, here `customers.country`, are keys like any
other: as each order comes in, Straum reads them from the customer row
the join names, in PostgreSQL, and keys the cell by them. Straum keeps
no customer row of its own.

- The join is `LEFT` because a fact row with no dim row still counts;
  its dim columns are null.
- Joins go from the fact to a dim only, so a spec can't fan out. The dim
  needs a primary key or unique index on exactly its key columns, so a
  key names one row. A dim keyed by several columns joins on one
  equality per key column, joined by `AND`:

  ```sql
  LEFT JOIN prices ON prices.sku = sku AND prices.currency = currency
  ```

- The fact column and the dim key column must be of the same kind,
  column for column.
- A join must select at least one dim column. The fact column joined on,
  `customer_id`, may be selected too or left out; left out, cells
  collapse to the dim column's few values ([Memory](#memory)).
- Two joins can point at one dim table, each reading its own columns. A
  dim table is one dim however many views join it, and all must join it
  on the same key columns.
- The join itself has no value: asks group and filter by
  `customers.country`, or by `customer_id`, never by `customers`.

Dim columns are as of ingest. Straum never follows a dim: the setup
publishes the fact tables only. A customer who moves from NO to SE keeps
their old orders under NO, while PostgreSQL's view would move them; new
orders read SE. When an order that came in under NO is updated or
deleted, Straum can't find its cell and [starts
over](how-it-works.md#applying-a-commit). For dims that change often,
select the fact column too and keep the mapping in your client.

## Null-ness keys

```sql
(shipped_at IS NOT NULL) AS shipped
```

A null-ness key is a boolean, true for the rows with a `shipped_at`,
grouped and filtered like a column under its name. `IS NULL` makes the
opposite key. The column may be of any type Straum reads, a time
included, but not the view's time column: a row with no time is never
answered, so the key could never be true. The view body stays what
PostgreSQL runs, since the key's name is no column there.

## Measures

| Measure | Input column | Answer |
| --- | --- | --- |
| `count(*)` | | an integer |
| `count(DISTINCT c)` | an integer type, `numeric(p, s)`, `boolean` or `text` | an integer |
| `sum(c)`, `min(c)`, `max(c)` | an integer type, or `numeric(p, s)` with `p` at most 18 | an exact decimal in `c`'s scale |
| `avg(c)` | the same | a decimal rounded half to even at four places past `c`'s scale |

Nulls are skipped, as in SQL. Sums are exact: Straum adds integers, with
numeric values scaled by their column's scale.

`FILTER` makes a measure count only some rows:

```sql
count(*) FILTER (WHERE status = 'paid') AS paid
count(*) FILTER (WHERE shipped_at IS NOT NULL) AS shipped_orders
```

The condition is `=` or `IN (...)` on keys the view selects, or `IS
NULL` or `IS NOT NULL` on a column with a null-ness key, joined by
`AND`. The second filter is why the view needs the `shipped` key: Straum
keeps nothing else about `shipped_at`. Literals are integers, `'text'`,
`true` or `false`, and must fit their column.

## What a spec can't say

No `WHERE` (filter measures instead), no expressions, no joins other than
to a dim's key, no subqueries, no window functions, no `HAVING`, no
`ROLLUP`, `CUBE` or `GROUPING SETS`. Ask PostgreSQL for those.

## Checking a spec

`straum plan --spec FILE` prints the setup SQL, or what is wrong and exits
1. Syntax outside the subset comes first, with its line, then every
problem between declarations at once:

```text
straum: orders.sql: line 5: WHERE, filter measures instead, is not in straum SQL, the PostgreSQL subset made only for straum
```

```text
straum: orders.sql: orders declared twice
orders.sql: fact orders: grain of 420s does not divide a day
```

`straum run` checks the tables themselves, such as a missing column or a
wrong type, [when it starts](postgresql.md#what-straum-checks-at-start).

## Memory

Memory follows cells: one per distinct combination of keys and bucket,
per width. Fewer keys, keys with fewer values and wider buckets mean
fewer cells. [Sizing memory](sizing.md) has the formula and what real
tables measured.
