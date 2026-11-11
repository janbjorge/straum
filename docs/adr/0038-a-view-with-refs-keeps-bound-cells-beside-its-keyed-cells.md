# ADR-0038: A view with refs keeps bound cells beside its keyed cells

Status: Accepted, 2026-11-11

## Context

ADR-0030 keys cells by the fact-side join columns and reads dim
columns at ask time through the projections. On the warehouse copy a
group holds a task or two, so the view holds a cell a task, and an ask
resolves every cell it covers: 21.8 s over all time and 31.7 ms over a
day, against 3.2 s and 127 ms in PostgreSQL, where the early-bound
view of M12 held 66 thousand day cells. The keyed cells and the
projections are still the least an exact maintainer holds
(research/JOINS.md, section 5).

## Decision

A fact whose refs all join on fact columns keeps a second rollup, its
bound cells, keyed by the `by` keys, every column its refs read and
the bucket, with the same widths and measures. A fact row goes into
both: into the keyed cells as it is, and into the bound cells under
the dim rows its join columns match now, the join kind applied as an
ask applies it. Asks read the bound cells. A bucket whose bound cells
are sealed is read through its keyed cells, as before.

A dim change moves bound cells, from the keyed ones. A projection
keeps the rows a changed key held when the commit began. Before the
commit's next fact change, and at its end, one pass over the keyed
cells unbinds each cell whose join columns name a changed key, under
those rows, and binds it again under the rows held now. The pass
compares codes and resolves nothing. A dim truncate rebuilds the
bound cells whole.

## Consequences

- An ask over a joined view costs what one over an unjoined view
  costs: the cells of its distinct key values, not one per join id.
- The keyed cells stay, so memory is ADR-0030's plus the bound cells,
  which are few. The load resolves each fact row once more.
- A commit that changes a column a view reads costs one pass over the
  keyed cells; one that changes no such column costs nothing. Nothing
  starts over.
- A ref joined through another dim's column keeps ask-time binding
  until a record admits it; its fact keeps no bound cells.
- Amends ADR-0030. ADR-0002 holds: bound cells are cells.
