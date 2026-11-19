# ADR-0004: Specs are a strict PostgreSQL CREATE VIEW subset

Status: Accepted 2026-11-10; revised 2026-11-19

## Context

The first spec (2026-11-08) was a notation of Straum's own: its own
parser, its own docs, and no way to check its meaning without Straum.
Agents write specs, not people, and PostgreSQL offers several
spellings for most meanings; each is a path the walker, the docs and
every reader carry, and a choice an agent can get wrong.

## Decision

A spec is Straum SQL: a Straum-only subset of `CREATE VIEW`, one view
per fact, refs joined as ADR-0006 lists, one `buckets(...)` time,
aggregates with `FILTER`, and no view options. The view body, with its
`buckets` item written as `date_bin` at the first width, runs in
PostgreSQL and is the oracle for every test. Straum reads it at start:
no build step, no codegen.

Names resolve as PostgreSQL resolves them, and each meaning has one
spelling with nothing inferred: every schema, alias, column owner,
output name (`AS`) and join kind is written. Where PostgreSQL would
itself infer, the spelling is the one it resolves without a lookup:
`GROUP BY` lists positions, a `FILTER` names columns. Names are
lowercase; a width is in the largest unit that divides it; a position
or parameter has no leading zero; an ask holds no comment or trailing
`;`; a bound is `YYYY-MM-DDTHH:MM:SS` with `Z` or a nonzero offset,
no fraction; a spec is
never empty and never `OR REPLACE`. The canon is the
grammar in `docs/src/spec.md` and `docs/src/asks.md`, whose examples
the tests parse. A second spelling or a default revises this record.

The parser is `sqlparser` with `PostgreSqlDialect`, pinned exactly,
without `recursive-protection` (it builds C). The walker denies by
default: each `match` lists what it accepts, a last arm names the
refused construct and the spelling to write, accepted structs are
destructured with every field and no `..`, and input never panics it.

Buckets: `buckets(alias.column, 'width', ...) AS name` is the view's
time, required and written once. It names the time column, then every
width kept, narrowest first, each a whole multiple of the one before;
each row goes in a bucket of the first, a grid from the epoch in UTC.
The name is the view's own, sharing the namespace of keys and
measures, and asks group and bound the time by it. Each fact is
written once: until 2026-11-19 the narrowest width was written twice,
in `date_bin` and in `WITH (buckets = ...)`, and the time was named
`bucket` by rule. Straum keeps a level of cells per width, updated in
the same commit; an ask reads the widest that fit and the narrowest at
its ends. Merging up at ask time took 1.4 s on the taxi month by
borough, against 6.6 to 8.8 ms.

A `timestamp` is wall clock: read as the Unix seconds its reading would
be in UTC (`ColumnType::Time { zoned }`). The time column is either
type; the catalog says which. The oracle's `date_bin` takes an untyped
`'epoch'`, which PostgreSQL reads as the column's type, so a
`timestamp` is never cast through the session zone. No `AT TIME ZONE`.
`infinity` reads as a null time, a bucket no ask reads.

Types are listed, the rest refused naming the type: the integer types,
`numeric(p, s)` with `p` at most 18, `boolean`, `text`, `varchar`,
`timestamptz`, `timestamp`, `date`, `uuid`, and an enum as text. A
`date` or `uuid` is a key, counted and counted distinct, never summed
or taken the least or greatest of, as text is. Its literal or
parameter is a string spelled as PostgreSQL prints it under
`DateStyle = ISO`, and only so: `2024-01-15`, `0044-03-15 BC`,
`infinity`, a uuid in lowercase 8-4-4-4-12 hex. `2024-1-15`, uppercase
or braces are refused, saying how to write it. A date keeps `infinity`
as a value, unlike a time, since PostgreSQL groups by it. `real` and
`double precision` are refused wherever the view reads them: a float
sum moves with the order it was added in and can't take a delete
exactly, so it can't equal PostgreSQL's.

Null-ness is a key: `(f.col IS NULL) AS name`, or `IS NOT NULL`, is a
boolean key. A `FILTER`'s `IS NULL` counts through that key and is
refused without it; never over the time column, nowhere else.

An enum is read as `alias.column::text AS name`, the only cast, on a
key or `FILTER` column. The catalog check refuses the cast on a
non-enum, an enum without it (naming it), and a `FILTER` literal that
is no label in `pg_enum`. Values are labels as text; the type's name
is metadata in `plan` and `/status`, never the
domain's. Mapping enums to strings by catalog class, as Debezium and
PeerDB do, was rejected as inference.

## Consequences

- A view body pasted into `psql` gives Straum's answer once its
  `buckets` item is written as `date_bin('<first width>', column,
  'epoch')`; as written, PostgreSQL refuses it, having no `buckets`
  function, rather than answering otherwise. The tests make that
  rewrite on the text, not through Straum's parser.
- Refused where PostgreSQL accepts: bare `JOIN`, `LEFT JOIN` without
  `OUTER`, `USING`, `NATURAL`, `IN (SELECT ...)`, `count(1)`, `GROUP
  BY` by name or expression, an item without `AS`, a bare column, a
  table without schema or alias, any expression. Each refusal has a
  negative test.
- A `sqlparser` upgrade that adds a field fails to compile until the
  walker handles it.
- Calendar grains, custom origins, sub-second widths and ask-time
  zones are out; a zone over `timestamp` has no instant.
- A null-ness key keys cells on two values, not a column's cardinality.
- Enum labels sort as text. `ADD VALUE` costs nothing; `RENAME VALUE`
  is not streamed and stops Straum at the next miss (ADR-0009),
  as does an unmapped type OID. Two enum columns can't be joined.
