# Writing a spec

A spec is a file of `CREATE VIEW` statements, one per fact. Straum reads
it at start and keeps one rollup per view. The syntax is a small subset
of PostgreSQL's, made only for Straum specs. Anything outside the subset
is an error that names what isn't allowed. Straum never creates the view
in PostgreSQL; it only sends the `SELECT` there, to load.

This spec, from Straum's tests, uses every part of the subset:

```sql
CREATE VIEW trips WITH (key = id, buckets = '1 day, 7 days') AS
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

## The view

`CREATE VIEW trips WITH (key = id) AS ...` declares the fact `trips`. Asks
name it in `"fact"`.

- `key` is required: the fact table's primary key. It is never part of
  the rollup, so it can't be a by column, a ref or the time column.
- `buckets` is optional: wider bucket widths Straum keeps beside the
  `date_bin` one. See [Time](#time).

## The fact table

`FROM schema.table alias` names the fact table. Every column in the view
is qualified with a table alias, and identifiers are unquoted: lowercase
letters, digits and `_`, at most 63 bytes.

## Time

Every view has exactly one bucket:

```sql
date_bin('15 minutes', f.pickup_at, timestamptz 'epoch')
```

- The column is a `timestamptz`.
- The width is `<n> seconds`, `minutes`, `hours` or `days`, and divides a
  day: `15 minutes` and `1 hour` do, `7 minutes` doesn't. The widest is
  `1 day`.
- The origin is always `timestamptz 'epoch'`, so buckets start on UTC
  midnight.

The bucket's alias is free. Asks always call it `bucket`.

`buckets = '1 day, 7 days'` keeps wider buckets too. Each width is a whole
multiple of the one before it, and starts at the epoch like the `date_bin`
one, so 7-day buckets start on Thursdays. They don't change answers. An
ask over a long range reads its middle from the widest buckets that fit and
its ends from narrower ones, so it reads far fewer cells. Each width costs
memory: on a month of New York taxi trips, day and week buckets are
modelled to add about 15% to the hourly cells.

## By columns

Other columns of the fact table are by columns: `f.payment_type`. Asks can
group and filter by them.

## Refs and dims

```sql
LEFT JOIN public.taxi_zones pickup ON pickup.location_id = f.pu_location_id
```

A `LEFT JOIN` declares a ref named by its alias, `pickup`: an N:1 pointer
from a fact column to the key of a dim table. Joins point from the fact to
a dim and nowhere else, so a spec can't fan out.

- The fact column, `f.pu_location_id`, must be selected.
- Columns of the dim, `pickup.borough`, are attributes. Asks group and
  filter by them as `pickup.borough`.
- Two refs can point at one dim table, like `pickup` and `dropoff` above.
  A dim table is one dim however many views join it, and they must join it
  on the same key.
- It's `LEFT` because a fact row with no dim row still counts. Its
  attributes are null.

Straum keeps dim rows by key and looks up attributes when it answers, not
when it loads. A change to a dim row never touches a rollup.

## Measures

Each measure is an aggregate with a name:

| Measure | Reads | Answers |
| --- | --- | --- |
| `count(*)` | rows | an integer |
| `count(DISTINCT c)` | an integer, numeric, boolean or text column | an integer |
| `sum(c)` | an integer or `numeric(p, s)` column, `p` at most 18 | a decimal in `c`'s scale |
| `min(c)`, `max(c)` | the same | a decimal in `c`'s scale |
| `avg(c)` | the same | `{"sum": ..., "count": ...}` |

Nulls are skipped, as in SQL. Sums are exact: Straum adds integers, with
numeric values scaled by their column's scale.

A measure can count only some rows:

```sql
count(*) FILTER (WHERE f.payment_type = 1) AS card
```

The condition is `=` or `IN (...)` on by columns, refs or attributes,
joined by `AND`. Literals are integers, `'text'`, `true` or `false`.

## GROUP BY

`GROUP BY` names exactly the selected columns and the bucket, by position
or by expression, so the view groups the way the rollup does.

## Column types

Straum checks the tables against the spec when it starts:

| Column | Type |
| --- | --- |
| The time column | `timestamptz` |
| `sum`, `min`, `max` and `avg` inputs | an integer type, or `numeric(p, s)` with `p` at most 18 |
| By columns, refs, dim keys, attributes and `count(DISTINCT)` inputs | an integer type, `numeric(p, s)`, `boolean` or `text` |
| The fact's key | any |

A ref's column and the dim's key must be of the same kind, and a filter's
literal must fit its column.

## What a spec can't say

No `WHERE` (filter measures instead), no expressions, no joins other than
refs, no subqueries, no window functions, no `HAVING`, no `ROLLUP`, `CUBE`
or `GROUPING SETS`. An aggregate a spec can't declare stays a query to
PostgreSQL.

## Checking a spec

`straum plan --spec FILE` reads the spec and prints the setup SQL, or
what is wrong with it and exits 1. Syntax outside the subset comes first,
with its line:

```text
straum: orders.sql: line 2: WHERE, filter measures instead, is not in straum SQL, the PostgreSQL subset straum specs use
```

Then every problem between the declarations, all at once:

```text
straum: orders.sql: orders: orders declared twice
orders.sql: fact refunds: grain of 420s does not divide a day
```

Problems with the tables themselves, such as a missing column or a wrong
type, show up when `straum run` starts. See [Setting up
PostgreSQL](postgresql.md#what-straum-checks-at-start).

## Memory

Straum keeps one cell per distinct combination of by columns, ref ids and
bucket, so memory grows with the number of cells. A spec whose key columns are
nearly unique per row, such as both a pickup and a dropoff zone at 15
minutes, keeps nearly a cell per row. Fewer key columns or wider buckets
collapse more rows into each cell. `count(DISTINCT c)` keeps an entry per
distinct value in each cell.
