//! The psql scripts that copy the tables of one or more specs into
//! another PostgreSQL, which `straum plan --copy` writes. `export.sql`
//! runs on the source in one read-only snapshot: it checks each column,
//! writes `schema.sql` with the schemas, enums and tables, then one CSV
//! per table with only the columns the specs read. `load.sql` runs on the
//! destination and reads them back. psql does the connecting, so any
//! sign-in libpq takes works. Pure text from the specs; nothing here
//! connects.

use postgres_protocol::escape::{escape_identifier, escape_literal};
use straum_domain::{Identifier, Spec, Table};

use crate::catalog::escaped;

/// The two scripts `straum plan --copy` writes.
#[derive(Debug, PartialEq, Eq)]
pub struct CopyScripts {
    /// Runs on the source.
    pub export: String,
    /// Runs on the destination.
    pub load: String,
}

/// One table to copy: every column the specs read of it, once each, in
/// the order first read, and the file its rows go in.
struct Copied {
    schema: Identifier,
    name: Identifier,
    columns: Vec<Identifier>,
    file: String,
}

/// How long the export waits for a lock on a table before it fails.
const LOCK_TIMEOUT: &str = "5s";

/// The relation kinds a copy reads: tables, partitioned tables, views,
/// materialized views and foreign tables, as the spec may name any.
const READABLE_KINDS: &str = "('r', 'p', 'v', 'm', 'f')";

/// Writes both scripts for `specs`. `dir` is where the files go unless
/// psql is given `-v dir=PATH`.
pub fn copy_scripts(specs: &[Spec], dir: &str) -> CopyScripts {
    let copied = copied_tables(specs);
    let needed = needed_values(&copied);
    CopyScripts {
        export: export_script(&copied, &needed, dir),
        load: load_script(&copied, dir),
    }
}

/// The tables of every spec, each once, with the columns read of it.
fn copied_tables(specs: &[Spec]) -> Vec<Copied> {
    let mut copied: Vec<Copied> = Vec::new();
    for spec in specs {
        for fact in spec.facts() {
            add_columns(&mut copied, &fact.table, &fact.columns());
        }
        for dim in spec.dims() {
            let columns: Vec<Identifier> = dim.columns().cloned().collect();
            add_columns(&mut copied, &dim.table, &columns);
        }
    }
    copied
}

fn add_columns(copied: &mut Vec<Copied>, table: &Table, columns: &[Identifier]) {
    let mut index = copied.len();
    for (at, have) in copied.iter().enumerate() {
        if have.schema == table.schema && have.name == table.name {
            index = at;
        }
    }
    if index == copied.len() {
        // Straum SQL takes no quoted identifier, so a name holds no `.` or
        // `/`, and two tables never share a file.
        let file = format!("{}.{}.csv", table.schema, table.name);
        copied.push(Copied {
            schema: table.schema.clone(),
            name: table.name.clone(),
            columns: Vec::new(),
            file,
        });
    }
    let copied_table = &mut copied[index];
    for column in columns {
        if !copied_table.columns.contains(column) {
            copied_table.columns.push(column.clone());
        }
    }
}

/// The columns as SQL rows `(schema, table, column, order)`, which the
/// check and the schema query both join to the catalog.
fn needed_values(copied: &[Copied]) -> String {
    let mut rows = Vec::new();
    for table in copied {
        for column in &table.columns {
            rows.push(format!(
                "({}, {}, {}, {})",
                escape_literal(table.schema.as_str()),
                escape_literal(table.name.as_str()),
                escape_literal(column.as_str()),
                rows.len() + 1
            ));
        }
    }
    rows.join(",\n    ")
}

