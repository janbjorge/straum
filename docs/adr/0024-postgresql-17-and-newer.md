# ADR-0024: Straum runs on PostgreSQL 17 and newer

Status: Superseded by ADR-0046

## Context

Supporting older servers means carrying pgoutput and catalog
differences nobody runs Straum against. Tests that need PostgreSQL
could share one developer-managed server or start their own.

## Decision

PostgreSQL 17 and newer, no upper bound. CI runs the tests on 17, 18
and 19rc1 (19 once it ships), picked by `STRAUM_TEST_POSTGRES`;
locally they run on 18. Tests start PostgreSQL themselves with
`testcontainers` (`blocking`, no default features, so no C), one
container per test, removed when the test ends.

## Consequences

- Docker is the one thing the gate needs besides Rust.
- From 19 a logical slot turns logical decoding on under `wal_level =
  replica` (PostgreSQL `67c20979ce7`), so the start check reads
  `server_version_num` and accepts `replica` there. Below 19 the check
  runs before the slot, since PostgreSQL's own error names no fix.
- A quirk of a newer release is found by CI, not by a user.
