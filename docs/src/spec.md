# Writing a spec

A spec is a file of `CREATE VIEW` statements, one per fact, in a small
subset of PostgreSQL's syntax. Straum keeps one rollup per view and
builds it itself. Nothing in the spec runs in PostgreSQL: Straum never
creates the view, and to load it copies only the columns the view reads.
Anything outside the subset is an error that names the construct.

This spec, which the [benchmark](benchmark.md#the-whole-view-end-to-end)
follows end to end, uses most of the subset; a [second
example](#keys-of-several-columns-and-null-ness-keys) below uses the
rest:

```sql
CREATE VIEW trips WITH (buckets = '1 day, 7 days') AS
SELECT date_bin('15 minutes', f.pickup_at, timestamptz 'epoch') AS bucket,
       f.payment_type,
       f.vendor_id,
       f.pu_location_id,
       f.do_location_id,
       pickup.borough,
       pickup.zone,
       pickup.service_zone,
       count(*) AS trips,
       sum(f.fare_amount) AS fare,
       max(f.tip_amount) AS tip_max,
       avg(f.trip_distance) AS distance,
       count(*) FILTER (WHERE f.payment_type = 1) AS card,
       count(*) FILTER (WHERE pickup.borough = 'Manhattan') AS manhattan_pickups
FROM public.yellow_trips f
LEFT JOIN public.taxi_zones pickup ON pickup.location_id = f.pu_location_id
LEFT JOIN public.taxi_zones dropoff ON dropoff.location_id = f.do_location_id
GROUP BY 1, 2, 3, 4, 5, 6, 7, 8;
```

## View and fact table

The view's name, `trips`, is the fact's name, which asks read `FROM`.
`FROM public.yellow_trips f` is the fact table. It needs no primary key:
Straum takes the old row of an update or delete from `REPLICA IDENTITY
FULL`, which the [setup SQL](postgresql.md) gives every fact table.

`WITH (buckets = ...)` is the only view option, and optional: wider
bucket widths, under [Time](#time).

Every column is qualified with its table's alias. Identifiers are
unquoted: lowercase letters, digits and `_`, at most 63 bytes.

## Time

Every view has exactly one bucket:

```sql
date_bin('15 minutes', f.pickup_at, timestamptz 'epoch')
```

The column is a `timestamptz` or a `timestamp`. The width is `<n>
seconds`, `minutes`, `hours` or `days`, at most `1 day`, and divides a
day: `15 minutes` does, `7 minutes` doesn't. The origin is `'epoch'` of
the column's type, so buckets start on midnight: `timestamptz 'epoch'`
for a `timestamptz`, `timestamp 'epoch'` for a `timestamp`. The alias is
free; asks call it `bucket`, so no column, ref or measure may be named
`bucket`.

A `timestamp` has no zone, and Straum never gives it one: its buckets are
wall clock, as `date_bin` makes them in PostgreSQL. Ask bounds over such a
view are wall clock too, and answers print its buckets with a `Z` that
means nothing. Straum refuses a view whose origin type is not the
column's, since PostgreSQL would cast the column through the session zone
and the view would not mean what Straum computes.

A row whose time is null, `infinity` or `-infinity` is kept but never
answered: it falls in no bucket an ask can name. Any other value Straum
can't read, such as a `NaN` in a `numeric` column, stops the load or the
stream with the table and column named, and Straum
[starts over](running.md#states) until the row is fixed.

`buckets = '1 day, 7 days'` keeps wider widths too. Each is a whole
multiple of the one before and starts at the epoch, so 7-day buckets start
on Thursdays. Answers don't change. An ask over a long range reads its
middle from the widest buckets that fit and its ends from narrower ones,
so it reads far fewer cells; an ask grouped by `bucket` reads the
`date_bin` width. Each width costs memory: on the taxi month, the day and
week buckets add 719,124 cells to the 2,322,783 at 15 minutes, 31% more
([benchmark](benchmark.md)).

## By columns, refs and dims

Other fact columns, such as `f.payment_type`, are by columns.

```sql
LEFT JOIN public.taxi_zones pickup ON pickup.location_id = f.pu_location_id
```

This declares the ref `pickup`: an N:1 pointer from a fact column to the
key of a dim table. The dim's columns, such as `pickup.borough`, are the
ref's attributes.

- The fact column, `f.pu_location_id`, must be selected: it is a by
  column like any other, and the ref only says which dim row its value
  names.
- Joins go from the fact to a dim only, so a spec can't fan out.
- Two refs can share a dim table, like `pickup` and `dropoff`. A dim table
  is one dim however many views join it, and all must join it on the same
  key columns, in any order.
- The join is `LEFT` because a fact row with no dim row still counts. Its
  attributes are null.

Asks group and filter by all three: `payment_type`, `pickup` and
`pickup.borough`. Straum looks attributes up when it answers, so a change
to a dim row never touches a rollup.

## Keys of several columns, and null-ness keys

A dim's key may be several columns, and a fact column may be both grouped
by and joined on. A nullable column may be a key by whether it is null.
This view has both:

```sql
CREATE VIEW tasks WITH (buckets = '1 day') AS
SELECT date_bin('1 hour', f.created, timestamp 'epoch') AS bucket,
       f.organization_id,
       f.taskgroup_id,
       (f.completed IS NOT NULL) AS done,
       g.category,
       count(*) AS tasks,
       count(*) FILTER (WHERE f.completed IS NOT NULL) AS completed
FROM public.tasks f
LEFT JOIN public.taskgroups g
       ON g.organization_id = f.organization_id
      AND g.taskgroup_id = f.taskgroup_id
GROUP BY 1, 2, 3, 4, 5;
```

- The join names one equality per key column, joined by `AND`. Every
  fact column joined on is selected; `organization_id` is a by column
  and part of the ref's key at once. A ref on one column is a key asks
  can group by, like `pickup`; a ref on several is not: ask for its
  columns, or for its attributes such as `g.category`.
- `(f.completed IS NOT NULL) AS done` is a null-ness key: a boolean, true
  for the rows with a `completed` time, grouped and filtered like a
  column under its name. `IS NULL` makes the opposite key. The column may
  be of any type Straum reads, a time included, but not the view's time
  column: a row with no time is never answered, so the key could never be
  true.
- `FILTER (WHERE f.completed IS NOT NULL)` counts through the null-ness
  key over `completed`, which is why the view needs one: Straum keeps
  nothing else about the column. The view body stays what PostgreSQL
  runs, since the key's name is no column there.

## Measures

| Measure | Input column | Answer |
| --- | --- | --- |
| `count(*)` | | an integer |
| `count(DISTINCT c)` | an integer type, `numeric(p, s)`, `boolean` or `text` | an integer |
| `sum(c)`, `min(c)`, `max(c)` | an integer type, or `numeric(p, s)` with `p` at most 18 | an exact decimal in `c`'s scale |
| `avg(c)` | the same | a decimal rounded to four places past `c`'s scale |

Nulls are skipped, as in SQL. Sums are exact: Straum adds integers, with
numeric values scaled by their column's scale.

`FILTER` makes a measure count only some rows:

```sql
count(*) FILTER (WHERE f.payment_type = 1) AS card
```

The condition is `=` or `IN (...)` on by columns, refs or attributes,
or `IS NULL` or `IS NOT NULL` on a column with a [null-ness
key](#keys-of-several-columns-and-null-ness-keys), joined by `AND`.
Literals are integers, `'text'`, `true` or `false`, and must fit their
column.

By columns, refs, dim keys and attributes are an integer type,
`numeric(p, s)`, `boolean` or `text`. A ref's columns and its dim's key
columns must be of the same kinds, column for column. The column of a
null-ness key may be any type Straum reads.

`GROUP BY` names exactly the selected columns, null-ness keys and the
bucket, by position or by expression, so the view groups the way the
rollup does.

## What a spec can't say

No `WHERE` (filter measures instead), no expressions, no joins other than
refs, no subqueries, no window functions, no `HAVING`, no `ROLLUP`, `CUBE`
or `GROUPING SETS`. Ask PostgreSQL for those.

## Checking a spec

`straum plan --spec FILE` prints the setup SQL, or what is wrong and exits
1. Syntax outside the subset comes first, with its line, then every
problem between declarations at once:

```text
straum: orders.sql: line 2: WHERE, filter measures instead, is not in straum SQL, the PostgreSQL subset made only for straum
```

```text
straum: orders.sql: orders: orders declared twice
orders.sql: fact refunds: grain of 420s does not divide a day
```

`straum run` checks the tables themselves, such as a missing column or a
wrong type, when it starts.

## Memory

Memory follows cells. Each bucket width keeps one cell per distinct
combination of by keys and bucket, the columns refs join on among them,
so per width:

```text
cells <= min(rows, distinct key combinations x buckets)
```

Keys that are nearly unique per row keep nearly a cell per row. In the
benchmark, pickup and dropoff zone at 15 minutes keep 2,322,783 cells for
2,964,624 trips; without the dropoff zone, 568,612. Fewer key columns or
wider buckets collapse more rows into each cell. `count(DISTINCT c)` adds
an entry per distinct value in each cell.
