# ADR-0012: The load is a binary COPY grouped in Straum

Status: Accepted (retroactive), 2026-11-10

## Context

The first load was one `GROUP BY` per fact run in PostgreSQL on the
slot's snapshot. It sorted or hashed the whole table there, could
spill to disk, ran spec text on the server, and arrived as text over
the simple query protocol. Taxi month: 9.4 s, text `COPY` 5.2 s,
binary 3.0 s.

## Decision

A load is `COPY (SELECT <columns> FROM <table> ...) TO STDOUT (FORMAT
binary)`, one per fact, and Straum groups the rows itself. PostgreSQL
only scans. A reader thread decodes into batches of 8192 rows, four in
flight; each bucket width is filled on its own thread. The snapshot
transaction commits right after the last load, not at the first
stream read.

## Consequences

- A load holds about 1.2 MB per column besides the store, however big
  the table. Taxi month loads in 1.9 to 2.0 s; PostgreSQL alone copies
  the columns to `/dev/null` in 1.4 s.
- Nothing from the spec runs in PostgreSQL. The join to dims (ADR-0013)
  is the one thing PostgreSQL may hash.
- The load holds `AccessShareLock` and pins xmin until the snapshot
  commits. Held longer, it blocked a test's `ALTER TABLE`.
- The stream never starts before the slot's consistent point.
