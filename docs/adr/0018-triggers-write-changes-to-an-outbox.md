# ADR-0018: Triggers write changes to an outbox

Status: Accepted 2026-11-20; revised 2026-11-20

## Context

Both streams need what a server may not give: logical decoding a
`wal_level` change, the physical stream the replication attribute.
Triggers need neither, and see whole rows, old and new. But a trigger
runs in the app's transaction: its error fails the app's write, and a
column it names ties it to the table's migrations. On PostgreSQL 18 a
trigger naming a column failed every write once that column was
renamed, and one filtering on columns blocked dropping or retyping
them.

A table holds what triggers write until Straum reads it, across any
disconnect. It has no commit order; ids and xids are handed out before
commit. But two transactions that change one row take its lock in
turn, and a trigger fires after the lock is held, so the later one's
rows come later. Changes to different rows commute in cells.

An outbox nobody reads grows on the source for good, and a long
transaction holds vacuum back, so even a read one grows on disk. One
PostgreSQL value holds at most 1 GB. Two readers of one outbox take
each other's rows.

## Decision

Straum captures a spec table's changes with triggers that `straum
plan` prints and the table owner installs. They fire once a statement,
not once a row, name no column, and send every changed row whole;
Straum takes the columns the spec reads. So no rename, drop or retype
fails the app's write or is blocked. An update goes as its old rows
out and its new rows in, which cells take exactly. Rows carry their
types, so Straum notices a retyped column. A `TRUNCATE`, or a
statement that changes more rows than a commit may hold (ADR-0009),
writes nothing and counts as skipped; that cap also keeps a
statement's rows within one value, short of rows near 10 KB each.

The triggers write to one outbox table that `straum plan` prints, its
ids handed out one at a time so their order follows the row locks.
Straum polls it and deletes exactly the rows it read, as it reads
them. A poll that returns less than it asked for took every committed
change, so all Straum read since the last such poll is whole
transactions: it applies that as one commit, in id order, and stops if
it outgrows what a commit may hold. A trigger gone or rows Straum
can't read fail hard (ADR-0009).

The triggers write only while Straum shows it is polling, a sign that
expires by itself. A trigger that finds it expired writes nothing and
counts the skip; Straum reads the count after each poll and fails hard
when it moved. A start renews the sign, waits out every transaction
that may have seen it expired, then loads the tables and deletes, in
the load's own snapshot, the outbox rows that snapshot sees: a skipped
write is always in the load, and nothing applies twice. Straum renews
the sign only while the outbox takes less on disk than the server may
keep for it (ADR-0009), so the outbox is bounded whether Straum is
gone, slow, or vacuum is held back.

One outbox serves one Straum. A second Straum that starts on it fails
hard, so the outbox stream runs one replica, and a deploy stops the
old Straum before it starts the new.

Rejected: triggers that name columns; a trigger per row, one outbox
row each; sending only the columns the spec reads with unchanged rows
left out, which reads the catalog and builds its SQL in every
statement; splitting a statement over outbox rows, which took a
5-million-row update from 46 s to 72 s; `TRUNCATE` to empty the outbox
at a start; an extension on the server; commit order from the server,
which needs a setting and a restart; a counter every writer updates,
which serializes them; `NOTIFY`, to carry the rows or only to wake
Straum, which holds a lock global to the database through every
notifying commit; reading past a remembered id, which loses a
transaction that commits late with a low one; an outbox per Straum,
a table each and every change written once per Straum.

## Consequences

- Runs on any server that allows triggers, with no replication
  attribute, slot or global lock. A disconnect costs a poll.
- The app's writes pay the trigger and an insert a statement. On the
  taxi trips a small statement took a sixth longer, a bulk one under
  the cap a third longer, and the outbox added a tenth to a fifth to
  the tables' write-ahead log (WAL).
- Whole rows carry columns no spec reads: a wide one is written twice,
  and a change to it alone is sent too.
- A column whose type PostgreSQL can't send in binary, `aclitem` and
  `gtsvector` of its own, fails every write to a table with triggers.
- One Straum an outbox: no second replica. On the other streams a
  deploy's new Straum loads beside the old; here the old stops first,
  and nothing answers until the new one has loaded.
- A `TRUNCATE`, a statement or backlog past what a commit may hold, a
  skipped write and a full outbox each cost a reload, never a partial
  answer. After a full outbox a start waits for vacuum, a minute when
  measured; a long transaction holds that and any start back.
- Lag is at least the poll interval. An outbox row carries no commit
  position, so the port's positions are the source's own (ADR-0016).
- No replicator found uses statement triggers or a sign its triggers
  check, so a property test against a reference ships with the code.
