# ADR-0004: Specs are a strict PostgreSQL CREATE VIEW subset

Status: Accepted 2026-11-10; revised 2026-11-17

## Context

The first spec (2026-11-08) was a notation of Straum's own: its own
parser, its own docs, and no way to check its meaning without Straum.
Agents write specs, not people, and PostgreSQL offers several
spellings for most meanings; each is a path the walker, the docs and
every reader carry, and a choice an agent can get wrong.

## Decision

A spec is Straum SQL: a Straum-only subset of `CREATE VIEW`, one view
per fact, refs joined as ADR-0006 lists, a `date_bin` bucket,
aggregates with `FILTER`, and `WITH (buckets = ...)` as the only
option. The view body runs unchanged in PostgreSQL and is the oracle
for every test. Straum reads it at start: no build step, no codegen.

Names resolve as PostgreSQL resolves them, and each meaning has one
spelling with nothing inferred: every schema, alias, column owner,
output name (`AS`) and join kind is written. Where PostgreSQL would
itself infer, the spelling is the one it resolves without a lookup:
`GROUP BY` lists positions, a `FILTER` names columns. Names are
lowercase; a width is in the largest unit that divides it; a position
or parameter has no leading zero; an ask holds no comment or trailing
`;`; a bound is `YYYY-MM-DDTHH:MM:SS` with `Z` or an offset; a spec is
never empty and never `OR REPLACE`. The canon is the
grammar in `docs/src/spec.md` and `docs/src/asks.md`, whose examples
the tests parse. A second spelling or a default revises this record.

The parser is `sqlparser` with `PostgreSqlDialect`, pinned exactly,
without `recursive-protection` (it builds C). The walker denies by
default: each `match` lists what it accepts, a last arm names the
refused construct and the spelling to write, accepted structs are
destructured with every field and no `..`, and input never panics it.

Buckets: `date_bin` from the epoch in UTC sets the narrowest width.
`WITH (buckets = '...')` is required and lists every width kept,
narrowest first, equal to the `date_bin` width, each a whole multiple
of the one before. Straum keeps a level of cells per width, updated in
the same commit; an ask reads the widest that fit and the narrowest at
its ends. Merging up at ask time took 1.4 s on the taxi month by
borough, against 6.6 to 8.8 ms.

A `timestamp` is wall clock: read as the Unix seconds its reading would
be in UTC (`ColumnType::Time { zoned }`). The `date_bin` origin matches
the column's type, `timestamp 'epoch'` or `timestamptz 'epoch'`. No
`AT TIME ZONE`. `infinity` reads as a null time, a bucket no ask reads.

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

- A view body pasted into `psql` gives Straum's answer. The `CREATE
  VIEW` with its options is not valid PostgreSQL; docs and errors say
  so.
- Refused where PostgreSQL accepts: bare `JOIN`, `LEFT JOIN` without
  `OUTER`, `USING`, `NATURAL`, `IN (SELECT ...)`, `count(1)`, `GROUP
  BY` by name or expression, an item without `AS`, a bare column, a
  table without schema or alias, any expression. Each refusal has a
  negative test.
- A `sqlparser` upgrade that adds a field fails to compile until the
  walker handles it.
- The narrowest width is written twice, in the option and `date_bin`,
  on purpose. Calendar grains, custom origins, sub-second widths and
  ask-time zones are out; a zone over `timestamp` has no instant.
- A null-ness key keys cells on two values, not a column's cardinality.
- Enum labels sort as text. `ADD VALUE` costs nothing; `RENAME VALUE`
  is not streamed and stops Straum at the next miss (ADR-0009),
  as does an unmapped type OID. Two enum columns can't be joined.
