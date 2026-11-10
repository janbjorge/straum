# ADR-0020: Lag is a time from the server's WAL position

Status: Accepted (retroactive), 2026-11-10

## Context

Readiness needs a measure of how far behind Straum is. The applied LSN
alone says nothing without the server's position, and a commit
timestamp stops moving on a quiet database.

## Decision

About once a second the writer reads `pg_current_wal_lsn()` and its
slot's `confirmed_flush_lsn` and `restart_lsn` on the plain connection.
Lag is the time since the last reading whose WAL the slot had confirmed
all of. `/readyz` is off past `--max-lag` (30 s); asks still answer,
with their LSN. WAL written during the load counts from the start.

## Consequences

- Lag is up to a second longer than the truth, never shorter. The same
  reading gives `as_of` (ADR-0007) and retained WAL (ADR-0015).
- `/readyz` says why Straum isn't ready; the log says why it started
  over. No metrics or OTLP yet (ADR-0023).
- The readings run as the role `straum plan` makes, with no grant
  beyond the tables'.
