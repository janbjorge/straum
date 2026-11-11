# ADR-0031: Every spec table has REPLICA IDENTITY FULL

Status: Accepted, 2026-11-11

## Context

ADR-0014 required `REPLICA IDENTITY FULL` on facts: removing a row
from a cell needs the old row whole. With refs bound at ask time
(ADR-0030) Straum holds each dim projected to its key and the columns
the views read, and takes a changed or deleted dim row out of that
projection the same way. A dim's default identity sends only the key
on delete and nothing old on an update that keeps the key, which is
not enough to find the row held.

## Decision

Every table a spec names, fact or dim, needs `REPLICA IDENTITY FULL`
and is in the publication. `straum plan` prints both for every table,
and every start checks them, naming the fix. A dim needs no primary
key or unique index: a key may match any number of rows. Updates and
deletes of either kind of table apply in place from the old row the
WAL carries; truncate empties the cells or the projection.

## Consequences

- FULL adds WAL on `UPDATE` and `DELETE` only; a table that only gets
  inserts pays nothing.
- A dim's identity is its whole row, so a row held twice is taken out
  one at a time, as a delete of one row in PostgreSQL removes one.
- A table that drops FULL or leaves the publication fails the start
  check with the fix named.
- Supersedes ADR-0014.
