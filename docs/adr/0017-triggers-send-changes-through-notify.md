# ADR-0017: Triggers send changes through NOTIFY

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

`NOTIFY` needs nothing either, but its payload is small, a commit that
notifies holds a lock global to the database through its flush, a full
queue fails the notifying commit, and a listener that disconnects
loses what came meanwhile. A connected listener gets every
notification in commit order.

## Decision

Straum captures a spec table's changes with triggers that `straum
plan` prints and the table owner installs. The triggers never fail the
app's write and never block a migration: they name no column, and are
told which columns the spec reads. A change to none of those columns
sends nothing.

The triggers fire once a statement, not once a row, so a bulk write
sends a few notifications instead of one per row. An update goes as
its old rows out and its new rows in, which cells take exactly. Rows
carry their types, so Straum notices a retyped column.

Straum listens on one connection and treats anything it can't vouch
for as lost: a disconnect, a dead connection, a statement's rows
arriving incomplete, a trigger gone. Each fails hard (ADR-0009), and
the start after reloads. Straum stops listening before the queue could
fill, so a slow Straum never fails the app.

Rejected: triggers that name columns, which fail writes or block
migrations; a trigger per row, one notification each under the global
lock; an extension on the server.

## Consequences

- Runs on any server that allows triggers: no replication attribute,
  no `wal_level`, no slot.
- Every commit on a spec table waits on the global lock; fit for
  tables written at modest rates.
- Every disconnect costs a full reload.
- The app's writes pay the trigger on every statement.
- A notification carries no commit position; lag is measured another
  way.
- That out-then-in updates apply exactly is reasoned, not tested; a
  property test against a reference ships with it.
