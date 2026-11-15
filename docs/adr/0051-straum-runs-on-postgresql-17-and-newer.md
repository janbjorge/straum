# ADR-0051: Straum runs on PostgreSQL 17 and newer

Status: Accepted, 2026-11-15

## Context

ADR-0046 lowered the floor to 16 because the logical stream reads
nothing 17 added. The physical stream (ADR-0049) does: 16 logs pruning
and vacuuming as their own heap records, which 17 folded into one prune
record. Keeping 16 meant a second prune parser, its tests and a CI job
per major for one release that leaves support in November 2028.

## Decision

PostgreSQL 17 and newer, for both streams, no upper bound. The start
check refuses an older server and names the floor. CI runs the tests on
17, 18 and 19rc1 (19 once it ships), picked by `STRAUM_TEST_POSTGRES`;
locally they run on 18. Supersedes ADR-0046, and narrows the majors
ADR-0049 parses to 17, 18 and 19.

## Consequences

- A managed server still on 16 can't run Straum until it upgrades.
- One prune layout to read, from 17 on; 19 only moves its flags.
- One CI job fewer in the test and correctness matrices.
- The floor moves again when 17 leaves support, with a record
  superseding this one.
