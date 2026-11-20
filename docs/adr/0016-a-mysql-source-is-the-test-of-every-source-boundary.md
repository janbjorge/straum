# ADR-0016: A MySQL source is the test of every source boundary

Status: Accepted 2026-11-20

## Context

PostgreSQL is the only source Straum reads (ADR-0007), and the only
one planned. But every way of following it (logical decoding, the
physical WAL and the ways still to come) puts PostgreSQL's words
within reach of the core: LSNs, slots, xids, snapshots, type OIDs. Once
`straum-app` or `straum-domain` holds one, a second source means
rewriting the core, and the core is where the tests and the cost
budget live.

A question asked at each boundary keeps them out cheaply, while the
boundary is still being drawn.

## Decision

Before a change to the `Source` port, or to what crosses it, ask: could
a MySQL source plug in here without touching `straum-app` or
`straum-domain`? If not, the change leaks PostgreSQL and is redrawn.

A source hands over `Commit`, `Change` and `Datum`, and positions and
backlog as values the core only orders and compares. Everything of
PostgreSQL stays in `straum-pg`: replication, slots, LSNs, xids,
snapshots, OIDs, wire formats. A source turns its values into
`Datum`'s meanings, in its own crate.

MySQL is the test because it has its own form of each: a binlog
position or GTID set instead of an LSN, `START TRANSACTION WITH
CONSISTENT SNAPSHOT` instead of an exported snapshot, its own types and
zero dates. No MySQL source is built or planned.

## Consequences

- The port's `Lsn`, `Wal` and slot words fail the test today; they
  become a position and a backlog.
- `Datum` keeps meanings taken from PostgreSQL, dates from 2000 with
  infinities and uuids in its order; another source converts into
  them, never the other way.
- A boundary costs a little more to draw: a neutral name, a conversion
  in the source crate.
- The test is a question, not a check script; review asks it.