fn export_script(copied: &[Copied], needed: &str, dir: &str) -> String {
    let mut script = vec![
        "-- Straum export, from straum plan --copy. Run it on the source with psql,".to_owned(),
        "-- as a role that can SELECT the columns it names:".to_owned(),
        "--   psql \"$SOURCE\" -f export.sql".to_owned(),
        "-- It reads one snapshot and writes nothing there. It writes schema.sql and".to_owned(),
        "-- one CSV per table into dir; -v dir=PATH puts them elsewhere.".to_owned(),
    ];
    script.extend(preamble(dir));
    script.extend([
        "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;".to_owned(),
        // A lock held on a table fails the export rather than queue behind
        // it, and every query behind that.
        format!("SET LOCAL lock_timeout = '{LOCK_TIMEOUT}';"),
        // A row security policy would hide rows without a word; off, the
        // query that a policy would filter fails instead.
        "SET LOCAL row_security = off;".to_owned(),
        check_sql(needed),
        "\\set QUIET on".to_owned(),
        "\\pset tuples_only on".to_owned(),
        "\\pset format unaligned".to_owned(),
        "\\o schema.sql".to_owned(),
        schema_sql(needed),
        "\\o".to_owned(),
        "\\pset tuples_only off".to_owned(),
        "\\pset format aligned".to_owned(),
        "\\set QUIET off".to_owned(),
    ]);
    for (index, table) in copied.iter().enumerate() {
        let qualified = escaped(&table.schema, &table.name);
        script.push(progress("exporting", &qualified, index, copied.len()));
        script.push(format!(
            "\\copy (SELECT {} FROM {qualified}) TO {} WITH (FORMAT csv, HEADER)",
            column_list(table),
            psql_string(&table.file)
        ));
    }
    script.push("COMMIT;".to_owned());
    script.join("\n") + "\n"
}

fn load_script(copied: &[Copied], dir: &str) -> String {
    let mut script = vec![
        "-- Straum load, from straum plan --copy. Run it on the destination with".to_owned(),
        "-- psql, after export.sql has written the files:".to_owned(),
        "--   psql \"$DESTINATION\" -f load.sql".to_owned(),
        "-- It creates the tables and loads them in one transaction; the first".to_owned(),
        "-- error stops it and leaves nothing behind. -v dir=PATH reads the files".to_owned(),
        "-- from elsewhere.".to_owned(),
    ];
    script.extend(preamble(dir));
    script.push("BEGIN;".to_owned());
    script.push("\\i schema.sql".to_owned());
    let mut tables = Vec::new();
    for (index, table) in copied.iter().enumerate() {
        let qualified = escaped(&table.schema, &table.name);
        script.push(progress("loading", &qualified, index, copied.len()));
        script.push(format!(
            "\\copy {qualified} ({}) FROM {} WITH (FORMAT csv, HEADER)",
            column_list(table),
            psql_string(&table.file)
        ));
        tables.push(qualified);
    }
    script.push("COMMIT;".to_owned());
    script.push(format!("ANALYZE {};", tables.join(", ")));
    script.join("\n") + "\n"
}

/// Stops at the first error, fixes how values are written as text, so
/// the load reads them back as the export wrote them whatever either
/// server's defaults, and moves into the directory of the files: `dir`
/// unless psql was given another.
fn preamble(dir: &str) -> [String; 13] {
    [
        "\\set ON_ERROR_STOP on".to_owned(),
        "\\encoding UTF8".to_owned(),
        "\\set QUIET on".to_owned(),
        "SET DateStyle = 'ISO, YMD';".to_owned(),
        "SET IntervalStyle = 'postgres';".to_owned(),
        "SET bytea_output = 'hex';".to_owned(),
        "SET extra_float_digits = 1;".to_owned(),
        "\\set QUIET off".to_owned(),
        "\\if :{?dir}".to_owned(),
        "\\else".to_owned(),
        format!("\\set dir {}", psql_string(dir)),
        "\\endif".to_owned(),
        // Unquoted: psql puts the value in whole, spaces and all, where
        // :'dir' would hand \cd the quotes too.
        "\\cd :dir".to_owned(),
    ]
}

fn progress(verb: &str, qualified: &str, index: usize, count: usize) -> String {
    let line = format!("{verb} {qualified}, table {} of {count}", index + 1);
    format!("\\echo {}", psql_string(&line))
}

