# Spec

A spec is a file of `CREATE VIEW` statements, one per table you
aggregate, in a small subset of PostgreSQL's syntax. The one item of
Straum's own is `buckets(...)`, the view's time ([Time](#time)).
Written as `date_bin` instead, each view gives the same rows in
PostgreSQL. Nothing in the spec runs in PostgreSQL:
Straum never creates the view, and loads it by copying only the
columns it reads.

Straum SQL has one spelling per meaning and infers nothing
([architecture decision record
ADR-0004](https://github.com/janbjorge/straum/blob/main/docs/adr/0004-specs-are-a-strict-postgresql-create-view-subset.md)).
Anything the synopsis does not show is an error that names the
construct and says what to write.

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
           buckets(alias.column, keep('width', 'duration')
                   [, keep('width', 'duration') ...], 'width' [, 'width' ...])

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

width      15 minutes, 1 hour, 1 day: a count above 0 of seconds,
           minutes, hours or days, in the largest unit that divides
           it, singular for one
duration   1 day, 14 days: written as a width is
literal    42, -7, 0, 'text', true, false: an integer with no leading
           zero, and 0 with no sign
fraction   0, 0.5, 0.99, 1: digits from 0 to 1, at most 19 past the point,
           written shortest: one digit before the point, no zero ending it
```

What the synopsis can't show:

| Of | Rule |
| --- | --- |
| A spec | One or more views, `;` between them, never empty. `CREATE OR REPLACE VIEW` and view options such as `WITH (...)` are refused. |
| A view | One time, at least one measure, any number of keys, in any order. |
| Columns | The time, a measure and a null-ness key read the fact table; a key or a condition may read a dim too. |
| `keep` | Only in a view's `buckets`, on the narrowest widths and never the widest; its rules are under [Keeping a width for a time](#keeping-a-width-for-a-time). |
| `IN` | Two values or more; for one, write `=`. |
| `EXISTS` | In the view's `WHERE` only, not in a `FILTER`. |
| Parentheses | Only where the synopsis has them: around a null-ness key, never around a condition or a column. |
| Operators | `=` for equal: PostgreSQL has no `==`. `IS NOT NULL`, never `NOTNULL`, PostgreSQL's second spelling. |
| Identifiers | Unquoted and lowercase: a letter or `_`, then letters, digits and `_`, at most 63 bytes. Function names too: `count`, not `COUNT`. Keywords such as `SELECT`, `AS` and `GROUP BY` take any case. |
| Numbers, widths | One spelling each: `GROUP BY 1`, never `01`; `'1 hour'`, never `'60 minutes'`. |
| Names | The time, a key or a measure needs a name an ask can say, so not a word SQL reserves, such as `user`, `all` or `from`. |

```text
line 3: user is a word SQL reserves, so no ask could name it; name it otherwise
```

A view over `events` and their `accounts`, with a null-ness key and
two filtered measures:

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
`has_device`, holding what the measures need. An ask reads `region`
from the account's row in `accounts`, merges cells and never reads a
row.

## The tables

`FROM` names the fact table, whose rows the view counts; a table it
joins is a dim. `straum plan` and the setup check use both words.
Each layer uses only its own names:

| Where | Written | Example |
| --- | --- | --- |
| `FROM`, `JOIN`, `EXISTS` | `schema.table AS alias`, always both | `public.events AS e` |
| The rest of the view | `alias.column` | `e.amount`, `a.region` |
| Asks | the `AS name` of a key or measure | `region`, `amount` |

Neither the schema nor the alias is ever left out, not even for
`public`: Straum has no search path, and `FROM events` is refused with
`table events must be schema.table`. Setup SQL and start checks print
tables as `public.events`, as PostgreSQL resolves them.

The fact table needs no primary key. On the logical stream the old
row of an update or delete comes from `REPLICA IDENTITY FULL`, which
the [setup SQL](postgresql.md#1-run-the-setup-sql) sets on every spec
table. `--wal physical` rebuilds that row from the page and does not
set `FULL`.

## Keys

Every `SELECT` item that is not a measure is a key: a fact column
(`e.kind`), a dim column (`a.region`), a
[null-ness key](#null-ness-keys) or the [time](#time). A cell is one
distinct combination of the fact table's keys, the
[join columns](#joining-a-dim) and the bucket. A dim column is not
part of the cell: an ask reads it through the join
([Dims](how-it-works.md#dims)).

An ask reaches an item only by its `AS name`: `region`, never
`a.region`. Names are unique within a view. `GROUP BY` lists the keys
by their positions in `SELECT`, in order.

A key column is an integer type, `numeric(p, s)`, `boolean`, `text`,
`varchar`, `date` or `uuid`, or an [enum written
`::text`](#enum-columns). Wherever the view reads a column:

| Column | Rule |
| --- | --- |
| `numeric` | Declared `numeric(p, s)` with `p` at most 18. A bare `numeric` has no fixed scale and is refused. |
| `real`, `double precision` | Refused: a float's sums can't equal PostgreSQL's exactly ([Aggregates 101](aggregates.md)). |
| `character(n)` | Refused as a key, a join column or the column of `count(DISTINCT ...)`: PostgreSQL compares it without its trailing spaces, so `'ab'` finds `'ab   '`, and Straum compares text whole. |

Fewer keys, or keys with fewer values, mean fewer cells, and memory
follows cells ([Cells](how-it-works.md#cells)).

## Time

Every view has exactly one time, a `buckets` item:

```sql
buckets(e.at, '1 hour', '1 day', '7 days') AS bucket
```

| Part | Rule |
| --- | --- |
| Column | A `timestamptz` or `timestamp` of the fact table. A `timestamp` has no zone and Straum never gives it one: its buckets and ask bounds are wall clock, and answers print them with a `Z` that means nothing. |
| Widths | Every bucket width Straum keeps, narrowest first, each its own string. Each row goes in a bucket of the first, which is at most `1 day` and divides a day: `15 minutes` does, `7 minutes` doesn't. Each wider width is a whole multiple of the one before. |
| Name | The view's own: `bucket` here, `pickup` or `hour` just as well. Asks group and bound the time by it; no key or measure of the view may share it. |

A row whose time is null, `infinity` or `-infinity` is kept but never
answered. Any other value Straum can't read, such as a `NaN`, stops
the load or the stream, naming the table and column.

In PostgreSQL the item is `date_bin` at the first width, from the
epoch of the column's type:

```sql
date_bin('1 hour', e.at, timestamptz 'epoch') AS bucket
```

`date_bin` in a spec is refused, naming the `buckets` item to write.

An ask over a long range reads its middle from the widest buckets that
fit and its ends from narrower ones, so it reads far fewer cells and
gets the same answer
([How Straum works](how-it-works.md#answering-an-ask)). A wider width
also lets an ask answer [a row per day or
week](asks.md#rows-a-day-or-a-week-apart), one stored bucket a row.
Each width costs memory.

Every width is a fixed grid counted from the epoch, 1970-01-01 00:00
UTC, whatever the data and the ask. A width that divides a day starts
a bucket on every UTC midnight, so `1 day` is a UTC day. A wider one
starts wherever the count lands. The epoch was a Thursday, so `7 days`
buckets run Thursday to Thursday, not as calendar weeks, and `2 days`
buckets start on 2023-12-31 and 2024-01-02, never on 2024-01-01.
January 2024 in `7 days` buckets:

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

### Keeping a width for a time

```sql
buckets(e.at, keep('1 hour', '14 days'), '1 day', '7 days') AS bucket
```

`keep('width', 'duration')` in place of a width keeps that width's
buckets only for the duration; every width written bare keeps every
bucket. Here hours are kept for 14 days, days and weeks for good. Both
strings are spelled as widths are: `14 days`, never `2 weeks` or
`336 hours`. `keep` goes nowhere else, and an ask never writes it.

The narrowest width holds most of the cells, and memory follows
cells, so keeping it for a recent window bounds memory by time rather
than by the tables' size, with nothing kept on disk. PostgreSQL still
holds every row: a restart loads them all again, and a row before a
horizon makes no bucket of the width kept for a time.

| Rule | Error from `straum plan` |
| --- | --- |
| The widest width keeps every bucket. | `view usage: 7 days is the widest width and keeps every bucket; only a narrower width can be kept for a time` |
| The duration is a whole multiple of the next wider width. | `view usage: 1 hour is kept for 36 hours, not a whole multiple of 1 day, the next wider width; keep it for 1 day or a multiple of it` |
| The widths kept for a time run on from the narrowest. | `view usage: 1 day is kept for a time but a narrower width is not; keep every narrower width for a time too` |
| A wider width is kept no shorter than a narrower one. | `view usage: 1 day is kept for 7 days, less than the 14 days a narrower width is kept for; keep each width at least as long as the narrower ones` |

Each width kept for a time has a horizon: PostgreSQL's time less the
duration, floored to the next wider width, so the horizon is always an
edge of the wider buckets. PostgreSQL's time is the snapshot's `now()`
when the tables load, then each streamed commit's time, before its
changes apply. With `now()` at 2024-03-15T05:47:13Z, the hours above
are kept from 2024-03-01T00:00:00Z. The horizon never moves back, and
a database that commits nothing moves none.

The horizon follows the server's clock, not the times in the rows. A
table of past data, loaded today under `keep('1 hour', '14 days')`,
holds no hour of it at all: every row is older than 14 days before
`now()`.

A `timestamp` has no zone, so the horizon reads the rows' wall clock as
UTC. Straum then reads rows written in a zone west of UTC, such as
`America/New_York`, as older by that zone's offset. A width kept for a
time can drop their buckets up to that many hours early. To keep the
whole duration, add the offset and round up to a whole multiple of the
next wider width. A `timestamptz` is an instant and has no such offset.

Buckets before the horizon are dropped and never made again. A later
insert, update or delete of an older row changes the wider widths, and
they stay exact. A row with a null time is never dropped. An ask that
would read a width's buckets before its horizon is refused, naming the
width and the time its buckets are kept from ([Widths kept for a
time](asks.md#widths-kept-for-a-time)).

In PostgreSQL the item is still `date_bin` at the first width, the
width alone when `keep` wraps it.

## Joining a dim

```sql
INNER JOIN public.accounts AS a ON a.id = e.account_id
LEFT OUTER JOIN public.accounts AS referrer ON referrer.id = e.referrer_id
```

A join points from a column of the fact, or of a dim already joined,
to the key of a dim. The dim columns the view selects, here
`a.region AS region`, are keys read as the dim is now: an account
moved from `eu` to `us` moves all its events to `us` in the next
answer, as PostgreSQL's view would, and Straum keeps running
([Dims](how-it-works.md#dims)). The join itself has no value: asks
group and filter by `region` or `account_id`, never by `a`.

A `RIGHT` or `FULL` join is refused. It would add dim rows no fact
row points at, whose bucket is null and which no ask reads, so it
answers as `INNER` or `LEFT OUTER`; write that.

An `ON` equality has the joined table's column on the left, of the
same type as the column it joins, column for column. Its right side
names the fact's alias or that of a dim joined before it, a chain:

```sql
INNER JOIN public.accounts AS a ON a.id = e.account_id
LEFT OUTER JOIN public.regions AS r ON r.name = a.region
```

Straum then holds `region` in its projection of `accounts` and reads
`regions` through it. A chain through a semi or anti join is refused:
those keep no row to read through.

Under `INNER JOIN` a dim key column, `a.id`, reads the fact column
joined to it, `e.account_id`, wherever it is written; in a chain,
`r.name` reads `a.region`. Under `LEFT OUTER JOIN` it is null where no
dim row matches and the column joined to it is not, so Straum refuses
it as a key, a measure's column, a filter or the near side of a chain:
write the column joined to it, or join with `INNER JOIN`.

A join selects at least one dim column besides its key columns. A
column may be selected under two names, each a key. The fact column
joined on, `e.account_id`, may be selected or not; either way it keys
the cells, so a view with a join has one cell per distinct join
column, bucket and other keys, and a dim with many rows means many
cells.

A dim needs no index on its key, and any number of dim rows may match
one. A dim key the dim holds twice counts the fact row twice, as
PostgreSQL's join does:

| Measures | Under a doubled row |
| --- | --- |
| `count(*)`, `count(c)`, `sum` | double |
| `min`, `max`, `count(DISTINCT ...)`, `sum(DISTINCT ...)`, `avg(DISTINCT ...)`, `bool_and`, `bool_or` | stay |
| `avg`, `percentile_disc`, `mode`, the variances, the standard deviations | count the row's value twice |

Two joins can point at one dim table, each reading its own columns, as
`a` and `referrer` do above. A dim table is one dim however many views
join it, and all must join it on the same key columns. A dim goes by
its table's name without the schema, in one namespace with the views:
`crm.accounts` and `billing.accounts` can't both be dims, and no view
can be named `accounts` beside them.

## Semi and anti joins

```sql
WHERE EXISTS (SELECT 1 FROM public.accounts AS known WHERE known.id = e.account_id)
```

`EXISTS` and `NOT EXISTS` take the equalities of `ON` and follow the
dim as it is now. No column of the subquery's table can be selected,
filtered on or joined through.

## The view's WHERE

```sql
WHERE e.kind IN ('purchase', 'refund') AND a.region = 'eu'
```

The view's conditions are those of a `FILTER`, below, on columns
the view selects as keys, so a condition on a dim column follows the
dim. A `FILTER` on every measure does not do the work of `WHERE`. The
`FILTER` keeps a group that has no matching row, with a count of 0 and
a null sum. `WHERE` removes the row before it groups, so that group is
not in the answer.

## Null-ness keys

```sql
(e.device_id IS NOT NULL) AS has_device
```

A null-ness key is a boolean, grouped and filtered under its name like
a column. Its column may be of any type Straum reads, a time included,
but not the view's time column: a row with no time is never answered,
so the key could never be true. In the view's `WHERE` or a `FILTER`,
write `e.device_id IS NOT NULL`, not `has_device`: PostgreSQL has no
column by the key's name.

## Enum columns

```sql
e.kind::text AS kind
```

Had `kind` been an enum, the view would read it so, as its label: a
text key that asks filter with a string and answers sort as text, not
in the type's order
([ADR-0004](https://github.com/janbjorge/straum/blob/main/docs/adr/0004-specs-are-a-strict-postgresql-create-view-subset.md)).

`::text` goes wherever the view reads the column: on a key, in a
null-ness key, `(e.kind::text IS NULL)`, in a `FILTER` and in `WHERE`,
spelled alike in every place and every view that reads the column. It
is the one cast and goes nowhere else: not in `ON`, `EXISTS`,
`buckets` or a measure. Straum refuses an enum column without it, and
the cast on any other type, so a spec says what it reads. Today only
enums take it.

A literal in a `FILTER` or the view's `WHERE` must be one of the
type's labels, which Straum reads at start; a label added later is
read as it comes. A renamed label stops Straum, since the rows it
loaded hold the old text. Each reading of the server's write-ahead log
(WAL), once a second, compares the labels, and under `--wal physical`
the rename's own commit stops it, before any row with the new text.

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

A measure's column is one of the fact table. Sums are exact: Straum
adds integers, numeric values scaled by their column's scale
([ADR-0003](https://github.com/janbjorge/straum/blob/main/docs/adr/0003-straum-holds-cells-and-nothing-else.md)).
A variance is in `c`'s units squared, so its scale is twice `c`'s: of
a `numeric(10, 2)` it prints eight places
([Variance and standard deviation](aggregates.md#variance-and-standard-deviation)).

`percentile_disc(f)` is the value at rank `ceil(f * n)` of the `n`
non-null values, at least the first, and null when there are none
([Median and percentiles](aggregates.md#median-and-percentiles)).
`mode()` is the value the most rows hold, the least of those tied, as
PostgreSQL takes the first in order; null when no `c` is. Both take
`ORDER BY alias.column` alone, ascending its one order: no `ASC`,
`DESC`, array, `USING`, `NULLS` or expression. `percentile_disc` takes
one fraction as the synopsis writes it, `mode()` none.

Refused: `percentile_cont`, the hypothetical-set aggregates, and
`variance` and `stddev`, PostgreSQL's other names for `var_samp` and
`stddev_samp`: write those.

A measure may take a `FILTER`:

```sql
count(*) FILTER (WHERE e.kind = 'purchase') AS purchases
count(*) FILTER (WHERE e.device_id IS NOT NULL) AS from_devices
```

`=` and `IN` take a column the view selects as a key, `IS NULL` and
`IS NOT NULL` a column with a null-ness key: the second filter is why
the view has `has_device`, since Straum keeps nothing else about
`device_id`. A condition comes once in a `FILTER` or the view's
`WHERE`; written twice, it is refused by name, as is a literal written
twice in one `IN`. A literal must fit its
column. A `date` or `uuid` literal is a string spelled only as
PostgreSQL prints the value: `'2024-01-15'`, `'0044-03-15 BC'` or
`'infinity'` for a date, lowercase hex in groups of 8-4-4-4-12 for a
uuid, such as `'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'`. A date or a
uuid is a key, never a `sum`, `min` or `max`.

## Checking a spec

`straum plan FILE --wal logical` prints the setup SQL, or what is
wrong, each line naming the file, and exits with status 1. Straum
reads the spec in two passes and reports every problem from the first
pass that finds any. The first reads each statement and names the
line of each problem, such as syntax outside the subset or an unknown
alias, listing the first 50, then how many more:

```text
straum: usage.sql: line 6: JOIN, write INNER JOIN, `JOIN public.accounts AS a ON a.id = e.account_id` is not in Straum SQL, the PostgreSQL subset made only for Straum
straum: usage.sql: line 3: unknown table alias a
```

The second checks the views against each other and their own rules,
such as names declared twice and widths:

```text
straum: usage.sql: usage declared twice: views and dims share one namespace, and a dim goes by its table's name without the schema
straum: usage.sql: view usage: width of 7 minutes does not divide a day
```

`straum run` checks the tables themselves, such as a missing column or
a wrong type, [when it starts](postgresql.md#what-straum-checks-at-start).
