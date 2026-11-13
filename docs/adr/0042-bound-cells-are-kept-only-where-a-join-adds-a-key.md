# ADR-0042: Bound cells are kept only where a join adds a key

Status: Accepted, 2026-11-13

## Context

ADR-0038 gives every fact whose refs join on fact columns a second
rollup and calls its bound cells few. They are few when a ref joins
on a column that is no `by` key: the keyed cells are then one per
join id and the bound cells one per dim value. A view that selects
its join column as a key has keyed cells no finer than its `by` keys,
so each bound cell copies one keyed cell. The benchmark's two views
are such views: 1,157,581 and 713,457 cells, each held twice.

## Decision

A fact keeps bound cells only when a ref's via column is a part of
the rollup key that no `by` key is. Any other fact with refs answers
from its keyed cells, bound at ask time as ADR-0030 has it.

The loop that walks each cell once takes every join kind while each
dim key holds one row: it passes over a cell whose key finds no row
under an inner or semi join, and one whose key finds a row under an
anti join. A key with several rows under a left or inner join, or a
join through another dim, still walks each cell once per row its
joins find.

## Consequences

- A view joined on its own keys holds its cells once: the benchmark
  holds 296 MiB where it held 480, and its asks take what they took.
- A dim change costs such a view nothing; the next ask reads the dim
  as it is. A changed fact row costs it one rollup, not two: the
  benchmark's lock hold fell from 2.9 to 0.6 ms per 1,000 rows.
- An inner join on a key answers 5 to 24% slower than its bound cells
  did. The loop that repeats cells took three to nine times as long.
- Where a dim holds a key twice, a view joined on its own keys is
  read by that slower loop, where bound cells answered before.
- Amends ADR-0038, which holds for every other fact.
