# ADR-0049: Straum reads physical WAL where logical decoding is off

Status: Accepted, 2026-11-14; amended by ADR-0051 and ADR-0052

## Context

PostgreSQL 16 to 18 refuse a logical slot under `wal_level = replica`,
and many production servers can't take the restart to change it. The
WAL still holds every change, but as page edits: a delete names only
its slot on the page, an update may log only the bytes that differ
from the old row. The old row is on the page, and the first change to
a page after a checkpoint carries a full image of it. So no reader of
this WAL can do without some state: it has to remember rows.

## Decision

`straum run --wal physical` streams WAL through a temporary physical
slot, which starts at the last checkpoint's redo point. Straum turns
heap records into the pgoutput messages the logical stream yields:
`Begin`, `Relation`, `Insert`, `Update` with the whole old row,
`Delete`, `Commit`. Values go in as text. From there the existing
decoder builds commits, so both streams share one path to the store.
The load is plain SQL in a `REPEATABLE READ` transaction; commits its
snapshot already saw are skipped. A table rewrite, DDL or a TOASTed
value Straum can't rebuild becomes `Unfit`, and Straum starts over.
`--wal logical` stays the default and is unchanged.

Straum remembers as little as the WAL allows. For each page of the
spec's tables changed since the last checkpoint, it keeps each row's
bytes up to the end of the last column the spec reads, in one buffer
per page. That is enough to rebuild an update: where the unchanged
tail reaches a column the spec reads, every byte after it is unchanged
too. A row's bytes go when a prune or vacuum frees its slot, as in
PostgreSQL, and a page goes once a checkpoint's redo point passes its
last change, since its next change brings a full image. Past
`--max-page-memory` Straum starts over, as it does past
`--max-retained-wal`.

## Consequences

- No `wal_level` change and no publication or `REPLICA IDENTITY FULL`
  in physical mode. The role still needs `REPLICATION`, and the server
  needs `full_page_writes = on`.
- The stream carries the whole server's WAL, every database.
- Memory follows the rows the spec's tables change in two checkpoint
  cycles, times the bytes up to their last read column. A shorter
  `checkpoint_timeout` lowers it and raises the WAL. Should that not
  do, the rows can move to disk behind a small cache, as the spill
  does.
- Heap records differ by major, so each of 16, 17, 18 and 19 is parsed
  and tested on its own, guarded by the WAL page magic.
- Proof: on one `wal_level = logical` server, both modes yield equal
  commits for a seeded workload; the view oracles also pass in
  physical mode under `replica`.
