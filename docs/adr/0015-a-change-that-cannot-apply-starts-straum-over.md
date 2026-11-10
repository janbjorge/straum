# ADR-0015: A change that can't apply starts Straum over

Status: Accepted (retroactive), 2026-11-10

## Context

Some changes can't be applied to cells: a row that doesn't fit, a fact
row bound to an old dim value, an altered table, a transaction too big
to buffer, a slot holding too much WAL. The first design read one
table again on a second connection at the failing LSN while the others
kept answering.

## Decision

Straum starts over from a new slot. The new slot's snapshot is after
the failing commit, so its rows arrive through the bulk `COPY` load
(ADR-0012) instead of being held in memory and applied under the write
lock. `--max-transaction-rows` (default 100000) caps the changes one
transaction may hold; past `--max-retained-wal` (4Gi) Straum starts
over too, since Azure fixes `max_slot_wal_keep_size` at `-1`. A value
Straum can't read fails the load naming the table and column, not the
row.

## Consequences

- One recovery path to test instead of per-table reload plus restart.
- Asks on every table answer not ready while a load runs, and
  `/readyz` says why.
- A load that takes longer than the WAL cap allows starts over again
  and again, which the log shows.
- The default cap comes from `bench/transaction.py`, which measures
  how long a transaction of that size holds the lock on the taxi month.
- Per-table reload and a chunked apply are open, not planned.
