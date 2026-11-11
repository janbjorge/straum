# ADR-0014: Facts need REPLICA IDENTITY FULL

Status: Superseded by ADR-0031

## Context

Removing a row from a cell needs the old row. Without it Straum would
keep a row store (ADR-0002 forbids it), read the fact again, or demand
a declared key and an `append` promise. Earlier specs had both a `key`
and an `append` view option.

## Decision

Every fact table needs `REPLICA IDENTITY FULL`, which `straum plan`
prints and every start checks. Updates and deletes apply in place from
the old row the WAL carries. There is no `key` or `append` option, and
a fact table needs no primary key; a spec that still says `WITH (key =
...)` is refused by name. Truncate empties the table in place.

## Consequences

- FULL adds WAL on `UPDATE` and `DELETE` only; insert-only tables pay
  nothing.
- A fact's identity is its whole row, so the mutable-facts question is
  closed without a row store.
- A table that drops FULL fails the start check with the fix named.
