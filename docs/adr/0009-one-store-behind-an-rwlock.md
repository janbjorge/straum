# ADR-0009: Asks and the writer share one store behind an RwLock

Status: Accepted (retroactive), 2026-11-10

## Context

Asks read while the writer applies commits and loads tables. Options
were a lock per table, copy-on-write shards, `left-right`, or one
`RwLock` around the store with the writer holding it as briefly as it
can.

## Decision

One store behind an `RwLock`. `App` is the store and how far it has
followed the source, cheap to clone. The writer takes the write lock
only to apply a commit it has already read, or to swap a table in or
out: a load takes the table out of the store and fills it outside the
lock.

## Consequences

- Asks never wait on PostgreSQL, and asks on other tables go on during
  a load.
- A long ask delays the writer, which shows as lag, and `/readyz` waits
  behind it. An ask over every 15-minute bucket on the taxi month holds
  the lock 2.4 to 3.0 s.
- Copy-on-write shards or `left-right` are the next step if that ever
  matters. Not taken yet.
