# ADR-0018: Triggers write changes to an outbox

Status: Accepted 2026-11-20

## Context

Both streams need what a server may not give: logical decoding a
`wal_level` change, the physical stream the replication attribute.
Triggers need neither, and see whole rows, old and new. But a trigger
runs in the app's transaction: its error fails the app's write, and a
column it names ties it to the table's migrations. On PostgreSQL 18 a
trigger naming a column failed every write after that column was
renamed, and one filtering on named columns blocked dropping or
retyping them.

A table holds what triggers write until Straum reads it, across any
disconnect. It has no commit order; ids and xids are handed out before
commit. But two transactions that change one row take its lock in
turn, and a trigger fires after the lock is held, so the later one's
rows come later. Changes to different rows commute in cells.

An outbox nobody reads grows on the source for good.

## Decision

Straum captures a spec table's changes with triggers that `straum
plan` prints and the table owner installs. The triggers never fail the
app's write and never block a migration: they name no column, and are
told which columns the spec reads. A change to none of those columns
writes nothing.

The triggers fire once a statement, not once a row, so a bulk write
costs a few outbox rows instead of one per row. An update goes as its
old rows out and its new rows in, which cells take exactly. Rows carry
their types, so Straum notices a retyped column.

The triggers write to an outbox table that `straum plan` prints.
Straum polls it, applies what it reads in the outbox's order, and
deletes it in the same transaction. A notification may wake Straum
early; it never carries a row. A start deletes what the load already
holds, so nothing applies twice. A trigger gone or rows Straum can't
read fail hard (ADR-0009).

The triggers write only while Straum shows it is polling. Straum fails
hard before that sign could go stale, and a start renews it and waits
out every transaction that may have seen it stale before loading, so a
skipped write is always in the load.

Rejected: triggers that name columns, which fail writes or block
migrations; a trigger per row, one outbox row each; an extension on
the server; commit order from the server, which needs a setting and a
restart; a counter every writer updates, which serializes them;
`NOTIFY` carrying the rows, which holds a lock global to the database
through every notifying commit, fails the app's commit when its queue
fills, and loses what comes while Straum is disconnected, so each
disconnect costs a reload.

## Consequences

- Runs on any server that allows triggers: no replication attribute,
  no `wal_level`, no slot, no global lock. A disconnect costs a poll,
  not a reload.
- The app's writes pay the trigger and an insert a statement.
- Straum writes to the source: its deletes, which vacuum pays for, and
  its sign of polling.
- A long transaction on the server holds a start back until it ends.
- Lag is at least the poll interval; an outbox row carries no commit
  position, so lag is measured another way.
- That out-then-in updates and the outbox's order apply exactly is
  reasoned, not tested; a property test against a reference ships with
  it.
