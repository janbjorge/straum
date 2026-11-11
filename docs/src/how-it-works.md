# How Straum works

Straum loads every fact table from a snapshot at one point in the WAL,
an LSN called L, then applies every commit after L. No commit is missed
between the load and the stream, and none is applied twice.

## Start

![How a start goes from connect to streaming](diagrams/start.svg)

*The writer opens a temporary slot whose snapshot is at L, checks the
catalog and loads every fact table in that snapshot, then streams from
L. A stream thread hands each commit to the writer.*

1. Straum opens a replication connection and creates a temporary logical
   slot with `pgoutput`. PostgreSQL exports a snapshot at the slot's
   consistent point, L.
2. In that snapshot, Straum [checks the
   catalog](postgresql.md#what-straum-checks-at-start) against the spec
   and loads every fact table. Each loads with one binary `COPY` of a
   `SELECT` that joins the fact to its dims and reads only the columns
   the spec names. Straum groups the rows into cells as they come, a
   batch at a time, so it never holds the table.
3. Straum ends the snapshot, swaps the loaded store in and turns ready.
4. It starts the stream at L. A stream thread reads it, answers the
   server's keepalives and confirms what the writer has applied, so the
   server can recycle WAL.

The load fills a new store outside the lock, so asks never wait on
PostgreSQL; they answer 503 until the load is in. Straum drops the old
store before it loads again, so the two never take memory at once.

## Cells

A cell is one distinct combination of a view's keys and bucket. It
holds its row count and what each measure needs:

- `sum` and `avg` share the column's sum and count of non-null values.
- `min` and `max` keep a count per value, so deleting the smallest value
  reveals the next one.
- `count(DISTINCT c)` keeps a count per value.

Each bucket width keeps cells of its own. A fact row with no time goes
to a bucket no ask reads.

## Applying a commit

Changes arrive after their transaction commits. Before Straum applies
one, it reads the dim columns of the fact rows in it: one `SELECT` per
join on a plain second connection, with the keys of every changed row,
as the dims are at that moment. Then it applies the transaction whole:

- An insert into a fact table adds the row to its cell.
- An update takes the old row out of its cell and adds the new one; the
  old row is there because fact tables have `REPLICA IDENTITY FULL`.
- A delete takes the old row out. A cell goes with its last row.
- A truncate empties the view's cells.

Dims are not in the stream. A change to a dim row shows in the fact rows
that come after it and leaves the cells before it as they were, as
[Writing a spec](spec.md#joining-a-lookup-table) describes.

A change Straum can't apply makes it [start over](running.md#states)
from a new slot: an update without its old row, a row that doesn't fit
its cells, a value it can't read, a table whose columns now read
differently, an update or delete of a row whose dim row has changed
since it came in, or a transaction above `--max-transaction-rows`. The
last is on purpose: a transaction waits in memory until its commit and
applies under the write lock, and the new slot's snapshot is after the
big commit, so its rows come in through the bulk `COPY` instead. The
[benchmark](benchmark.md#transactions) sets the default.

## Answering an ask

The [quick start](quick-start.md) after step 7 holds five orders in
four cells. This ask reads paid orders by country over the day:

```sql
SELECT country, orders, revenue FROM orders
WHERE status = $1 AND bucket >= $2 AND bucket < $3
GROUP BY country
```

```text
cells of orders, one per hour, status, customer_id, country

  hour   status    customer_id  country | orders  revenue
  09:00  paid      1            NO      |   1     120.00   -> NO
  09:00  paid      2            SE      |   1      80.50   -> SE
  10:00  refunded  3            NO      |   2      61.99   status is not paid
  11:00  paid      2            SE      |   1     300.00   -> SE

answer: NO 1 120.00, SE 2 380.50
```

1. Straum checks the ask against the spec and refuses any ask it can't
   answer exactly. Once a load has read the column types, it also refuses
   a filter value of another type than its column.
2. It reads the cells of the buckets in the range: the widest the view
   keeps that fit, and narrower ones at the ends.
3. Every key the ask names is part of the cell key, so nothing is looked
   up: a cell's country has sat in its key since the row came in.
4. It adds each cell to its group, and to each measure whose filter holds.
   Filters name only parts of the cell key, so a cell counts whole or
   not at all.

One writer applies the stream while asks read. They share the store
behind a read-write lock, which the writer takes only to apply a commit it
has already read. A long ask delays the writer, and that shows as lag.
