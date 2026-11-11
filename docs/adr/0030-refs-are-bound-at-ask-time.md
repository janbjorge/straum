# ADR-0030: Refs are bound at ask time

Status: Accepted, 2026-11-11; amended by ADR-0038

## Context

ADR-0013 resolved a view's dim columns in PostgreSQL as each fact row
came in and keyed the cells by their values. A dim row changed later
left every cell made before as it was, until a fact row of it changed
and Straum started over. No dim is immutable, and a per-dim choice of
binding, or a flip at run time, fits the solution to the data. The
floor for a join view maintained outside the database is the fact at
join-key grain plus each dim projected to its key and the columns the
view reads (Quass, Gupta, Mumick and Widom, 1996; research/JOINS.md).

## Decision

A view is a tree of refs rooted at the fact. A ref joins columns of
the fact, or of a dim joined before it, to a dim's key, and is one of
four kinds: `INNER JOIN`, `LEFT OUTER JOIN`, `EXISTS` and `NOT
EXISTS`. Cells are keyed by the fact-side join columns. Straum holds
each dim projected to its key and the columns the views read, fed from
the stream like a fact, and an ask reads the dim columns through the
cell's key as the dim is now. Nothing is looked up after the load.

Any number of dim rows may match a key. A cell counts once per match,
as PostgreSQL's join repeats the row: count and sum scale, min, max
and count distinct do not. An inner or semi ref with no match drops
the cell, an anti ref with one drops it, a left ref reads nulls. The
view's `WHERE` stands on every ask. `RIGHT` and `FULL` would add dim
rows no fact points at, in a bucket no ask reads, so they answer as
`INNER` and `LEFT OUTER`; the walker says which to write.

## Consequences

- A dim change is one projection entry, seen by the next answer.
  Nothing starts over; the ADR-0015 case of a fact row bound to an old
  dim value is gone.
- Memory follows distinct join ids, not the dim columns' few values,
  plus the projections: on the warehouse copy of docs/src/sizing.md,
  12 million cells a width and 2 GiB where early binding held 312
  thousand and 84 MiB. That is the cost of the question, not of a
  shortcut.
- Every join kind is exact at ask time, chains included, and a dim
  needs no unique index.
- Dims are published with `REPLICA IDENTITY FULL` (ADR-0031), and the
  load is one plain `COPY` per table.
- Supersedes ADR-0013. ADR-0002 still holds for facts: cells, and
  beside them only what a join needs of a dim.
