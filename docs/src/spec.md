# Writing a spec

A spec is a file of `CREATE VIEW` statements, one per fact, in a small
subset of PostgreSQL's syntax. Straum keeps one rollup per view and
builds it itself. Nothing in the spec runs in PostgreSQL: Straum never
creates the view, and to load it copies only the columns the view reads.
Anything outside the subset is an error that names the construct.

This spec, from the [benchmark](benchmark.md), uses all of the subset:

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

## View and fact table

The view's name, `trips`, is the fact's name, which asks give in
`"fact"`. `FROM public.yellow_trips f` is the fact table.

- `key` is required: the fact table's primary key. It is never part of
  the rollup, so it can't be grouped by.
- `buckets` is optional: wider bucket widths, under [Time](#time).

Every column is qualified with its table's alias. Identifiers are
unquoted: lowercase letters, digits and `_`, at most 63 bytes.

## Time

Every view has exactly one bucket:

```sql
date_bin('15 minutes', f.pickup_at, timestamptz 'epoch')
```

The column is a `timestamptz`. The width is `<n> seconds`, `minutes`,
`hours` or `days`, at most `1 day`, and divides a day: `15 minutes` does,
`7 minutes` doesn't. The origin is always `timestamptz 'epoch'`, so
buckets start on UTC midnight. The alias is free; asks call it `bucket`,
so no column, ref or measure may be named `bucket`.

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

- The fact column, `f.pu_location_id`, must be selected.
- Joins go from the fact to a dim only, so a spec can't fan out.
- Two refs can share a dim table, like `pickup` and `dropoff`. A dim table
  is one dim however many views join it, and all must join it on the same
  key.
- The join is `LEFT` because a fact row with no dim row still counts. Its
  attributes are null.

Asks group and filter by all three: `payment_type`, `pickup` and
`pickup.borough`. Straum looks attributes up when it answers, so a change
to a dim row never touches a rollup.

## Measures

| Measure | Input column | Answer |
| --- | --- | --- |
| `count(*)` | | an integer |
| `count(DISTINCT c)` | an integer type, `numeric(p, s)`, `boolean` or `text` | an integer |
| `sum(c)`, `min(c)`, `max(c)` | an integer type, or `numeric(p, s)` with `p` at most 18 | an exact decimal in `c`'s scale |
| `avg(c)` | the same | `{"sum": ..., "count": ...}` |

Nulls are skipped, as in SQL. Sums are exact: Straum adds integers, with
numeric values scaled by their column's scale.

`FILTER` makes a measure count only some rows:

```sql
count(*) FILTER (WHERE f.payment_type = 1) AS card
```

The condition is `=` or `IN (...)` on by columns, refs or attributes,
joined by `AND`. Literals are integers, `'text'`, `true` or `false`, and
must fit their column.

By columns, refs, dim keys and attributes are an integer type,
`numeric(p, s)`, `boolean` or `text`. A ref's column and its dim's key
must be of the same kind. The key can be any type.

`GROUP BY` names exactly the selected columns and the bucket, by position
or by expression, so the view groups the way the rollup does.

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
combination of by columns, ref ids and bucket, so per width:

```text
cells <= min(rows, distinct key combinations x buckets)
```

Keys that are nearly unique per row keep nearly a cell per row. In the
benchmark, pickup and dropoff zone at 15 minutes keep 2,322,783 cells for
2,964,624 trips; without the dropoff zone, 568,612. Fewer key columns or
wider buckets collapse more rows into each cell. `count(DISTINCT c)` adds
an entry per distinct value in each cell.