fn column_list(table: &Copied) -> String {
    let mut names = Vec::new();
    for column in &table.columns {
        names.push(escape_identifier(column.as_str()));
    }
    names.join(", ")
}

/// A single-quoted argument to a psql backslash command, which reads a
/// doubled quote as one and a backslash as the start of an escape.
fn psql_string(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "''"))
}

/// The columns joined to the catalog: each relation, column and the type
/// a copy creates for it. A domain is its base type, so the destination
/// needs no domain.
fn typed_columns(needed: &str) -> String {
    format!(
        "FROM (VALUES\n    {needed}) AS n(nsp, rel, col, ord)
LEFT JOIN pg_namespace s ON s.nspname = n.nsp
LEFT JOIN pg_class c
  ON c.relnamespace = s.oid AND c.relname = n.rel AND c.relkind IN {READABLE_KINDS}
LEFT JOIN pg_attribute a
  ON a.attrelid = c.oid AND a.attname = n.col AND a.attnum > 0 AND NOT a.attisdropped
LEFT JOIN pg_type t ON t.oid = a.atttypid
LEFT JOIN pg_type u ON u.oid = CASE WHEN t.typtype = 'd' THEN t.typbasetype ELSE t.oid END
LEFT JOIN pg_namespace un ON un.oid = u.typnamespace
LEFT JOIN pg_type et ON et.oid = u.typelem
LEFT JOIN pg_namespace etn ON etn.oid = et.typnamespace"
    )
}

/// Fails the export, naming every column it can't copy: a table or
/// column the source lacks, one the role can't read, or a type that is
/// neither built in nor an enum. A `DO` block, which a read-only
/// transaction allows, since psql has no way to fail on a query's rows.
fn check_sql(needed: &str) -> String {
    format!(
        "DO $straum$
DECLARE
  problems text;
BEGIN
  SELECT string_agg(why, E'\\n' ORDER BY ord) INTO problems FROM (
    SELECT n.ord, CASE
      WHEN c.oid IS NULL THEN format('%s.%s does not exist', n.nsp, n.rel)
      WHEN a.attnum IS NULL THEN format('%s.%s has no column %s', n.nsp, n.rel, n.col)
      WHEN NOT has_column_privilege(c.oid, a.attnum, 'SELECT')
        THEN format('%s.%s.%s: the role cannot SELECT it', n.nsp, n.rel, n.col)
      WHEN u.typtype <> 'e' AND coalesce(et.typtype, '') <> 'e' AND un.nspname <> 'pg_catalog'
        THEN format('%s.%s.%s has type %s, which a copy does not create',
                    n.nsp, n.rel, n.col, format_type(a.atttypid, a.atttypmod))
    END AS why
{}
  ) AS checked WHERE why IS NOT NULL;
  IF problems IS NOT NULL THEN
    RAISE EXCEPTION 'straum: cannot copy:%', E'\\n' || problems;
  END IF;
END
$straum$;",
        typed_columns(needed)
    )
}

