# ADR-0008: Straum reads physical WAL where logical decoding is off

Status: Accepted 2026-11-14; revised 2026-11-20

## Context

PostgreSQL 17 and 18 refuse a logical slot under `wal_level =
replica`, and many production servers can't take the restart to
change it. The WAL still holds every change, but as page edits: a
delete names only its slot on the page, an update may log only the
bytes that differ. The old row is on the page, and the first change to
a page after a checkpoint carries a full image of it. So any reader of
this WAL has to remember rows.

## Decision

`straum run --wal physical` streams WAL through a temporary physical
slot from the last checkpoint's redo point. Straum turns heap records
into the messages the logical stream yields, `Begin`, `Relation`,
`Insert`, `Update` with the whole old row, `Delete` and `Commit`, so
one decoder feeds the store from both streams. Values are read from
the row's bytes as the load reads binary `COPY`, sharing `value.rs`;
printing them as text to parse back was dropped, and its reader
refused years before 1 or after 9999. Only the columns the spec reads
are decoded; the others are stepped over, as a busy table's other
values were most of the decoding. The load is plain SQL in a
`REPEATABLE READ` transaction, and commits its snapshot saw are
skipped. A table rewrite, DDL or a TOASTed value Straum can't rebuild
is `Unfit` and stops Straum. Neither stream is a default: `straum run`
and the setup `straum plan` prints take `--wal logical` or `--wal
physical`, since the two need different server settings and setup.

The reader knows the spec tables' relation files and passes by each
record that names only other files' pages and whose resource manager
is not XLOG, transaction, storage or standby: it reads the block
headers in place and checks the page headers the record runs across,
but neither copies, checksums nor decodes it. On a busy server nearly
all WAL is such records, and the checksum and copy were most of
reading it. A corrupt record of another table can't change a row
Straum reads, and a corrupt length trips the next page header check;
every record Straum reads is still checksummed. A record whose block
headers run onto the next page is read whole.

Page state keeps each changed page of the spec's tables, its rows
whole as PostgreSQL has them. A row goes when a prune or vacuum frees
its slot; a page goes once a checkpoint's redo point passes its last
change, since its next change brings a full image. Past
`--max-page-memory` Straum stops. Cutting rows at the last
column read was dropped: rebuilding an update from a row of unknown
length needs a partial column walk per major.

## Consequences

- No `wal_level` change, publication or `REPLICA IDENTITY FULL`. The
  role still needs `REPLICATION`; the server needs `full_page_writes =
  on`.
- The stream carries the whole server's WAL, every database; the
  bench passes other tables' WAL at about 6 GB/s, above 2 GB/s for
  splitting every record.
- Memory is about 1.2 times the bytes of the spec tables' pages changed
  in one checkpoint cycle, plus open transactions' changes; wide tables
  cost more. A shorter `checkpoint_timeout` or a larger
  `--max-page-memory` answers a start-over; rows on disk behind a cache
  or cut rows wait for a measurement.
- WAL differs by major: 17, 18 and 19 are each parsed and tested; 19
  moves prune flags and drops the page flag `XLP_BKP_REMOVABLE`, so
  its overwrite flag takes that bit. The setup check refuses a newer
  major, whose layout no test has seen. The reader takes the page
  flags from the major the setup check read; the WAL page magic guards
  only a change of format mid-stream, not which major wrote it.
- A physical value equals the load's, years the text reader refuses
  included; the logical stream still errors on those.
- Proof: on a `wal_level = logical` server both modes yield equal
  commits for a seeded workload; the view oracles pass in physical mode
  under `replica`; page oracles against `pageinspect` compare rows; a
  property test holds the records read with passing by to those an
  unfiltered read keeps.
