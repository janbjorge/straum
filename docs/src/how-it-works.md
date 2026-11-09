# How Straum works

Straum loads every table from a snapshot at one point in the WAL, an LSN
called L, then applies every commit after L. No commit is missed between
the load and the stream, and none is applied twice.

## Start

![The writer thread connects for replication and opens a temporary logical slot whose snapshot is at L. In that snapshot it checks the catalog and loads dims and facts, then swaps the loaded store in. The stream starts at L, and a stream thread hands each commit to the writer, which applies it under the write lock.](diagrams/start.svg)

1. Straum opens a replication connection and creates a temporary logical
   slot with `pgoutput`. PostgreSQL exports a snapshot at the slot's
   consistent point, L.
2. In that snapshot, Straum checks the catalog against the spec and loads
   every dim and fact. Each table loads with one binary `COPY` of the
   columns the spec reads. Straum groups the rows into cells as they
   come, a batch at a time, so it never holds the table.
3. Straum ends the snapshot, swaps the loaded store in and turns ready.
4. It starts the stream at L. A stream thread reads it, answers the
   server's keepalives and confirms what the writer has applied, so the
   server can recycle WAL.

The load fills a new store outside the lock, so asks never wait on
PostgreSQL; they answer 503 until the load is in. Straum drops the old
store before it loads again, so the two never take memory at once.

## Cells

A cell holds its row count and what each measure needs:

- `sum` and `avg` share the column's sum and count of non-null values.
- `min` and `max` keep a count per value, so deleting the smallest value
  reveals the next one.
- `count(DISTINCT c)` keeps a count per value.

Each bucket width keeps cells of its own. Dims are kept as rows by key.

## Applying a commit

Changes arrive after their transaction commits, and Straum applies the
transaction whole:

- An insert into a fact adds the row to its cell.
- An update takes the old row out of its cell and adds the new one. Facts
  need `REPLICA IDENTITY FULL` for this: without the old row, Straum can't
  know which cell to take it from.
- A delete takes the old row out. A cell goes with its last row.
- A truncate empties the rollup.
- Dim rows are upserted and deleted by key.

A change Straum can't apply makes it [start over](running.md#states) from
a new slot: an update without its old row, a row that doesn't fit its
rollup, or a table whose columns now read differently.

## Answering an ask

![An ask for trips by pickup.borough from 00:00 to 00:30 walks the cells of the trips rollup. It keeps the cells whose bucket is in the range, looks up each ref id's borough in the dim, and adds each cell's rows to the measures whose filter holds. A ref id with no dim row groups under null.](diagrams/ask.svg)

1. Straum checks the ask against the spec and refuses any ask it can't
   answer exactly.
2. It reads the cells of the buckets in the range: the widest the view
   keeps that fit, and narrower ones at the ends.
3. It looks up the attributes the ask needs by ref id, once per ref id it
   meets. Attributes are not part of the rollup key, so a change to a dim
   row changes answers without touching a cell.
4. It adds each cell to its group, and to each measure whose filter holds.
   Filters name only parts of the rollup key, so a cell counts whole or
   not at all.

One writer applies the stream while asks read. They share the store
behind a read-write lock, which the writer takes only to apply a commit it
has already read. A long ask delays the writer, and that shows as lag.
