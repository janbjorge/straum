# ADR-0018: Triggers write changes to an outbox

Status: Accepted 2026-11-20

## Context

Where a server gives neither logical decoding nor a slot, `NOTIFY`
(ADR-0017) costs a global lock on every notifying commit, a reload on
every disconnect, and app commits that fail if the queue fills. A
table has none of that: rows wait until Straum reads them.

A table has no commit order; ids and xids are handed out before
commit. But two transactions that change one row take its lock in
turn, and a trigger fires after the lock is held, so the later one's
rows come later. Changes to different rows commute in cells.

An outbox nobody reads grows on the source for good.

## Decision

The triggers of ADR-0017 write their rows to an outbox table that
`straum plan` prints, instead of notifying. Straum polls it, applies
what it reads in the outbox's order, and deletes it in the same
transaction. A notification may wake Straum early; it never carries a
row.

A start deletes what the load already holds, so nothing applies
twice.

The triggers write only while Straum shows it is polling. Straum fails
hard (ADR-0009) before that sign could go stale, and a start renews it
and waits out every transaction that may have seen it stale before
loading, so a skipped write is always in the load.

Rejected: commit order from the server, which needs a setting and a
restart; a counter every writer updates, which serializes them;
`NOTIFY` alone (ADR-0017) where a table is allowed.

## Consequences

- Runs where triggers run, with no replication attribute, no
  `wal_level` and no global lock; a disconnect costs a poll, not a
  reload.
- The app's writes pay the trigger and an insert a statement.
- Straum writes to the source: its deletes, which vacuum pays for, and
  its sign of polling.
- A long transaction on the server holds a start back until it ends.
- Lag is at least the poll interval.
- That the outbox's order applies exactly is reasoned from row locks,
  not tested; a property test against a reference ships with it.
