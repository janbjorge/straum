# ADR-0029: Straum SQL has one spelling per meaning

Status: Accepted, 2026-11-11

## Context

Agents write specs and asks, not people. PostgreSQL offers several
spellings for most meanings: `JOIN` and `INNER JOIN`, `IN (SELECT)` and
`EXISTS`, `GROUP BY` by name or by position, `count(1)` and `count(*)`,
a table with or without its schema, a column with or without its
table. Every extra spelling is a path the walker, the docs and every
reader carry, and a choice an agent can get wrong. ADR-0004 made the
spec a PostgreSQL subset; it did not say how small.

## Decision

Each meaning Straum SQL can express has exactly one spelling, and
nothing is inferred: every schema, alias, column owner, output name,
join kind and ask bound is written. The walker refuses every other form
with an error that names the construct and the one spelling, and fills
in no default. There is no sugar, no convenience form and no second
name for a thing already named. Where PostgreSQL itself would have to
infer, the spelling is the one it resolves without a lookup: `GROUP BY`
lists positions, since an output name there may be read as an input
column, and a `FILTER` names columns, since an output name there is no
column at all.

The canon is the grammar in `docs/src/spec.md` and `docs/src/asks.md`,
whose examples the tests parse. Adding a meaning adds one spelling.
Adding a second spelling for a meaning Straum has, or a default, needs
a record that supersedes this one.

## Consequences

- A spec reads the same wherever it was written, and the walker has
  one path per meaning.
- An error is a fix, not a search: it says what to write.
- Someone who wants PostgreSQL's freedom uses PostgreSQL, or an agent
  to rewrite into the canon.
- Bare `JOIN`, `LEFT JOIN` without `OUTER`, `USING`, `NATURAL`,
  `IN (SELECT ...)`, `count(1)`, `GROUP BY` by name or expression, a
  select item without `AS`, a bare column, a table without schema or
  alias and an ask without both bucket bounds are refused where
  PostgreSQL would accept them.
- Every spec and ask written so far changes. Straum isn't live, so
  nothing is kept compatible.
