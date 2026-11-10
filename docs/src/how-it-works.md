# How Straum works

Straum loads every fact from a snapshot at one point in the WAL, an LSN
called L, then applies every commit after L. No commit is missed between
the load and the stream, and none is applied twice.

## Start

![The writer thread connects for replication and opens a temporary logical slot whose snapshot is at L. In that snapshot it checks the catalog and loads each fact joined to its dims, then swaps the loaded store in. The stream starts at L, and a stream thread hands each commit to the writer, which applies it under the write lock.](diagrams/start.svg)

1. Straum opens a replication connection and creates a temporary logical
   slot with `pgoutput`. PostgreSQL exports a snapshot at the slot's
   consistent point, L.
2. In that snapshot, Straum [checks the
   catalog](postgresql.md#what-straum-checks-at-start) against the spec
   and loads every fact. Each loads with one binary `COPY` of a `SELECT`
   that joins the fact to its dims and reads only the columns the spec
   names. Straum groups the rows into cells as they come, a batch at a
   time, so it never holds the table.
3. Straum ends the snapshot, swaps the loaded store in and turns ready.
4. It starts the stream at L. A stream thread reads it, answers the
   server's keepalives and confirms what the writer has applied, so the
   server can recycle WAL.

The load fills a new store outside the lock, so asks never wait on
PostgreSQL; they answer 503 until the load is in. Straum drops the old
store before it loads again, so the two never take memory at once.

## Cells

A cell is keyed by its bucket, by columns, null-ness keys and
attributes, and holds its row count and what each measure needs:

- `sum` and `avg` share the column's sum and count of non-null values.
- `min` and `max` keep a count per value, so deleting the smallest value
  reveals the next one.
- `count(DISTINCT c)` keeps a count per value.

Each bucket width keeps cells of its own. A fact row with no time goes
to a bucket no ask reads.

## Applying a commit

Changes arrive after their transaction commits. Before Straum applies
one, it reads the attributes of the fact rows in it: one `SELECT` per ref
on a plain second connection, with the keys of every changed row, as the
dims are at that moment. Then it applies the transaction whole:

- An insert into a fact adds the row to its cell.
- An update takes the old row out of its cell and adds the new one. This
  is why facts need `REPLICA IDENTITY FULL`: without the old row, Straum
  can't know which cell to take it from.
- A delete takes the old row out. A cell goes with its last row.
- A truncate empties the rollup.

Dims are not in the stream. A change to a dim row shows in the fact rows
that come after it and leaves the cells before it as they were, as
[Writing a spec](spec.md#dims-refs-and-attributes) describes.

A change Straum can't apply makes it [start over](running.md#states) from
a new slot: an update without its old row, a row that doesn't fit its
rollup, a table whose columns now read differently, or an update or
delete of a row whose dim row has changed since the row came in, since
its cell is no longer where the dim says.

A transaction that changes more than `--max-transaction-rows` rows of the
spec's facts makes it start over too, on purpose. Straum holds a
transaction's rows until its commit and applies them under the write
lock, so a bulk update of millions of rows would hold the memory for them
and block asks for as long as it takes. The new slot's snapshot is after
the big commit, so its rows come in through the bulk `COPY` load
instead. The [benchmark](benchmark.md#transactions) measures where the
default comes from.

## Answering an ask

![An ask for trips by pickup.borough from 00:00 to 00:30 walks the cells of the trips rollup. It keeps the cells whose bucket is in the range, reads each cell's borough from its key, and adds each cell's rows to the measures whose filter holds. A row that had no dim row when it came in groups under null.](diagrams/ask.svg)

1. Straum checks the ask against the spec and refuses any ask it can't
   answer exactly. Once a load has read the column types, it also refuses
   a filter value of another type than its column.
2. It reads the cells of the buckets in the range: the widest the view
   keeps that fit, and narrower ones at the ends.
3. Every key the ask names is part of the cell key, so nothing is looked
   up.
4. It adds each cell to its group, and to each measure whose filter holds.
   Filters name only parts of the cell key, so a cell counts whole or
   not at all.

One writer applies the stream while asks read. They share the store
behind a read-write lock, which the writer takes only to apply a commit it
has already read. A long ask delays the writer, and that shows as lag.
