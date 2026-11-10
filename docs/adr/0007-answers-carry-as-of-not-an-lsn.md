# ADR-0007: Answers carry as_of, the LSN goes in a header

Status: Accepted (retroactive), 2026-11-10

## Context

The first answers carried the LSN they reflected and the commit time of
the last applied transaction. Without a WAL barrier a client can't act
on an LSN, and the commit time stops moving on a quiet database, so
neither told a client how fresh the answer was.

## Decision

An answer is `{"as_of", "rows"}`, each row an object keyed by the names
in `SELECT`. `as_of` is when the slot was last known to have confirmed
all the WAL the server had written (`Lag::caught_up`): every commit
made before it is in the answer, and it keeps moving on a quiet
database. The LSN goes in a `Straum-LSN` header. `GET /status` shows
the same `as_of`.

## Consequences

- `as_of` can lag the truth by up to the WAL reading interval, about a
  second, never the other way.
- avg is rounded half to even at four places past its column's scale,
  trailing zeros dropped; the domain keeps sum and count, so rounding
  stays in `straum-http`.
- `Answer` names its own columns, so the writer doesn't need the ask,
  and a key or measure asked twice is refused.
- A WAL barrier, if one ever comes, can use the header without changing
  the body.
