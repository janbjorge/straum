# ADR-0010: Cold buckets are sealed and spilled

Status: Accepted 2026-11-11; revised 2026-11-16; no adapter ships

## Context

Since refs bind at ask time (ADR-0006), the warehouse copy holds 2,378
MiB, and most asks read recent buckets. Old buckets could stay, be
dropped or move out of memory. Where they go should not reach the
domain. The narrowest width holds most cells: 76% of the taxi month's,
50% of the warehouse copy's.

## Decision

A bucket of the `date_bin` width whose end is older than
`--seal-after` is sealed: the domain hands it out as `Sealed`, plain
data with keys, row counts and every column's state, and keeps only
its place in the level. Wider widths stay in memory. A rule per width,
a quiet period or a memory budget each added state to explain.

A port in `straum-app`, `Spill`, puts, gets and removes sealed buckets
by `SealedId` (fact, width, start). Without `--spill-dir` nothing is
sealed. The domain names the sealed buckets an ask reads or a commit
touches; the app fetches them; a bucket read back is thawed, checked
against the store, first. The writer unseals a touched bucket before
applying the commit, and the next pass seals it again. Every start
empties the spill.

The adapter `straum-arrow` writes each sealed bucket as one
uncompressed Arrow IPC record batch under `--spill-dir`, through
`arrow-ipc` and `arrow-array`, read with plain reads, not mmap. A
format of our own costs upkeep; Parquet decodes on every read; Vortex
and Lance are young and heavy; a key-value store keeps a row per cell.

## Consequences

- A storage is an adapter; domain, app and port don't change for it.
- Steady memory follows the wider widths, the hot narrow buckets, the
  codes and the projections. The load's peak doesn't fall: `COPY` isn't
  ordered by time.
- A long-range ask reads the spill only at its ends; one grouped by
  `bucket` over old time reads every bucket, under the read lock, which
  the writer sees as lag.
- Unsealing on write keeps one cell form; a signed delta merged at ask
  time would read less, but count distinct can't take one. Churn on a
  often-changed bucket is unmeasured; a quiet period is the first fix.
- A failed spill starts Straum over (ADR-0009); an unreadable sealed
  bucket answers 500 naming it.
- A restart loads from PostgreSQL: reusing the spill needs the codes
  and the slot's LSN kept with it.
- No mmap, so no second `unsafe` (ADR-0013). Arrow is Apache-2.0.
  `straum-arrow` depends on `straum-app` and `straum-domain`. The root
  filesystem is read-only, so the chart needs a volume for the spill.
- Until the adapter ships, `main.rs` passes no `Spill` and nothing is
  sealed.
