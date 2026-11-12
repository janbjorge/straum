# Writing a spec

A spec is a file of `CREATE VIEW` statements, one per table you
aggregate, in a small subset of PostgreSQL's syntax. PostgreSQL would
give the same rows for each view. Straum never creates it, and nothing
in the spec runs in PostgreSQL: to load a view, Straum copies only the
columns it reads.

Straum SQL has one spelling per meaning and infers nothing
([ADR-0029](https://github.com/janbjorge/straum/blob/main/docs/adr/0029-straum-sql-has-one-spelling-per-meaning.md)).
The synopsis is the whole language: anything it does not show is an
error that names the construct and says what to write.

## Synopsis

```text
CREATE VIEW view WITH ( buckets = 'width [, ...]' ) AS
SELECT item AS name [, ...]
FROM schema.table AS alias
[ { INNER JOIN | LEFT OUTER JOIN } schema.table AS alias
    ON equality [ AND ... ] ] [ ... ]
[ WHERE { condition | exists } [ AND ... ] ]
GROUP BY position [, ...]

where item is one of:

    date_bin('width', alias.column, { timestamptz | timestamp } 'epoch')
    column
    ( column IS [ NOT ] NULL )
    count(*) [ filter ]
    count( [ DISTINCT ] alias.column ) [ filter ]
    { sum | min | max | avg | bool_and | bool_or } ( alias.column ) [ filter ]

and filter is:

    FILTER ( WHERE condition [ AND ... ] )

and condition is one of:

    column = literal
    column IN ( literal, literal [, ...] )
    column IS [ NOT ] NULL

and exists is:

    [ NOT ] EXISTS ( SELECT 1 FROM schema.table AS alias
                     WHERE equality [ AND ... ] )

and equality is:

    alias.column = alias.column

and column is:

    alias.column [ ::text ]

and width is:

    n { second | minute | hour | day }[s]

and literal is one of:

    [ - ] integer
    'text'
    true
    false
```

Brackets hold what may be left out, braces a choice, and `...` says
the part before it repeats. A spec is one or more views, with `;`
between them. Identifiers are unquoted: lowercase letters, digits and
`_`, at most 63 bytes.

This is the [quick start](quick-start.md) view with a wider bucket, a
null-ness key and two filtered measures:

```sql
CREATE VIEW orders WITH (buckets = '1 hour, 1 day') AS
SELECT date_bin('1 hour', orders.placed_at, timestamptz 'epoch') AS bucket,
       orders.status AS status,
       orders.customer_id AS customer_id,
       customers.country AS country,
       (orders.shipped_at IS NOT NULL) AS shipped,
       count(*) AS orders,
       sum(orders.amount) AS revenue,
       avg(orders.amount) AS avg_amount,
       count(*) FILTER (WHERE orders.status = 'paid') AS paid,
       count(*) FILTER (WHERE orders.shipped_at IS NOT NULL) AS shipped_orders
FROM public.orders AS orders
LEFT OUTER JOIN public.customers AS customers ON customers.id = orders.customer_id
GROUP BY 1, 2, 3, 4, 5;
```

Straum keeps one cell per distinct hour, status, customer, country and
shipped-ness, holding what the measures need. An ask merges cells and
never reads a row.

## The tables

`FROM` names the fact table, the table whose rows the view counts. A
table the view joins is a dim. `straum plan` and the setup check call
them fact and dim.

The fact table needs no primary key. Straum takes the old row of an
update or delete from `REPLICA IDENTITY FULL`, which the [setup
SQL](postgresql.md#2-run-the-setup-sql) gives every fact table.

## Keys

Every `SELECT` item that is not a measure is a key, and each cell is
one distinct combination of the keys. A key is a column of the fact
table (`orders.status`), a column of a dim (`customers.country`), a
[null-ness key](#null-ness-keys) or the [time bucket](#time).

The `AS name` is what asks use, and it is the only way an ask reaches
the item: it says `country`, never `customers.country`. Names are
unique within a view. `GROUP BY` lists the keys by their positions in
`SELECT`, in order: a position is what PostgreSQL resolves without a
lookup, where an output name it may read as an input column.

A key column is an integer type, `numeric(p, s)`, `boolean` or `text`,
or an [enum written `::text`](#enum-columns). Fewer keys, or keys with
fewer values, mean fewer cells ([Memory](#memory)).

## Time

Every view has exactly one bucket, named `bucket`:

```sql
date_bin('1 hour', orders.placed_at, timestamptz 'epoch') AS bucket
```

- The column is a `timestamptz` or a `timestamp`. The origin is `'epoch'`
  of the column's type, so buckets start on midnight. Straum refuses an
  origin of the other type, since PostgreSQL would then cast the column
  through the session zone and the view would not mean what Straum
  computes.
- The width is at most `1 day` and divides a day: `15 minutes` does,
  `7 minutes` doesn't.
- The name is `bucket`, which is what asks call it, so nothing else in
  the view may be named `bucket`.
- A `timestamp` has no zone, and Straum never gives it one. Its buckets
  and ask bounds are wall clock, and answers print them with a `Z` that
  means nothing.
- A row whose time is null, `infinity` or `-infinity` is kept but never
  answered. Any other value Straum can't read, such as a `NaN`, stops
  the load or the stream with the table and column named.

`WITH (buckets = '1 hour, 1 day')` lists every bucket width Straum
keeps, narrowest first, and the first must be the `date_bin` width, so the whole list is in one
place while the body stays what PostgreSQL runs. Each wider width is a
whole multiple of the one before and starts at the epoch, so `7 days`
buckets start on Thursdays. Answers don't change. An ask over a long
range reads its middle from the widest buckets that fit and its ends
from narrower ones, so it reads far fewer cells; both are drawn under [How Straum
works](how-it-works.md#answering-an-ask). Aggregate values stay the same;
only which cells an ask reads changes. Each width costs memory ([Memory](#memory)).

## Joining a dim

```sql
INNER JOIN public.customers AS customers ON customers.id = orders.customer_id
LEFT OUTER JOIN public.products AS products ON products.id = orders.product_id
```

A join points from a column of the fact, or of a dim already joined, to
the key of a dim. The dim columns the view selects, here
`customers.country AS country`, are keys like any other. Straum keys
each cell by the join column, `customer_id`, and holds the dim projected
to its key and the columns the views read, kept current from the stream;
an ask reads `country` through the cell's `customer_id` as the customer
rows are now. Beside those cells it keeps bound cells keyed by
`country` itself, which answer asks without the lookup.

- `INNER JOIN` keeps a fact row only while a dim row matches it;
  `LEFT OUTER JOIN` keeps every fact row, with null dim columns while
  none matches. A `RIGHT` or `FULL` join would add dim rows no fact row
  points at, whose bucket is null and which no ask reads, so it answers
  as `INNER` or `LEFT OUTER`; write that.
- An `ON` equality has the joined table's column on the left. Its right
  side names the fact's alias or the alias of a dim joined before it, a
  chain:

  ```sql
  INNER JOIN public.customers AS customers ON customers.id = orders.customer_id
  LEFT OUTER JOIN public.tiers AS tiers ON tiers.id = customers.tier_id
  ```

  Straum then holds `tier_id` in its projection of `customers` and
  reads `tiers` through it. A chain through a semi or anti join is
  refused: those keep no row to read through.
- Any number of dim rows may match a key. A key the dim holds twice
  counts the fact row twice, as PostgreSQL's join does: `count(*)`,
  `count(c)` and `sum` double, `min`, `max`, `count(DISTINCT ...)`,
  `bool_and` and `bool_or` don't, `avg` stays. A dim needs no index on
  its key.
- The fact column and the dim key column must be of the same kind,
  column for column.
- A join must select at least one dim column. The fact column joined on,
  `orders.customer_id`, may be selected too or left out; either way it
  keys the cells, so a view with a join has one cell per distinct join
  id, bucket and other keys ([Memory](#memory)).
- Two joins can point at one dim table, each reading its own columns. A
  dim table is one dim however many views join it, and all must join it
  on the same key columns.
- The join itself has no value: asks group and filter by `country` or
  `customer_id`, never by `customers`.

Dim columns are as of now. The setup publishes the dims too, and a
change to a customer row is one entry in Straum's projection of
`customers`: a customer who moves from NO to SE moves every order of
theirs to SE in the next answer, as PostgreSQL's view would. Nothing
starts over and no cell moves, since cells are keyed by `customer_id`,
not by the country.

## Semi and anti joins

```sql
WHERE EXISTS (SELECT 1 FROM public.promos AS promos WHERE promos.sku = orders.sku)
  AND NOT EXISTS (SELECT 1 FROM public.blocked AS blocked WHERE blocked.id = orders.customer_id)
```

`EXISTS` keeps a fact row while at least one dim row matches it, once
however many match; `NOT EXISTS` keeps it while none does. Its
equalities are those of `ON`. No column of the subquery's table can be
selected, filtered on or joined through. Both follow the dim as it is
now, like any join.

## The view's WHERE

```sql
WHERE orders.status IN ('paid', 'shipped') AND customers.country = 'NO'
```

`WHERE` drops rows from every answer, as the view's `WHERE` does in
PostgreSQL. Its conditions are those of a `FILTER`, below, on columns
the view selects as keys, so a condition on a dim column follows the
dim. A `FILTER` that every measure repeats is refused: write it here.

## Null-ness keys

```sql
(orders.shipped_at IS NOT NULL) AS shipped
```

A null-ness key is a boolean, true for the rows with a `shipped_at`,
grouped and filtered like a column under its name. `IS NULL` makes the
opposite key. The column may be of any type Straum reads, a time
included, but not the view's time column: a row with no time is never
answered, so the key could never be true. The view body stays what
PostgreSQL runs, since the key's name is no column there.

## Enum columns

```sql
orders.status::text AS status
```

An enum column is read as its label, the text PostgreSQL prints for
it, by writing `::text` wherever the view reads the column: on a key,
in a null-ness key, `(orders.status::text IS NULL)`, in a `FILTER` and
in `WHERE`, spelled alike in every place and in every view that reads
the column. `::text` is the one cast, and it goes nowhere else: not in
`ON`, `EXISTS`, `date_bin` or a measure. Straum refuses an enum column
written without it, and the cast on a column of any other type, so a
spec says what it reads. A `FILTER` literal must be one of the type's
labels, which Straum reads at start. The label is a text key: asks
filter it with a string, and answers sort labels as text, not in the
type's order ([ADR-0037](https://github.com/janbjorge/straum/blob/main/docs/adr/0037-an-enum-column-is-read-as-its-label-when-cast-to-text.md)).
Which types take the cast is listed here; today it is enums alone.

## Measures

| Measure | Input column | Answer |
| --- | --- | --- |
| `count(*)` | | an integer |
| `count(c)` | an integer type, `numeric(p, s)`, `boolean`, `text`, `timestamptz` or `timestamp` | an integer |
| `count(DISTINCT c)` | an integer type, `numeric(p, s)`, `boolean` or `text` | an integer |
| `sum(c)`, `min(c)`, `max(c)` | an integer type, or `numeric(p, s)` with `p` at most 18 | an exact decimal in `c`'s scale |
| `avg(c)` | the same | a decimal rounded half to even at four places past `c`'s scale |
| `bool_and(c)`, `bool_or(c)` | `boolean` | `true` or `false` |

Nulls are skipped, as in SQL. Sums are exact: Straum adds integers, with
numeric values scaled by their column's scale. `count(*)` counts rows
and `count(c)` the rows where `c` is not null. `bool_and(c)` is true
when every `c` that is not null is true, and `bool_or(c)` when any is;
both are null when no `c` is
([ADR-0041](https://github.com/janbjorge/straum/blob/main/docs/adr/0041-bool-and-and-bool-or-keep-counts-of-true-and-false.md)).

`FILTER` makes a measure count only some rows:

```sql
count(*) FILTER (WHERE orders.status = 'paid') AS paid
count(*) FILTER (WHERE orders.shipped_at IS NOT NULL) AS shipped_orders
```

`=` and `IN` take a column the view selects as a key, `IS NULL` and
`IS NOT NULL` a column with a null-ness key. The second filter is why
the view needs the `shipped` key: Straum keeps nothing else about
`shipped_at`. A literal must fit its column.

## Checking a spec

`straum plan --spec FILE` prints the setup SQL, or what is wrong and exits
1. Syntax outside the subset comes first, with its line, then every
problem between declarations at once:

```text
straum: orders.sql: line 5: JOIN, write INNER JOIN, `JOIN public.customers AS customers ON customers.id = orders.customer_id` is not in Straum SQL, the PostgreSQL subset made only for Straum
```

```text
straum: orders.sql: orders declared twice
orders.sql: view orders: width of 420s does not divide a day
```

`straum run` checks the tables themselves, such as a missing column or a
wrong type, [when it starts](postgresql.md#what-straum-checks-at-start).

## Memory

Memory follows cells: one per distinct combination of keys, join
columns and bucket, per width, plus one entry per dim row for the
columns the views read. Fewer keys, keys with fewer values and wider
buckets mean fewer cells; a join to a dim with many rows means many
cells, one per distinct join-column value in the fact. [How Straum
works](how-it-works.md#cells) has the formula and the
[benchmark](benchmark.md#memory) what real tables measured.
