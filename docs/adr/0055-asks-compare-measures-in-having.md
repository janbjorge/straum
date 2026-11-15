# ADR-0055: Asks compare measures in HAVING

Status: Accepted, 2026-11-15

## Context

An ask could filter rows by key with `=` and `IN` (ADR-0006), but not
keep the groups whose measure passes a bound, such as the boroughs whose
median fare is over 20. A client had to fetch every group and filter
itself. A fraction like 20.5 had no exact parameter: JSON numbers with a
fraction were refused, since a float would round them.

## Decision

An ask may end in `HAVING measure op $n [AND ...]`, `op` one of
`= <> < <= > >=`. The measure must be one the ask selects, so the
answer shows every value a group was kept for. A key goes in `WHERE`
and a measure in `HAVING`; each place refuses the other and names the
right one. Straum applies `HAVING` to the finished groups, after the
cell loop, and compares exactly: a sum against a parameter at either
scale, an average as its sum over its count, with no float. A null
measure meets no comparison. A boolean measure takes `=` and `<>`.

A JSON number with a fraction or exponent is a parameter, read from its
written digits (`serde_json`'s `arbitrary_precision`), with up to 38
digits after the point. It compares with measures only; a key still
takes the integer, string or boolean of its type.

## Consequences

- Asks that filter groups answer in Straum instead of in the client.
- The cell loop and the store are unchanged; `HAVING` costs one pass
  over the answer's groups.
- A measure not selected can't be compared. Allowing it later breaks no
  ask; the reverse would.
- `!=`, `OR`, parentheses and expressions stay refused (ADR-0029).
- `straum ask` and the shell send `20.5` as a number and `'20.5'` as a
  string.
