# ADR-0006: Asks are Straum SQL with parameters

Status: Accepted (retroactive), 2026-11-10; amended by ADR-0029

## Context

Asks were JSON fields (`fact`, `measures`, `by`, `time`, `filter`,
`from`, `to`) until 2026-11-10. A second language for asks meant a
second set of names and docs for what the spec already said in SQL.
Speaking pgwire instead would make Straum look like a database it isn't.

## Decision

`POST /ask` takes `{"query", "parameters"}`. The query is Straum SQL:
one `SELECT` over a view, keys then measures by name, `=` and `IN`
joined by `AND`, and `bucket >= $n AND bucket < $m`; `GROUP BY` names
exactly the selected keys. Every value is a `$n` parameter; a literal in
the query is an error, so values never become query text. The client
names the view; Straum never picks one.

## Consequences

- Like view options, an ask is not valid PostgreSQL: measures go by
  bare name and refs are in scope without a join.
- `bucket` is reserved: a spec can't name a column, ref or measure so.
- Either bucket bound may be left out; the ask then reads from the
  first bucket held, or to the last.
- A filter value must have its column's type, with no conversion: an
  integer for an integer, a string for text, a boolean for a boolean.
  Anything else is `AskError::WrongType`, not an empty answer.
- `straum-notation` walks an ask like a spec; `straum-http` reads only
  the JSON around it (ADR-0003).
- No backward compatibility was kept; Straum wasn't live.
