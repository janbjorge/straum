# ADR-0019: Null-ness is a key

Status: Accepted (retroactive), 2026-11-10

## Context

A nullable column such as `completed` means something by being null.
Straum has no expressions, and grouping by the column itself would key
cells on every distinct value to answer a yes-or-no question.

## Decision

`(f.col IS NULL) AS name` or `IS NOT NULL` is a boolean key, selected
and grouped like a column whatever the column's type. A `FILTER`'s `IS
NULL` or `IS NOT NULL` counts through the null-ness key over that
column and is refused when there is none. Asks filter the key by name
with a boolean. Over the time column it is refused. No `IS NULL`
elsewhere.

## Consequences

- The view body stays what PostgreSQL runs (ADR-0004).
- Cells key on two values, not the column's cardinality.
- A spec that wants both the value and its null-ness selects both.
