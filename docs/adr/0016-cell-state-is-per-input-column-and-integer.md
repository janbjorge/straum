# ADR-0016: Cell state is per input column and integer

Status: Accepted (retroactive), 2026-11-10; amended by ADR-0037 and
ADR-0041

## Context

Each measure could keep its own accumulator behind a trait, and sums
could take floats. Measures over one column share most of their state,
a trait puts dynamic dispatch on the per-row path, and float sums
depend on order and can't be removed from exactly.

## Decision

Cell state is kept per input column and shared by measures: sum and
avg share sum and count, min and max share value counts. One enum of
cell states, no accumulator trait. Sum, avg, min and max read
integers: integer columns, and `numeric(p <= 18, s)` as scaled `i64`,
with scale carried per column. Sums are `i128`. Float columns are
rejected. Filtered measures keep no state: their conditions name
rollup-key parts only, so asks apply them to cells.

## Consequences

- An `i128` sum can't overflow below 2^64 rows.
- Answers print exact decimals; only the source knows column types, so
  a load sets each column's `ColumnType`, and the same enum is read by
  `straum-pg`.
- The readable types are integer, fixed `numeric(p <= 18)`, boolean,
  text, `timestamp` and `timestamptz`. `uuid`, `date`, enums and wider
  numerics are open.
- Count distinct over a high-cardinality column holds every value; its
  memory is an open question.
