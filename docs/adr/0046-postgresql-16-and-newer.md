# ADR-0046: Straum runs on PostgreSQL 16 and newer

Status: Accepted, 2026-11-14

## Context

ADR-0024 set the floor at PostgreSQL 17 to avoid differences nobody
had tested. Straum reads nothing 17 added: pgoutput protocol version 1
in text, a temporary slot made with the parenthesized options of 15,
and the `attnames` and `rowfilter` columns `pg_publication_tables` has
had since 15. The straum-pg and straum tests pass on 16 unchanged.
Many managed servers still run 16.

## Decision

PostgreSQL 16 and newer, no upper bound. CI runs the tests on 16, 17,
18 and 19rc1 (19 once it ships), picked by `STRAUM_TEST_POSTGRES`;
locally they run on 18. Tests start PostgreSQL themselves with
`testcontainers`, one container per test, as before.

## Consequences

- 16 needs `wal_level = logical`, as 17 and 18 do; the start check
  already says so.
- One more CI job, about as long as the others.
- PostgreSQL 16 leaves support in November 2028; the floor moves up
  then, with a record superseding this one.
- A quirk of a newer release is found by CI, not by a user.
