# ADR-0015: Straum writes the copy and psql runs it

Status: Accepted 2026-11-19

## Context

Trying Straum on real data needs the spec's tables in a throwaway
PostgreSQL. `straum copy` (v0.11.0 to v0.11.2) did it with Straum's own
client: one read-only snapshot, the columns and enums, the rows. That
client signs in with SCRAM-SHA-256 or trust only, so a server asking
for another method, as an Azure server did, stopped it. Teaching it
every method libpq knows would grow a second client beside the one the
stream needs.

`pg_dump` signs in every way, but a dump of a table's rows also reads
each sequence the table owns. A role with `SELECT` on the tables alone
fails, and no flag skips it (`--exclude-table-data`, a `--filter` file
and `--data-only` were tried on PostgreSQL 18). `pg_dump -t` also
leaves out the enums and extensions a table uses, so the restore fails
on its first `CREATE TABLE`. `bench/try_your_data.py` then copied with
`pg_dump --schema-only` and one `psql` session per table: no shared
snapshot, every column of every table, and the whole schema's
definitions, whose errors buried the one that mattered.

## Decision

Straum copies no data. `straum plan --copy DIR` writes two psql scripts
from one or more specs, and psql runs them. Straum reads no database
for it.

`export.sql` runs on the source in one `REPEATABLE READ READ ONLY`
transaction, with `row_security` off and a lock timeout. A `DO` block
checks every column first and fails naming each it can't copy: a table
or column the source lacks, one the role can't read, a type neither
built in nor an enum. A query on the catalog writes `schema.sql` with
the schemas, the enums and the tables, with only the columns the specs
read; a domain becomes its base type. `\copy (SELECT columns) TO`
writes one CSV per table. `load.sql` creates the tables and loads the
CSVs in one transaction. Both stop at the first error, fix `DateStyle`,
`IntervalStyle`, `bytea_output` and `extra_float_digits`, and take
`-v dir=PATH`.

Several specs merge by table: a table comes once, with every column any
of them reads. The setup SQL still takes one spec, since one
`straum run` serves one. `bench/try_your_data.py` and
`docs/compose/try.yaml` load such a directory.

Rejected: auth methods beyond SCRAM in Straum's client, a second client
for one task; `pg_dump`, which fails for a read-only role; DuckDB and
`pgcopydb`, a dependency each where psql is in every PostgreSQL install.

## Consequences

- Any sign-in libpq takes works, and a role needs `SELECT` on the
  columns read, nothing on the sequences.
- The copy is one snapshot and only the columns read, so it is smaller
  than the tables.
- The CSVs open in any tool, not only PostgreSQL.
- A copy takes two commands and psql where `straum copy` took one.
- Progress is a line per table and psql's row count, not a percentage.
- Indexes, keys and defaults stay behind, as before.
