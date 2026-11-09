# How Straum works

Straum loads every table from a snapshot at one point in the WAL, an LSN
called L, then applies every commit after L. No commit is missed between
the load and the stream, and none is applied twice.

## Start

![The writer thread connects for replication and opens a temporary logical slot whose snapshot is at L. In that snapshot it checks the catalog and loads dims and facts, then swaps the loaded store in. The stream starts at L, and a stream thread hands each commit to the writer, which applies it under the write lock.](diagrams/start.svg)

1. Straum opens a replication connection and creates a temporary logical
   slot with `pgoutput`. PostgreSQL exports a snapshot at the slot's
   consistent point, L.
2. In that snapshot, Straum checks the catalog against the spec, then
   loads every dim and fact. Each fact loads with one `GROUP BY` over the
   rollup key and the bucket, so PostgreSQL sends one row per group.
3. Straum ends the snapshot, swaps the loaded store in and turns ready.
4. It starts the stream at L. A stream thread reads it, answers the
   server's keepalives and confirms what the writer has applied, so the
   server can recycle WAL.

The load fills a new store outside the lock, so asks never wait on
PostgreSQL: they answer 503 until the load is in. Straum drops the old
store before it loads again, so the two never take memory at once.

## Rollups

For each fact, Straum keeps a rollup: one cell per combination of by
columns, ref ids and time bucket. A cell holds its row count and what
each measure needs:

- `sum` and `avg` share the column's sum and count of non-null values.
- `min` and `max` keep a count per value, so deleting the smallest value
  reveals the next one.
- `count(DISTINCT c)` keeps a count per value.

Cells are kept per bucket, side by side, and with `buckets` each wider
width keeps cells of its own. Dims are kept as rows by key.

## Applying a commit

Changes arrive after their transaction commits, and Straum applies a
transaction whole:

- An insert into a fact adds the row to its cell.
- An update takes the old row out of its cell and adds the new one. That
  is why facts need `REPLICA IDENTITY FULL`: without the old row, Straum
  can't know which cell to take it from.
- A delete takes the old row out. A cell goes with its last row.
- A truncate empties the rollup.
- Dim rows are upserted and deleted by key.

A change Straum can't apply makes it start over: an update without its old
row, a row that doesn't fit its rollup, or a table whose columns now read
differently. Starting over drops the slot and loads everything again from
a new one.

## Asking

![An ask for trips by pickup.borough from 00:00 to 00:30 walks the cells of the trips rollup. It keeps the cells whose bucket is in the range, looks up each ref id's borough in the dim, and adds each cell's rows to the measures whose filter holds. A ref id with no dim row groups under null.](diagrams/ask.svg)

An ask reads only cells:

1. Straum checks the ask against the spec and refuses any ask it can't
   answer exactly.
2. It reads the cells of the buckets in the range: the widest the view
   keeps that fit, and narrower ones at the ends.
3. It looks up the attributes the ask needs by ref id, once per ref id it
   meets. Dim attributes are not part of the rollup key, so a change to a
   dim row changes answers without touching a cell.
4. It adds each cell to its group, and to each measure whose filter holds.
   Filters name only parts of the rollup key, so a cell counts whole or
   not at all.

One writer applies the stream while asks read. They share the store behind
a read-write lock, which the writer takes only to apply a commit it has
already read. A long ask delays the writer, and that shows as lag.

## Lifecycle

![Straum moves from Loading to Ready once every table is loaded. Ready turns Lagging past --max-lag and back once it catches up. Any failure leads to Waiting, then Loading from a new slot. SIGTERM leads to Stopping.](diagrams/lifecycle.svg)

`/readyz` is 200 only in Ready. Asks answer in Ready and Lagging, and
answer 503 while Straum waits and loads.

## Why RAM only

Straum keeps nothing on disk, and its slot is temporary. A Straum that
crashes, runs out of memory or loses its pod leaves no slot behind to hold
WAL on the server. The price:

- Every start is a full load, under one snapshot that holds back vacuum
  while it runs.
- Temporary slots don't move to a standby, so after a failover Straum
  loads again from the new primary.
- While connected, the slot holds WAL until Straum confirms it, which
  `--max-retained-wal` bounds.
