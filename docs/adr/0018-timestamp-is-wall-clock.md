# ADR-0018: A timestamp column is wall clock

Status: Accepted (retroactive), 2026-11-10

## Context

Real schemas use `timestamp` without a zone. PostgreSQL's `date_bin`
over it bins the wall clock reading; cast through the session zone it
would mean something else, and Straum has no session zone.

## Decision

A `timestamp` column is a time column read as the Unix seconds its wall
clock reading would be in UTC, in binary `COPY` and pgoutput text
alike. `ColumnType::Time { zoned }` tells it from `timestamptz`. The
`date_bin` origin must match the column's type, `timestamp 'epoch'` or
`timestamptz 'epoch'`, which the catalog check enforces. No `AT TIME
ZONE` in views. `infinity` and `-infinity` read as a null time, so the
row lands in the bucket no ask reads.

## Consequences

- Buckets, bounds and printed buckets over a `timestamp` are wall
  clock, as `date_bin` makes them in PostgreSQL.
- A zone parameter over a `timestamp` will be refused by name when
  zones come (M8): there is no instant to move.
- A row with no time is never answered; `NaN`, an overflowing numeric
  and non-UTF-8 text still fail the load (ADR-0015).
