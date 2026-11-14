# ADR-0048: A percentile reads the counts min and max keep

Status: Accepted, 2026-11-14

## Context

Straum refused median and percentiles. Other systems answer them with a
sketch, but t-digest and KLL can't take a delete, and every sketch's
answer is an estimate, where a spec reads as PostgreSQL would
(ADR-0005). A cell already keeps a count per value for `min` and `max`
(ADR-0016, ADR-0043), and a sealed bucket keeps the same.

## Decision

Straum SQL takes `percentile_disc(fraction) WITHIN GROUP (ORDER BY
alias.column)`, one fraction from 0 to 1 written as digits. It reads
the input a `min` or `max` on that column keeps, adding none. An ask
gathers the value counts of the cells in its range, per group, sorts
them and returns the value at rank `ceil(fraction * n)`, at least 1,
the product in float8 as PostgreSQL takes it: 0.07 of 100 values is
the 8th there, where exact math gives the 7th. `percentile_cont`,
`median`, `mode`, an array of fractions, `ASC` and `DESC` are refused,
naming the construct (ADR-0029).

A sketch was rejected: its answer would differ from PostgreSQL's under
a PostgreSQL name. `percentile_cont` waits: it interpolates in float8,
and an exact answer would differ from PostgreSQL's in the last digits.

## Consequences

- A percentile holds no byte beyond a `min` or `max` on its column. A
  view without one pays for every distinct value a cell holds, as a
  `min` or `max` would.
- An ask costs by the values in its range, not a number a cell: a long
  range over many values takes milliseconds where a sum takes
  microseconds. The benchmark's percentile asks track it.
- If that is too slow, a running count per run makes the pick a binary
  search on value. That changes ADR-0043's records and gets a record
  of its own.
- A change to the column moves a value between counts under the write
  lock, as for a `min` or `max`.
- A dim key held twice counts its rows' values twice, as it does
  `count`.
