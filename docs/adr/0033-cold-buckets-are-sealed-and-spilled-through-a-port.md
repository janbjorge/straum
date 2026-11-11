# ADR-0033: Cold buckets are sealed and spilled through a port

Status: Accepted, 2026-11-11; amended by ADR-0035

## Context

Since ADR-0030 cells follow distinct join ids, and the warehouse copy
holds 2,378 MiB. Most asks read recent buckets. Old buckets could stay
in memory, be dropped, or move out of memory. Where they go, a file, a
key-value store or an object store, should not reach the domain.

## Decision

A bucket whose end is older than `--seal-after` is sealed: the domain
hands it out as `Sealed`, plain data with the keys, row counts and each
column's states, value counts included, and keeps only its place in the
level. A port in `straum-app`, `Spill`, puts, gets and removes sealed
buckets by `SealedId` (fact, width, start). Without `--spill-dir`
nothing is sealed.

The domain stays free of I/O. It names the sealed buckets an ask reads
or a commit touches; the app gets them through `Spill`, the ask reads
them beside the cells in memory, and the writer unseals a touched
bucket before it applies the commit. Spilled buckets are scratch: every
start empties them, and the load fills memory as before.

## Consequences

- A storage is an adapter. The domain, the app and the port don't
  change for another one.
- Steady memory follows the hot buckets, the codes and the projections.
  The load's peak doesn't: COPY isn't ordered by time (ADR-0012).
- An ask over sealed buckets reads them under the read lock, which the
  writer sees as lag (ADR-0009).
- Unsealing on write keeps one cell form. A signed delta merged at ask
  time would read less, but count distinct can't take one.
- A restart still loads from PostgreSQL. Reusing spilled buckets needs
  the codes and the slot's LSN kept with them, a decision of its own.
