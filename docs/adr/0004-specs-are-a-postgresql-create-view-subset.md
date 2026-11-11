# ADR-0004: Specs are a PostgreSQL CREATE VIEW subset

Status: Accepted (retroactive), 2026-11-10; amended by ADR-0029

## Context

A spec names what Straum aggregates. The first design (2026-11-08) was a
notation of Straum's own. It needed its own parser, its own docs, and
users had no way to check what a spec meant without Straum.

## Decision

A spec is Straum SQL: a small subset of PostgreSQL `CREATE VIEW` syntax
made only for Straum, one view per fact, `LEFT JOIN` refs, a `date_bin`
bucket, aggregates with `FILTER`, `GROUP BY` naming the selected keys,
and `buckets` as the only view option. Each view body runs unchanged in
PostgreSQL, so it is the reference for every test. Straum reads the spec
at start; there is no build step and no codegen.

The parser is the `sqlparser` crate with `PostgreSqlDialect`, pinned to
an exact version, without its C-building `recursive-protection`
feature. Straum walks the AST and denies by default: each `match` lists
what it accepts, a final arm names the rejected construct, accepted
structs are destructured with every field named and no `..`, and the
walker never panics on input.

## Consequences

- Every doc and error says Straum SQL is a straum-only subset. The
  `CREATE VIEW` with its options is not valid PostgreSQL; only the body
  is.
- A `sqlparser` upgrade that adds a field fails to compile until the
  walker handles it.
- Each rejected construct gets a negative test.
- No expressions, no `AT TIME ZONE`, no arbitrary joins. What the subset
  lacks is refused by name, never ignored.
