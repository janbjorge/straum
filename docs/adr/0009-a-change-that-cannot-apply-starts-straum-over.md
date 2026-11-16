# ADR-0009: A change that can't apply starts Straum over

Status: Accepted 2026-11-10; revised 2026-11-16

## Context

Some changes can't be applied to cells: a row that doesn't fit, an
altered table, a transaction too big to buffer, a slot holding too
much WAL, a failed spill. The first design read one table again at the
failing LSN while the others kept answering. Readiness needs a
measure of how far behind Straum is; the applied LSN means nothing
without the server's, and a commit time stops on a quiet database.

## Decision

Straum starts over from a new slot. Its snapshot is after the failing
commit, so those rows arrive through the bulk load (ADR-0007) instead
of being held and applied under the write lock.
`--max-transaction-rows` (100000) caps the changes a transaction may
hold, a default measured by `bench/taxi.py transactions`. Past
`--max-retained-wal` (4Gi) Straum starts over too, since Azure fixes
`max_slot_wal_keep_size` at `-1`. A value Straum can't read (`NaN`,
an overflowing numeric, non-UTF-8 text) fails the load naming table
and column.

About once a second the writer reads `pg_current_wal_lsn()` and its
slot's `confirmed_flush_lsn` and `restart_lsn`. Lag is the time since
the last reading whose WAL the slot had wholly confirmed; WAL written
during a load counts from its start. `/readyz` is off past `--max-lag`
(30 s), while asks still answer. The same reading gives `as_of`
(ADR-0005) and the retained WAL.

## Consequences

- One recovery path to test, not per-table reload plus restart.
- Asks answer not ready while a load runs; `/readyz` says why and the
  log says why Straum started over.
- A load slower than the WAL cap allows starts over again and again,
  which the log shows.
- Lag is up to a second longer than the truth, never shorter. The
  readings need no grant beyond the tables'.
- Per-table reload and a chunked apply are open, not planned.
