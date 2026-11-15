# ADR-0056: A dim change moves bound cells as each row is applied

Status: Accepted (retroactive), 2026-11-15

## Context

ADR-0038 batches every changed dim key into one pass over the keyed
cells, run before the commit's next fact change and again at its end,
comparing codes and resolving nothing. The store applies each dim row
as the commit reaches it. One walk per row costs more when a commit
changes many dim rows. One walk per commit needs the changed keys kept
until the pass runs.

## Decision

Each dim row is applied as it comes. When the rows under its key
change, the store walks the hot keyed cells of each fact that reads
the dim and moves the cells whose join columns name that key
(`Store::retarget`). The walk resolves the join rows under the rows
the key held, then under the projection as it is. A dim truncate
still rebuilds the bound cells whole.

## Consequences

- A commit that changes many dim rows walks the hot keyed cells once
  for each changed row. The answers match the one pass ADR-0038
  describes.
- The walk resolves join rows. It does not compare codes alone.
- Nothing starts over, as ADR-0038 has it.
- Amends the pass in ADR-0038. ADR-0042 still chooses which facts
  keep bound cells.
