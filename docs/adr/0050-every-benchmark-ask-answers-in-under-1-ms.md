# ADR-0050: Every benchmark ask answers in under 1 ms

Status: Accepted, 2026-11-14

## Context

An ask that merges a number a cell answers in well under a millisecond
over the taxi trips. A percentile gathers and sorts every value in its
range (ADR-0048), and its asks take tens of milliseconds. Without a
stated target, a new aggregate can be accepted at any speed.

## Decision

Every ask in `bench/taxi.py` answers in under 1 ms, the HTTP round
trip included, on the machine `benchmark.md` names. An aggregate whose
asks miss it is not done: it stays on the roadmap below until it
meets it. A new aggregate gets an ask there before it lands.

## Consequences

- The percentile asks miss the target today; `benchmark.md` shows by
  how much. The roadmap, each step measured before the next:
  1. Count what an ask does: cells read, pairs gathered, time in the
     gather and in the sort.
  2. Pick the rank without the sort: each cell's values are already
     in order, so count up to the rank across them.
  3. A running count per run, so a rank is a binary search on value
     instead of a walk (amends ADR-0043).
  4. If still above 1 ms, keep counts per wider bucket across keys,
     so an ask reads fewer, larger counts.
- Asks over views with a cell per row, as the warehouse runs in
  `benchmark.md`, are outside the target: their time follows cells
  read, whatever the aggregate.
- The target is wall time on one machine, so `tests/cost.rs` keeps
  guarding counts and bytes, which hold on any machine.
