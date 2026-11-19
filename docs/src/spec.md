# Spec

A spec is a file of `CREATE VIEW` statements, one per table you
aggregate, in a small subset of PostgreSQL's syntax. One item is
Straum's own: `buckets(...)`, the view's time, which PostgreSQL writes
as `date_bin` ([Time](#time)). With that item written so, PostgreSQL
gives the same rows for each view. Straum never creates the view, and
nothing in the spec runs in PostgreSQL: to load a view, Straum copies
only the columns it reads.

Straum SQL has one spelling per meaning and infers nothing
([ADR-0004](https://github.com/janbjorge/straum/blob/main/docs/adr/0004-specs-are-a-strict-postgresql-create-view-subset.md)).
The synopsis is the whole language: anything it does not show is an
error that names the construct and says what to write.

## Synopsis

```text
CREATE VIEW view AS
SELECT time AS name,
       key AS name, ...
       measure AS name, ...
FROM schema.table AS alias
[ INNER JOIN | LEFT OUTER JOIN
    schema.table AS alias ON alias.column = alias.column [AND ...] ] ...
[ WHERE condition [AND ...] ]
GROUP BY position, ...

time       buckets(alias.column, 'width' [, 'width' ...])

key        alias.column
           alias.column::text
           (alias.column IS [NOT] NULL)

measure    count(*)
           count([DISTINCT] alias.column)
           sum([DISTINCT] alias.column)    min(alias.column)
           max(alias.column)       avg([DISTINCT] alias.column)
           bool_and(alias.column)  bool_or(alias.column)
           var_samp(alias.column)  var_pop(alias.column)
           stddev_samp(alias.column)  stddev_pop(alias.column)
           percentile_disc(fraction) WITHIN GROUP (ORDER BY alias.column)
           mode() WITHIN GROUP (ORDER BY alias.column)
           each of these [FILTER (WHERE condition [AND ...])]

condition  alias.column = literal
           alias.column IN (literal, literal, ...)
           alias.column IS [NOT] NULL
           [NOT] EXISTS (SELECT 1 FROM schema.table AS alias
                         WHERE alias.column = alias.column [AND ...])

width      15 minutes, 1 hour, 1 day: seconds, minutes, hours or days,
           in the largest unit that divides it, singular for one
literal    42, -7, 0, 'text', true, false: an integer with no leading
           zero, and 0 with no sign
fraction   0, 0.5, 0.99, 1: digits from 0 to 1, at most 19 past the point,
           written shortest: one digit before the point, no zero ending it
```

Brackets hold what may be left out, `|` separates a choice, and `...`
repeats what is before it. What the synopsis can't show:

- A spec is one or more views, with `;` between them, and never empty.
  `CREATE OR REPLACE VIEW` and view options such as `WITH (...)` are
  refused.
- A view has one time, at least one measure, and any number of keys,
  in any order.
- The time, a measure and a null-ness key read a column of the fact
  table. A key or a condition may read a dim column too.
- `IN` lists two values or more; for one, write `=`.
- `EXISTS` goes in the view's `WHERE` only, not in a `FILTER`.
- A parenthesis is written where the synopsis has one and nowhere
  else: around a null-ness key, never around a condition or a column.
- Identifiers are unquoted and lowercase: a letter or `_`, then
  letters, digits and `_`, at most 63 bytes. Function names too:
  `count`, not `COUNT`. Keywords such as `SELECT`, `AS` and `GROUP BY`
  take any case.
- Each number and width has one spelling: `GROUP BY 1`, never `01`;
  `'1 hour'`, never `'60 minutes'`.
- The name of the time, a key or a measure is one an ask can say, so
  not a word SQL reserves, such as `user`, `all` or `from`:

  ```text
  line 3: user is a word SQL reserves, so no ask could name it; name it otherwise
  ```

A view over an `events` table and the `accounts` it belongs to, with
a null-ness key and two filtered measures:

```sql
CREATE VIEW usage AS
SELECT buckets(e.at, '1 hour', '1 day', '7 days') AS bucket,
       e.kind AS kind,
       e.account_id AS account_id,
       a.region AS region,
       (e.device_id IS NOT NULL) AS has_device,
       count(*) AS events,
       sum(e.amount) AS amount,
       avg(e.amount) AS avg_amount,
       max(e.amount) AS top_amount,
       percentile_disc(0.5) WITHIN GROUP (ORDER BY e.amount) AS median_amount,
       percentile_disc(0.99) WITHIN GROUP (ORDER BY e.amount) AS p99_amount,
       count(*) FILTER (WHERE e.kind = 'purchase') AS purchases,
       count(*) FILTER (WHERE e.device_id IS NOT NULL) AS from_devices
FROM public.events AS e
LEFT OUTER JOIN public.accounts AS a ON a.id = e.account_id
GROUP BY 1, 2, 3, 4, 5;
```

Straum keeps one cell per distinct hour, `kind`, `account_id` and
`has_device`, holding what the measures need. `region` is read from
the account's row in `accounts` when an ask runs. An ask merges cells
and never reads a row.

## The tables

`FROM` names the fact table, the table whose rows the view counts. A
table the view joins is a dim. `straum plan` and the setup check call
them fact and dim.

Each name goes through three layers, and each layer uses only its own:

| Where | Written | Example |
| --- | --- | --- |
| `FROM`, `JOIN`, `EXISTS` | `schema.table AS alias`, always both | `public.events AS e` |
| The rest of the view | `alias.column` | `e.amount`, `a.region` |
| Asks | the `AS name` of a key or measure | `region`, `amount` |

The schema is never left out, even for `public`: Straum has no search
path, and `FROM events` is refused with `table events must be
schema.table`. The alias is never left out either, so every column says
which table it is from. Setup SQL and start checks print tables as
`public.events`, the way PostgreSQL resolves them.

The fact table needs no primary key. On the logical stream the old
row of an update or delete comes from `REPLICA IDENTITY FULL`, which
the [setup SQL](postgresql.md#1-run-the-setup-sql) sets on every spec
table. `--wal physical` rebuilds that row from the page and does not
set `FULL`.

## Keys

Every `SELECT` item that is not a measure is a key: a column of the
fact table (`e.kind`), a column of a dim (`a.region`), a
[null-ness key](#null-ness-keys) or the [time](#time). Each cell is
one distinct combination of the keys read from the fact table, the
[join columns](#joining-a-dim) and the bucket. A dim column is not part
of the cell: an ask reads it through the join
([Dims](how-it-works.md#dims)).

The `AS name` is what asks use, and it is the only way an ask reaches
the item: it says `region`, never `a.region`. Names are
unique within a view. `GROUP BY` lists the keys by their positions in
`SELECT`, in order: a position is what PostgreSQL resolves without a
lookup, where an output name it may read as an input column.

A key column is an integer type, `numeric(p, s)`, `boolean`, `text`,
`varchar`, `date` or `uuid`, or an [enum written
`::text`](#enum-columns). Wherever the view reads a column:

- A `numeric` column is declared `numeric(p, s)` with `p` at most 18.
  A bare `numeric` has no fixed scale and is refused.
- A `real` or `double precision` column is refused: a float's sums
  can't equal PostgreSQL's exactly ([Aggregates 101](aggregates.md)).
- A `character(n)` column is refused as a key, a join column or the
  column of `count(DISTINCT ...)`: PostgreSQL compares it without its
  trailing spaces, so `'ab'` finds `'ab   '`, and Straum compares text
  whole.

Fewer keys, or keys with fewer values, mean fewer cells, and memory
follows cells ([Cells](how-it-works.md#cells)).

## Time

Every view has exactly one time, a `buckets` item:

```sql
buckets(e.at, '1 hour', '1 day', '7 days') AS bucket
```

- The column is a `timestamptz` or a `timestamp` of the fact table.
- Then every bucket width Straum keeps, narrowest first, each its own
  string. Each row goes in a bucket of the first. Each wider width is a
  whole multiple of the one before.
- The first width is at most `1 day` and divides a day: `15 minutes`
  does, `7 minutes` doesn't.
- The name is the view's own: `bucket` here, `pickup` or `hour` just as
  well. Asks group and bound the time by it, and no key or measure of
  the view may share it.
- A `timestamp` has no zone, and Straum never gives it one. Its buckets
  and ask bounds are wall clock, and answers print them with a `Z` that
  means nothing.
- A row whose time is null, `infinity` or `-infinity` is kept but never
  answered. Any other value Straum can't read, such as a `NaN`, stops
  the load or the stream with the table and column named.

In PostgreSQL the item is `date_bin` at the first width, from the
epoch of the column's type:

```sql
date_bin('1 hour', e.at, timestamptz 'epoch') AS bucket
```

A spec writes `buckets`; `date_bin` in it is refused, naming the
`buckets` item to write.

An ask over a long range reads its middle from the widest buckets that
fit and its ends from narrower ones, so it reads far fewer cells, as
drawn under [How Straum works](how-it-works.md#answering-an-ask).
Answers stay the same. A wider width also lets an ask answer [a row per
day or week](asks.md#rows-a-day-or-a-week-apart), one stored bucket a
row. Each width costs memory.

Every width is a fixed grid of buckets counted from the epoch,
1970-01-01 00:00 UTC, whatever the data and whenever the ask. A width
that divides a day starts a bucket on every UTC midnight, so `1 day` is
a UTC day. A wider one starts wherever the count from the epoch lands.
The epoch was a Thursday, so `7 days` buckets run Thursday to Thursday,
not as calendar weeks, and `2 days` buckets start on 2023-12-31 and
2024-01-02, never on 2024-01-01. January 2024 in `7 days` buckets:

```text
2023-12-28T00:00:00Z to 2024-01-04T00:00:00Z
2024-01-04T00:00:00Z to 2024-01-11T00:00:00Z
2024-01-11T00:00:00Z to 2024-01-18T00:00:00Z
2024-01-18T00:00:00Z to 2024-01-25T00:00:00Z
2024-01-25T00:00:00Z to 2024-02-01T00:00:00Z
```

PostgreSQL's `date_bin('7 days', e.at, timestamptz 'epoch')` gives the
same buckets. Weeks from Monday would need another origin, and a month
or a local day is no fixed number of seconds, so neither is a width.

## Joining a dim

```sql
INNER JOIN public.accounts AS a ON a.id = e.account_id
LEFT OUTER JOIN public.accounts AS referrer ON referrer.id = e.referrer_id
```

A join points from a column of the fact, or of a dim already joined, to
the key of a dim. The dim columns the view selects, here
`a.region AS region`, are keys like any other, read as the dim is now:
an account moved from `eu` to `us` moves all its events to `us` in the
next answer, as PostgreSQL's view would, and nothing stops
([Dims](how-it-works.md#dims)).

- `INNER JOIN` keeps a fact row only while a dim row matches it;
  `LEFT OUTER JOIN` keeps every fact row, with null dim columns while
  none matches. A `RIGHT` or `FULL` join would add dim rows no fact row
  points at, whose bucket is null and which no ask reads, so it answers
  as `INNER` or `LEFT OUTER`; write that.
- An `ON` equality has the joined table's column on the left. Its right
  side names the fact's alias or the alias of a dim joined before it, a
  chain:

  ```sql
  INNER JOIN public.accounts AS a ON a.id = e.account_id
  LEFT OUTER JOIN public.regions AS r ON r.name = a.region
  ```

  Straum then holds `region` in its projection of `accounts` and reads
  `regions` through it. A chain through a semi or anti join is
  refused: those keep no row to read through.
- Any number of dim rows may match a dim key. A dim key the dim holds
  twice counts the fact row twice, as PostgreSQL's join does: `count(*)`,
  `count(c)` and `sum` double, `min`, `max`, `count(DISTINCT ...)`,
  `sum(DISTINCT ...)`, `avg(DISTINCT ...)`, `bool_and` and `bool_or`
  don't, `avg` stays, and `percentile_disc`,
  `mode`, the variances and the standard deviations count the row's
  value twice. A dim needs no index on its key.
- The column joined on and the dim key column must be of the same
  type, column for column.
- Under `INNER JOIN` a dim key column, `a.id`, reads the fact column
  joined to it, `e.account_id`, wherever it is written; in a chain,
  `r.name` reads `a.region`. Under `LEFT OUTER JOIN` it is null where
  no dim row matches and the column joined to it is not, so Straum
  refuses it as a key, a measure's column, a filter or the near side of
  a chain: write the column joined to it, or join with `INNER JOIN`.
- A join must select at least one dim column other than its key
  columns, which read the column joined to them. A column may be
  selected under two names, each a key. The fact column joined on,
  `e.account_id`, may be selected too or left out; either way it
  keys the cells, so a view with a join has one cell per distinct join
  column, bucket and other keys, and a dim with many rows means many
  cells.
- Two joins can point at one dim table, each reading its own columns,
  as `a` and `referrer` do above. A dim table is one dim however
  many views join it, and all must join it on the same key columns.
  A dim goes by its table's name without the schema, in one namespace
  with the views: `crm.accounts` and `billing.accounts` can't both be
  dims, and no view can be named `accounts` beside them.
- The join itself has no value: asks group and filter by `region` or
  `account_id`, never by `a`.

## Semi and anti joins

```sql
WHERE EXISTS (SELECT 1 FROM public.accounts AS known WHERE known.id = e.account_id)
```

`EXISTS` keeps a fact row while at least one dim row matches it, once
however many match: here an event whose account is in `accounts`.
`NOT EXISTS` keeps it while none does. Its
equalities are those of `ON`. No column of the subquery's table can be
selected, filtered on or joined through. Both follow the dim as it is
now, like any join.

## The view's WHERE

```sql
WHERE e.kind IN ('purchase', 'refund') AND a.region = 'eu'
```

`WHERE` drops rows from every answer, as the view's `WHERE` does in
PostgreSQL. Its conditions are those of a `FILTER`, below, on columns
the view selects as keys, so a condition on a dim column follows the
dim. A `FILTER` that every measure repeats is refused: write it here.

## Null-ness keys

```sql
(e.device_id IS NOT NULL) AS has_device
```

A null-ness key is a boolean, true for the rows with a `device_id`,
grouped and filtered like a column under its name. `IS NULL` makes the
opposite key. The column may be of any type Straum reads, a time
included, but not the view's time column: a row with no time is never
answered, so the key could never be true. The view body stays what
PostgreSQL runs, since the key's name is no column there.

## Enum columns

```sql
e.kind::text AS kind
```

Had `kind` been an enum, the view would read it so: as its label, the
text PostgreSQL prints for it.

- `::text` goes wherever the view reads the column: on a key, in a
  null-ness key, `(e.kind::text IS NULL)`, in a `FILTER` and in
  `WHERE`, spelled alike in every place and in every view that reads
  the column.
- It is the one cast, and it goes nowhere else: not in `ON`, `EXISTS`,
  `buckets` or a measure. Straum refuses an enum column written
  without it, and the cast on a column of any other type, so a spec
  says what it reads. Which types take the cast is listed here; today
  it is enums alone.
- A literal in a `FILTER` or the view's `WHERE` must be one of the
  type's labels, which Straum reads at start. A label added later is
  read as it comes.
- A renamed label stops Straum, since the rows it loaded hold the old
  text. Each reading of the server's WAL, once a second, compares the
  labels, and under `--wal physical` the rename's own commit stops it,
  before any row with the new text.
- The label is a text key: asks filter it with a string, and answers
  sort labels as text, not in the type's order
  ([ADR-0004](https://github.com/janbjorge/straum/blob/main/docs/adr/0004-specs-are-a-strict-postgresql-create-view-subset.md)).

## Measures

| Measure | Input column | Answer |
| --- | --- | --- |
| `count(*)` | | an integer |
| `count(c)` | an integer type, `numeric(p, s)`, `boolean`, `text`, `timestamptz`, `timestamp`, `date` or `uuid` | an integer |
| `count(DISTINCT c)` | an integer type, `numeric(p, s)`, `boolean`, `text`, `date` or `uuid` | an integer |
| `sum(c)`, `min(c)`, `max(c)` | an integer type, or `numeric(p, s)` with `p` at most 18 | an exact decimal in `c`'s scale |
| `avg(c)` | the same | a decimal rounded half to even at four places past `c`'s scale, its trailing zeros dropped |
| `sum(DISTINCT c)`, `avg(DISTINCT c)` | the same | as `sum(c)` and `avg(c)`, of each value once |
| `percentile_disc(f) WITHIN GROUP (ORDER BY c)` | the same | a value of `c`, in `c`'s scale |
| `mode() WITHIN GROUP (ORDER BY c)` | the same | a value of `c`, in `c`'s scale |
| `var_samp(c)`, `var_pop(c)` | the same | a decimal rounded half to even at four places past twice `c`'s scale |
| `stddev_samp(c)`, `stddev_pop(c)` | the same | a decimal rounded half to even at four places past `c`'s scale |
| `bool_and(c)`, `bool_or(c)` | `boolean` | `true` or `false` |

A measure's column is one of the fact table. Nulls are skipped, as in
SQL. Sums are exact: Straum adds integers, with
numeric values scaled by their column's scale. `count(*)` counts rows
and `count(c)` the rows where `c` is not null. `bool_and(c)` is true
when every `c` that is not null is true, and `bool_or(c)` when any is;
both are null when no `c` is
([ADR-0003](https://github.com/janbjorge/straum/blob/main/docs/adr/0003-straum-holds-cells-and-nothing-else.md)).
`percentile_disc(f)` is the value at rank `ceil(f * n)` of the `n`
values that are not null, at least the first, and null when there are
none; ascending is its one order, so `ASC` and `DESC` are not written
([Median and percentiles](aggregates.md#median-and-percentiles)).
`mode()` is the value the most rows hold, the least of those tied, as
PostgreSQL takes the first in order; null when no `c` is.
`var_samp(c)` is the sum of the squared distances of the values from
their mean, over `n - 1`; `var_pop(c)` the same over `n`.
`stddev_samp(c)` and `stddev_pop(c)` are their square roots. All four
are null when no `c` is, and the two `_samp` ones also for a single
value. A variance is in `c`'s units squared, so its scale is twice
`c`'s: of a `numeric(10, 2)` it prints eight places
([Variance and standard deviation](aggregates.md#variance-and-standard-deviation)).

`percentile_disc` takes one fraction, as the synopsis writes it, and
`mode()` none; both take `ORDER BY alias.column` alone: no array,
`DESC`, `USING`, `NULLS` or expression. `percentile_cont` and the
hypothetical-set aggregates are refused. So are `variance` and
`stddev`, PostgreSQL's other names for `var_samp` and `stddev_samp`:
write those.

`FILTER` makes a measure count only some rows:

```sql
count(*) FILTER (WHERE e.kind = 'purchase') AS purchases
count(*) FILTER (WHERE e.device_id IS NOT NULL) AS from_devices
```

`=` and `IN` take a column the view selects as a key, `IS NULL` and
`IS NOT NULL` a column with a null-ness key. The second filter is why
the view needs the `has_device` key: Straum keeps nothing else about
`device_id`. A condition comes once in a `FILTER` or the view's
`WHERE`: written twice, it is refused by name. A literal must fit its
column. A `date` or `uuid` literal is a string spelled as PostgreSQL
prints the value, and only so:
`'2024-01-15'`, `'0044-03-15 BC'` or `'infinity'` for a date,
lowercase hex in groups of 8-4-4-4-12 for a uuid, such as
`'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'`. A date or a uuid is a key,
never a `sum`, `min` or `max`.

## Checking a spec

`straum plan FILE` prints the setup SQL, or what is wrong and exits
with status 1. Each line of the report names the file. Straum reads
the spec in two passes and reports every problem from the first pass
that finds any. The first pass reads each statement and names the line
of each problem, such as syntax outside the subset or an unknown
alias. It lists the first 50, then how many more:

```text
straum: usage.sql: line 6: JOIN, write INNER JOIN, `JOIN public.accounts AS a ON a.id = e.account_id` is not in Straum SQL, the PostgreSQL subset made only for Straum
straum: usage.sql: line 3: unknown table alias a
```

The second pass checks the views against each other and against their
own rules, such as names declared twice and widths:

```text
straum: usage.sql: usage declared twice: views and dims share one namespace, and a dim goes by its table's name without the schema
straum: usage.sql: view usage: width of 7 minutes does not divide a day
```

`straum run` checks the tables themselves, such as a missing column or a
wrong type, [when it starts](postgresql.md#what-straum-checks-at-start).
