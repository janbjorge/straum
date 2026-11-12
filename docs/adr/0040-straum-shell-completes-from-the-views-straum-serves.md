# ADR-0040: straum shell completes from the views Straum serves

Status: Accepted, 2026-11-12

## Context

`straum views` and `straum ask` (ADR-0039) answer one command each. To
look around, a person wants a prompt: list the views, see a view's
rows, write an ask without typing every name and bound. An ask needs
both bounds as parameters, on bucket edges, so even "show me this
view" took a `straum views`, a query and two timestamps.

## Decision

`straum shell` is a prompt at a running Straum. `\d` prints the views,
`\rows VIEW` prints the rows PostgreSQL would give for the view, and
any other line is an ask written `QUERY; PARAMETER...`.

Straum serves what the shell needs. `GET /views/NAME` answers the ask
`App::rows_ask` builds: every measure by the bucket and every key over
the buckets held, as a table, the first 1000 rows. `GET /views` with
`Accept: application/json` is the views as JSON, and the shell
completes from it by where the cursor is: a view after `FROM`, the
view's names after `SELECT`, a whole condition with the next `$n`
after `WHERE`, the selected keys after `GROUP BY`, the bucket range
held where a parameter is a bound.

The terminal is `rustyline` with only its history file: raw mode,
line editing and history, which `std` lacks and which take `unsafe`
to write. Without a terminal at both ends the shell reads lines from
its input, as a script feeds them.

## Consequences

- The shell knows no spec and parses no Straum SQL: it splits a line
  into words. A completion can offer what the walker then refuses.
- `\rows` reads every cell of the view, as an ask over its whole
  range does, then prints 1000.
- The binary gains `rustyline` and reads JSON with `serde_json`, which
  `straum-http` already builds.
- The terminal glue runs only at a terminal, so no test covers it;
  the completion under it is plain functions with tests.
- Amends ADR-0039: `/views` has a JSON form, for programs.
