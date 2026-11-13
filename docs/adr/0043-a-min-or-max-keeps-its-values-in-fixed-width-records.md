# ADR-0043: A min or max keeps its values in fixed-width records

Status: Accepted, 2026-11-13

## Context

A sum or an average takes a deleted row by subtracting it. A min or
max can't: when the row that holds the max leaves, the next value is
not in that row, and Straum keeps no rows to look for it. So a cell
keeps every value it holds, with a count (ADR-0016). Values repeat
little inside a cell: `max(fare_amount)` by hour and dropoff zone
keeps 14,943,826 fares for the 20,332,093 taxi trips. Each took 16
bytes as a pair of integers, or about 31 in a tree, and the view held
447 MiB where it holds 146 with a sum. A cell's values lie close
together, so most of those bytes were zeros.

## Decision

A cell keeps its values as records in a run: a value less the run's
base, in as many bytes as the widest of the run takes, then its count
less one likewise. The widths are fixed within a run, so a value is
found by halving, and a value or a count too wide for its run has the
run encoded anew. A few values stay in the cell itself, where a run's
pointer would be. A run holds at most 4 KiB, and a cell with more
keeps several in a tree.

Records of varying length, each value as its gap to the one before,
would be about 30% smaller on the fares. But a value is then found by
walking its run, at every row added or removed.

## Consequences

- The view holds 222 MiB: 5.3 bytes a value all told, where it was
  21.1. 64% of its cells keep their values in themselves.
- A change costs what it did. An update of 100,000 rows in a few
  hundred cells holds the write lock 66 ms against 67, and one of
  20,000 rows all over the table 29 ms against 36. The load takes as
  long.
- Answers don't change. An ask reads each cell's two ends, mirrored
  beside the records as before.
- `count(DISTINCT c)` still keeps a hash map per cell.
