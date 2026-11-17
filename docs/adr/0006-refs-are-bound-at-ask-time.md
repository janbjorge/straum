# ADR-0006: Refs are bound at ask time

Status: Accepted 2026-11-11; revised 2026-11-17

## Context

Before M12 Straum held dim rows and joined in memory: 897 MiB on a
warehouse copy. M12 resolved dims in PostgreSQL as fact rows came in
and keyed cells by dim values: 84 MiB, but a dim change left old cells
as they were until a fact row changed and Straum started over. No dim
is immutable. The floor for a join view kept outside the database is
the fact at join-key grain plus each dim projected to its key and the
columns read (Quass et al. 1996; `research/JOINS.md`).

## Decision

A view is a tree of refs rooted at the fact. A ref joins columns of
the fact, or of a dim joined before it, to a dim's key, as `INNER
JOIN`, `LEFT OUTER JOIN`, `EXISTS` or `NOT EXISTS`. `RIGHT` and `FULL`
add dim rows no fact points at, in no bucket, so the walker names the
kind to write instead.

Keyed cells are keyed by the fact-side join columns. Each dim is held
projected to its key and the columns views read, fed from the stream
like a fact. Nothing is looked up after the load. Any number of dim
rows may match a key, and a cell counts once per match as PostgreSQL's
join repeats the row: counts, sums and percentile counts scale; min,
max, `bool_and`, `bool_or` and count distinct don't. An inner or semi
ref with no match drops the cell, an anti ref with one drops it, a
left ref reads nulls. The view's `WHERE` stands on every ask.

A fact keeps bound cells, a second rollup, only when a ref's via
column is a rollup-key part no `by` key is. They are keyed by the `by`
keys, every column its refs read and the bucket, with the same widths
and measures. A fact row goes into both rollups, into the bound cells
under the dim rows it matches now. Asks read bound cells. Every other fact with refs,
and any ref joined through another dim, binds keyed cells at ask time.

A dim row is applied as it comes. When the rows under a key change,
the store walks each reading fact's hot keyed cells and moves those
naming the key (`Store::retarget`), resolving join rows under the old
rows, then the new. A dim truncate rebuilds bound cells whole. A
batched pass per commit was dropped: it held changed keys until it ran.

## Consequences

- A dim change is seen by the next answer. Nothing stops.
- Every join kind is exact, chains included; a dim needs no unique
  index, only `REPLICA IDENTITY FULL` (ADR-0007).
- Memory follows distinct join ids plus projections: the warehouse
  holds 12 million cells a width and 2 GiB, where M12 held 312 thousand
  and 84 MiB. That is the cost of the question.
- Without bound cells, a warehouse ask took 21.8 s over all time and
  31.7 ms over a day, against PostgreSQL's 3.2 s and 127 ms. With them,
  a joined view asks like an unjoined one.
- Bound cells only where a join adds a key: the benchmark went from 480
  to 296 MiB, and lock hold from 2.9 to 0.6 ms per 1,000 rows. An inner
  join on a key asks 5 to 24% slower; a dim key held twice puts the
  view on the per-row loop, three to nine times slower.
- A commit changing many dim rows walks the hot keyed cells once per
  row. The load resolves each fact row once more for bound cells.
