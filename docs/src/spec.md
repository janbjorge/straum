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
CREATE VIEW view WITH (buckets = 'width, ...') AS
SELECT key AS name, ...
       measure AS name, ...
FROM schema.table AS alias
[ INNER JOIN | LEFT OUTER JOIN
    schema.table AS alias ON alias.column = alias.column [AND ...] ] ...
[ WHERE condition [AND ...] ]
GROUP BY position, ...

key        date_bin('width', alias.column, timestamptz 'epoch')
           alias.column
           alias.column::text
           (alias.column IS [NOT] NULL)

measure    count(*)
           count([DISTINCT] alias.column)
           sum(alias.column)       min(alias.column)
           max(alias.column)       avg(alias.column)
           bool_and(alias.column)  bool_or(alias.column)
           each of these [FILTER (WHERE condition [AND ...])]

condition  alias.column = literal
           alias.column IN (literal, literal, ...)
           alias.column IS [NOT] NULL
           [NOT] EXISTS (SELECT 1 FROM schema.table AS alias
                         WHERE alias.column = alias.column [AND ...])

width      15 minutes, 1 hour, 1 day: seconds, minutes, hours or days
literal    42, -7, 'text', true, false
```

Brackets hold what may be left out, `|` separates a choice, and `...`
repeats what is before it. Keys and measures come in any order.
`EXISTS` goes in the view's `WHERE` only, not in a `FILTER`. A
parenthesis is written where the synopsis has one and nowhere else:
around a null-ness key, never around a condition or a column. A spec is
one or more views, with `;` between them. Identifiers are unquoted:
lowercase letters, digits and `_`, at most 63 bytes.

A view over an `events` table and the `accounts` it belongs to, with
a null-ness key and two filtered measures:

```sql
CREATE VIEW usage WITH (buckets = '1 hour, 1 day, 7 days') AS
SELECT date_bin('1 hour', e.at, timestamptz 'epoch') AS bucket,
       e.kind AS kind,
       e.account_id AS account_id,
       a.region AS region,
       (e.device_id IS NOT NULL) AS has_device,
       count(*) AS events,
       sum(e.amount) AS amount,
       avg(e.amount) AS avg_amount,
       count(*) FILTER (WHERE e.kind = 'purchase') AS purchases,
       count(*) FILTER (WHERE e.device_id IS NOT NULL) AS from_devices
FROM public.events AS e
LEFT OUTER JOIN public.accounts AS a ON a.id = e.account_id
GROUP BY 1, 2, 3, 4, 5;
```

Straum keeps one cell per distinct hour, kind, account, region and
`has_device`, holding what the measures need. An ask
merges cells and never reads a row.

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

The fact table needs no primary key. Straum takes the old row of an
update or delete from `REPLICA IDENTITY FULL`, which the [setup
SQL](postgresql.md#2-run-the-setup-sql) gives every fact table.

## Keys

Every `SELECT` item that is not a measure is a key, and each cell is
one distinct combination of the keys. A key is a column of the fact
table (`e.kind`), a column of a dim (`a.region`), a
[null-ness key](#null-ness-keys) or the [time bucket](#time).

The `AS name` is what asks use, and it is the only way an ask reaches
the item: it says `region`, never `a.region`. Names are
unique within a view. `GROUP BY` lists the keys by their positions in
`SELECT`, in order: a position is what PostgreSQL resolves without a
lookup, where an output name it may read as an input column.

A key column is an integer type, `numeric(p, s)`, `boolean` or `text`,
or an [enum written `::text`](#enum-columns). Fewer keys, or keys with
fewer values, mean fewer cells, and memory follows cells
([Cells](how-it-works.md#cells)).

## Time

Every view has exactly one bucket, named `bucket`:

```sql
date_bin('1 hour', e.at, timestamptz 'epoch') AS bucket
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

`WITH (buckets = '1 hour, 1 day, 7 days')` lists every bucket width
Straum keeps, narrowest first, and the first must be the `date_bin`
width. Each wider width is a whole multiple of the one before and
starts at the epoch, so `7 days` buckets start on Thursdays. An ask
over a long range reads its middle from the widest buckets that fit
and its ends from narrower ones, so it reads far fewer cells, as drawn
under [How Straum works](how-it-works.md#answering-an-ask). Answers
stay the same, and each width costs memory.

## Joining a dim

```sql
INNER JOIN public.accounts AS a ON a.id = e.account_id
LEFT OUTER JOIN public.accounts AS referrer ON referrer.id = e.referrer_id
```

A join points from a column of the fact, or of a dim already joined, to
the key of a dim. The dim columns the view selects, here
`a.region AS region`, are keys like any other, read as the dim is now:
an account moved from `eu` to `us` moves all its events to `us` in the
next answer, as PostgreSQL's view would, and nothing starts over
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
- Any number of dim rows may match a key. A key the dim holds twice
  counts the fact row twice, as PostgreSQL's join does: `count(*)`,
  `count(c)` and `sum` double, `min`, `max`, `count(DISTINCT ...)`,
  `bool_and` and `bool_or` don't, `avg` stays. A dim needs no index on
  its key.
- The fact column and the dim key column must be of the same kind,
  column for column.
- A join must select at least one dim column. The fact column joined on,
  `e.account_id`, may be selected too or left out; either way it
  keys the cells, so a view with a join has one cell per distinct join
  id, bucket and other keys, and a dim with many rows means many
  cells.
- Two joins can point at one dim table, each reading its own columns,
  as `a` and `referrer` do above. A dim table is one dim however
  many views join it, and all must join it on the same key columns.
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

Had `kind` been an enum, the view would read it so. An enum
column is read as its label, the text PostgreSQL prints for it, by
writing `::text` wherever the view reads the column: on a key, in a
null-ness key, `(e.kind::text IS NULL)`, in a `FILTER` and
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
count(*) FILTER (WHERE e.kind = 'purchase') AS purchases
count(*) FILTER (WHERE e.device_id IS NOT NULL) AS from_devices
```

`=` and `IN` take a column the view selects as a key, `IS NULL` and
`IS NOT NULL` a column with a null-ness key. The second filter is why
the view needs the `has_device` key: Straum keeps nothing else about
`device_id`. A literal must fit its column.

## Checking a spec

`straum plan --spec FILE` prints the setup SQL, or what is wrong and exits
1. Syntax outside the subset comes first, with its line, then every
problem between declarations at once:

```text
straum: usage.sql: line 6: JOIN, write INNER JOIN, `JOIN public.accounts AS a ON a.id = e.account_id` is not in Straum SQL, the PostgreSQL subset made only for Straum
usage.sql: line 3: unknown table alias a
```

```text
straum: usage.sql: usage declared twice
usage.sql: view usage: width of 420s does not divide a day
```

`straum run` checks the tables themselves, such as a missing column or a
wrong type, [when it starts](postgresql.md#what-straum-checks-at-start).
