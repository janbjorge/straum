# ADR-0005: A spec reads as PostgreSQL would

Status: Accepted (retroactive), 2026-11-10; amended by ADR-0029

## Context

Straum SQL could demand its own conventions: every column qualified,
every table aliased, a schema on every name. That makes a spec a
Straum document rather than a view PostgreSQL would accept as is.

## Decision

Name resolution follows PostgreSQL. A bare column is the fact's. A table
without a schema is in `public`. A table without an alias goes by its
name. A dim's columns go through its alias. `GROUP BY` stays, so every
spec is a view PostgreSQL would accept.

## Consequences

- A user can paste the view body into `psql` and get the same answer
  Straum will give.
- The walker resolves names the way the PostgreSQL planner does, so it
  needs the catalog's schema search semantics only for `public`.
- A spec that works around a PostgreSQL rule is refused rather than
  interpreted.
