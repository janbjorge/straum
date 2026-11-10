# Writing a spec

A spec is a file of `CREATE VIEW` statements, one per fact, in a small
subset of PostgreSQL's syntax. Nothing in it runs in PostgreSQL: Straum
never creates the view, and to load it copies only the columns the view
reads. Anything outside the subset is an error that names the construct.

This is the [quick start](quick-start.md) view with everything the subset
has added to it: a wider bucket, a null-ness key and two filtered
measures.

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

Straum keeps one cell per distinct hour, status, customer, country and
shipped-ness, holding what the measures need. An ask merges cells and
never reads a row.

## The view and its fact table

The view's name, `orders`, is what asks read `FROM`. `FROM orders` is the
fact table, `public.orders`: a table without a schema is in `public`. It
needs no primary key: Straum takes the old row of an update or delete
from `REPLICA IDENTITY FULL`, which the [setup
SQL](postgresql.md#2-run-the-setup-sql) gives every fact table.

A bare column is the fact's. A joined table's columns go through its
alias, which is its name unless the join gives one, as `customers.country`
does. Identifiers are unquoted: lowercase letters, digits and `_`, at most
63 bytes. `GROUP BY` names exactly the selected keys, by position or by
expression.

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

## Dims: refs and attributes

Other fact columns, such as `status`, are by columns. A join declares a
ref:

```sql
LEFT JOIN customers ON customers.id = customer_id
```

`customers` is an N:1 pointer from a fact column to the key of a dim
table. The dim columns the view selects, here `customers.country`, are
the ref's attributes. As each order comes in, Straum reads them from the
customer row the join names, in PostgreSQL, and keys the cell by them
like any by column. Straum keeps no customer row of its own.

- The join is `LEFT` because a fact row with no dim row still counts;
  its attributes are null.
- Joins go from the fact to a dim only, so a spec can't fan out. The dim
  needs a primary key or unique index on exactly its key columns, so a
  key names one row. A dim keyed by several columns joins on one
  equality per key column, joined by `AND`:

  ```sql
  LEFT JOIN prices ON prices.sku = sku AND prices.currency = currency
  ```

- A join must select at least one dim column. The fact column joined on,
  `customer_id`, may be selected too or left out; left out, cells
  collapse to the attribute's few values ([Memory](#memory)).
- Two refs can join one dim table, each reading its own attributes. A
  dim table is one dim however many views join it, and all must join it
  on the same key columns.
- The ref itself has no value: asks group and filter by
  `customers.country`, or by `customer_id`, never by `customers`.

Attributes are as of ingest. Straum never follows a dim: the setup
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

The condition is `=` or `IN (...)` on by columns or attributes the view
selects, or `IS NULL` or `IS NOT NULL` on a column with a null-ness key,
joined by `AND`. The second filter is why the view needs the `shipped`
key: Straum keeps nothing else about `shipped_at`. Literals are
integers, `'text'`, `true` or `false`, and must fit their column.

## Types

By columns, dim keys and attributes are an integer type, `numeric(p,
s)`, `boolean` or `text`. A ref's columns and its dim's key columns must
be of the same kinds, column for column. The column of a null-ness key
may be any type Straum reads.

## What a spec can't say

No `WHERE` (filter measures instead), no expressions, no joins other than
refs, no subqueries, no window functions, no `HAVING`, no `ROLLUP`, `CUBE`
or `GROUPING SETS`. Ask PostgreSQL for those.

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

Memory follows cells, and only cells: Straum holds no dim rows. Each
bucket width keeps one cell per distinct combination of keys and bucket,
so per width:

```text
cells <= min(rows, distinct key combinations x buckets)
```

Keys that are nearly unique per row keep nearly a cell per row; fewer
key columns or wider buckets collapse more rows into each cell, and
selecting a dim's attribute instead of the fact column joined on
collapses most. `count(DISTINCT c)` adds an entry per distinct value in
each cell. The [benchmark](benchmark.md#cells) measures the lever on
real tables.