/// The lines of `schema.sql`: each schema, each enum a column uses, then
/// each table with only the columns read, in the order the CSVs hold them.
fn schema_sql(needed: &str) -> String {
    format!(
        "WITH typed AS (
  SELECT n.nsp, n.rel, n.col, n.ord,
    CASE WHEN u.typtype = 'e' THEN format('%I.%I', un.nspname, u.typname)
         WHEN et.typtype = 'e' THEN format('%I.%I[]', etn.nspname, et.typname)
         ELSE format_type(u.oid, CASE WHEN t.typtype = 'd' THEN t.typtypmod
                                      ELSE a.atttypmod END)
    END AS sql_type,
    CASE WHEN u.typtype = 'e' THEN u.oid WHEN et.typtype = 'e' THEN et.oid END AS enum_oid
{}
), enums AS (
  SELECT DISTINCT e.oid, ens.nspname, e.typname
  FROM typed JOIN pg_type e ON e.oid = typed.enum_oid
  JOIN pg_namespace ens ON ens.oid = e.typnamespace
)
SELECT line FROM (
  SELECT 1 AS step, nsp AS sort, format('CREATE SCHEMA IF NOT EXISTS %I;', nsp) AS line
  FROM (SELECT nsp FROM typed UNION SELECT nspname FROM enums) AS schemas
  UNION ALL
  SELECT 2, format('%I.%I', nspname, typname),
    format('CREATE TYPE %I.%I AS ENUM (%s);', nspname, typname,
      (SELECT string_agg(quote_literal(l.enumlabel), ', ' ORDER BY l.enumsortorder)
       FROM pg_enum l WHERE l.enumtypid = enums.oid))
  FROM enums
  UNION ALL
  SELECT 3, lpad(min(ord)::text, 10, '0'),
    format('CREATE TABLE %I.%I (%s);', nsp, rel,
      string_agg(format('%I %s', col, sql_type), ', ' ORDER BY ord))
  FROM typed GROUP BY nsp, rel
) AS lines ORDER BY step, sort;",
        typed_columns(needed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(text: &str) -> Spec {
        straum_notation::parse(text).unwrap()
    }

    /// A view `name` counting `table`'s rows by the hour and by `key`.
    fn view(name: &str, table: &str, key: &str) -> String {
        format!(
            "CREATE VIEW {name} AS
             SELECT buckets(f.at, '1 hour') AS bucket,
                    f.{key} AS {key}, count(*) AS n
             FROM {table} AS f GROUP BY 1, 2;"
        )
    }

    #[test]
    fn a_table_two_specs_read_is_copied_once_with_both_their_columns() {
        let specs = [
            spec(&view("x", "public.events", "kind")),
            spec(
                &[
                    view("y", "public.events", "region"),
                    view("z", "b.u", "kind"),
                ]
                .concat(),
            ),
        ];
        let scripts = copy_scripts(&specs, "/tmp/copy");
        let export = &scripts.export;
        assert!(
            export.contains(
                "\\copy (SELECT \"at\", \"kind\", \"region\" FROM \"public\".\"events\") \
                 TO 'public.events.csv' WITH (FORMAT csv, HEADER)\n"
            ),
            "{export}"
        );
        assert_eq!(
            export.matches("TO 'public.events.csv'").count(),
            1,
            "{export}"
        );
        assert!(export.contains("TO 'b.u.csv'"), "{export}");
        assert!(
            scripts.load.contains(
                "\\copy \"public\".\"events\" (\"at\", \"kind\", \"region\") \
                 FROM 'public.events.csv' WITH (FORMAT csv, HEADER)\n"
            ),
            "{}",
            scripts.load
        );
        assert!(
            scripts
                .load
                .contains("ANALYZE \"public\".\"events\", \"b\".\"u\";\n"),
            "{}",
            scripts.load
        );
    }

    /// The export reads one snapshot and stops at its first error; the
    /// load takes the directory from psql when given.
    #[test]
    fn both_scripts_stop_at_an_error_and_find_their_directory() {
        let scripts = copy_scripts(&[spec(&view("x", "public.events", "kind"))], "/data/it's");
        for script in [&scripts.export, &scripts.load] {
            assert!(script.contains("\\set ON_ERROR_STOP on\n"), "{script}");
            assert!(script.contains("\\set dir '/data/it''s'\n"), "{script}");
            assert!(script.contains("\\cd :dir\n"), "{script}");
        }
        assert!(
            scripts
                .export
                .contains("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY;\n"),
            "{}",
            scripts.export
        );
        assert!(
            scripts.export.contains("SET LOCAL row_security = off;\n"),
            "{}",
            scripts.export
        );
        assert!(
            scripts.load.contains("BEGIN;\n\\i schema.sql\n"),
            "{}",
            scripts.load
        );
    }

    #[test]
    fn a_backslash_in_the_directory_stays_one() {
        assert_eq!(psql_string("C:\\data"), "'C:\\\\data'");
        assert_ne!(psql_string("C:\\data"), "'C:\\data'");
    }
}
