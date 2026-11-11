# ADR-0037: An enum column is read as its label, when cast to text

Status: Accepted, 2026-11-11

## Context

ADR-0016 reads integer, fixed numeric, boolean, text and time columns
and leaves enums open. Real schemas key their facts on enums: a
status, a category, a mode. PostgreSQL already serialises an enum as
its label everywhere Straum reads: `enum_out` for pgoutput, `enum_send`
for binary `COPY`. Debezium, PeerDB and Fivetran turn every enum into a
string by catalog class; Materialize refuses unknown types until the
source declares a column text. Straum SQL infers nothing (ADR-0029)
and keeps the domain free of any source's type names.

## Decision

A spec reads an enum column by writing the cast PostgreSQL would:
`alias.column::text AS name`. The walker accepts `::text` and no other
cast, on a key or a `FILTER` column, and nowhere else: not in `ON`,
not in `EXISTS`. The catalog check allows it only on a column whose
`pg_type.typtype` is `e`, refuses an enum column written without it,
naming the cast to write, refuses a cast on a column already text, and
refuses a `FILTER` literal that is no label of the type, read from
`pg_enum`. The adapter maps the column to `ColumnType::Text` and reads
the label bytes as text. Values stay the bare label; the type's
qualified name travels as metadata on the column, in `plan`, `/status`
and spilled schemas. The domain never learns a type name or a label.

## Consequences

- The view body stays what PostgreSQL runs (ADR-0004): `::text` on an
  enum is valid there, so the oracle still compares.
- A value is never bare in Straum: cells key on (slot, bytes), codes
  are per slot, asks and `FILTER` name one column. Two types with the
  same labels cannot meet, and a join between two enum columns is
  refused, as PostgreSQL refuses `=` between two enum types.
- Asks filter a label with a string; answers sort labels as text, not
  in the type's order.
- `ALTER TYPE ... ADD VALUE` costs nothing. `RENAME VALUE` is not
  streamed: cells keep the old label until a delete or update of such
  a row misses and Straum starts over (ADR-0015).
- Enum OIDs are per database, so the type map is built from the
  catalog at start, and a Relation message with an unmapped OID starts
  Straum over, as a type change does.
- `uuid`, `date`, domains and arrays stay refused by name until a
  record admits them. Which classes the cast accepts is a guide line;
  today it is enums alone.
