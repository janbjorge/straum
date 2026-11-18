//! Stores one snapshot of the tables a spec names.
//! The source is a read-only repeatable-read transaction, so the tables
//! agree and the source is not written. The destination gets the columns,
//! their enums and the rows. Indexes, keys and defaults stay behind.
//! Text `COPY`, not binary: a binary array carries the element type's oid,
//! and an enum's oid differs between databases.

use std::fmt;

use postgres_protocol::escape::{escape_identifier, escape_literal};
use straum_domain::Spec;

use crate::dsn::Dsn;
use crate::error::Error;
use crate::wire::{Connection, ConnectionKind};

/// Why a copy stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum CopyError {
    /// Connecting, reading or writing failed. `database` is the URL
    /// without its password.
    Database { database: String, message: String },
    /// The spec names a table the source has no relation for.
    Missing { schema: String, table: String },
    /// The role can see the table and cannot `SELECT` it.
    Unreadable { schema: String, table: String },
    /// The table has no column a copy can write.
    Empty { schema: String, table: String },
    /// A column's type is not a built-in and not an enum.
    Unsupported {
        schema: String,
        table: String,
        column: String,
        type_name: String,
    },
}

impl fmt::Display for CopyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CopyError::Database { database, message } => {
                write!(formatter, "{database}: {message}")
            }
            CopyError::Missing { schema, table } => {
                write!(formatter, "{schema}.{table} does not exist")
            }
            CopyError::Unreadable { schema, table } => {
                write!(formatter, "{schema}.{table}: the role cannot SELECT it")
            }
            CopyError::Empty { schema, table } => {
                write!(formatter, "{schema}.{table} has no columns")
            }
            CopyError::Unsupported {
                schema,
                table,
                column,
                type_name,
            } => write!(
                formatter,
                "{schema}.{table}.{column} has type {type_name}, which a copy does not create"
            ),
        }
    }
}

struct Column {
    name: String,
    sql_type: String,
}

struct Table {
    schema: String,
    name: String,
    columns: Vec<Column>,
}

struct EnumType {
    schema: String,
    name: String,
    labels: Vec<String>,
}

/// Copies one snapshot of the spec's tables from `from` into `to`.
pub fn copy(spec: &Spec, from: &Dsn, to: &Dsn) -> Result<(), CopyError> {
    let mut source = connect(from)?;
    run(
        &mut source,
        from,
        "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY",
    )?;
    let (tables, enums) = snapshot(&mut source, from, spec)?;
    let mut dest = connect(to)?;
    run(&mut dest, to, "BEGIN")?;
    create(&mut dest, to, &tables, &enums)?;
    for table in &tables {
        copy_table(&mut source, from, &mut dest, to, table)?;
    }
    run(&mut dest, to, "COMMIT")?;
    run(&mut source, from, "COMMIT")?;
    Ok(())
}

fn connect(dsn: &Dsn) -> Result<Connection, CopyError> {
    Connection::connect(dsn, ConnectionKind::Sql).map_err(|error| failed(dsn, &error))
}

fn run(connection: &mut Connection, dsn: &Dsn, sql: &str) -> Result<(), CopyError> {
    connection.execute(sql).map_err(|error| failed(dsn, &error))
}

fn query(
    connection: &mut Connection,
    dsn: &Dsn,
    sql: &str,
) -> Result<Vec<Vec<Option<String>>>, CopyError> {
    connection.rows(sql).map_err(|error| failed(dsn, &error))
}

fn failed(dsn: &Dsn, error: &Error) -> CopyError {
    CopyError::Database {
        database: dsn.to_string(),
        message: error.to_string(),
    }
}

/// The tables and enums in the open snapshot.
fn snapshot(
    connection: &mut Connection,
    dsn: &Dsn,
    spec: &Spec,
) -> Result<(Vec<Table>, Vec<EnumType>), CopyError> {
    let rows = query(connection, dsn, &catalog(&table_pairs(spec)))?;
    let mut enums = Vec::new();
    let mut tables = Vec::new();
    for table in spec.tables() {
        tables.push(one_table(
            &rows,
            dsn,
            table.schema.as_str(),
            table.name.as_str(),
            &mut enums,
        )?);
    }
    Ok((tables, enum_labels(connection, dsn, enums)?))
}

fn catalog(pairs: &str) -> String {
    format!(
        "SELECT n.nspname, c.relname, has_table_privilege(c.oid, 'SELECT'),
                a.attname,
                format_type(u.oid, CASE WHEN t.typtype = 'd' THEN t.typtypmod
                                        ELSE a.atttypmod END),
                u.typtype, un.nspname, u.typname,
                et.typtype, etn.nspname, et.typname
           FROM pg_class c
           JOIN pg_namespace n ON n.oid = c.relnamespace
           LEFT JOIN pg_attribute a
             ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
           LEFT JOIN pg_type t ON t.oid = a.atttypid
           LEFT JOIN pg_type u ON u.oid = CASE WHEN t.typtype = 'd' THEN t.typbasetype
                                               ELSE t.oid END
           LEFT JOIN pg_namespace un ON un.oid = u.typnamespace
           LEFT JOIN pg_type et ON et.oid = u.typelem
           LEFT JOIN pg_namespace etn ON etn.oid = et.typnamespace
          WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f')
            AND (n.nspname, c.relname) IN (VALUES {pairs})
          ORDER BY n.nspname, c.relname, a.attnum"
    )
}

fn one_table(
    rows: &[Vec<Option<String>>],
    dsn: &Dsn,
    schema: &str,
    name: &str,
    enums: &mut Vec<(String, String)>,
) -> Result<Table, CopyError> {
    let mut saw = false;
    let mut readable = false;
    let mut columns = Vec::new();
    for row in rows {
        if row.first().and_then(Option::as_deref) != Some(schema) {
            continue;
        }
        if row.get(1).and_then(Option::as_deref) != Some(name) {
            continue;
        }
        saw = true;
        readable = field(row, 2, dsn)? == "t";
        if !readable {
            continue;
        }
        let Some(column) = row.get(3).and_then(Option::as_deref) else {
            continue;
        };
        columns.push(Column {
            name: column.to_owned(),
            sql_type: sql_type(row, enums, dsn)?,
        });
    }
    if !saw {
        return Err(CopyError::Missing {
            schema: schema.to_owned(),
            table: name.to_owned(),
        });
    }
    if !readable {
        return Err(CopyError::Unreadable {
            schema: schema.to_owned(),
            table: name.to_owned(),
        });
    }
    if columns.is_empty() {
        return Err(CopyError::Empty {
            schema: schema.to_owned(),
            table: name.to_owned(),
        });
    }
    Ok(Table {
        schema: schema.to_owned(),
        name: name.to_owned(),
        columns,
    })
}

/// The destination type of one column. A domain is its base type, so the
/// destination does not need the domain.
fn sql_type(
    row: &[Option<String>],
    enums: &mut Vec<(String, String)>,
    dsn: &Dsn,
) -> Result<String, CopyError> {
    if row.get(8).and_then(Option::as_deref) == Some("e") {
        return enum_type(row, 9, true, enums, dsn);
    }
    if field(row, 5, dsn)? == "e" {
        return enum_type(row, 6, false, enums, dsn);
    }
    if field(row, 6, dsn)? == "pg_catalog" {
        return field(row, 4, dsn);
    }
    Err(CopyError::Unsupported {
        schema: field(row, 0, dsn)?,
        table: field(row, 1, dsn)?,
        column: field(row, 3, dsn)?,
        type_name: field(row, 4, dsn)?,
    })
}

fn enum_type(
    row: &[Option<String>],
    at: usize,
    array: bool,
    enums: &mut Vec<(String, String)>,
    dsn: &Dsn,
) -> Result<String, CopyError> {
    let schema = field(row, at, dsn)?;
    let name = field(row, at + 1, dsn)?;
    if !enums
        .iter()
        .any(|(have_schema, have_name)| have_schema == &schema && have_name == &name)
    {
        enums.push((schema.clone(), name.clone()));
    }
    let mut sql_type = qualified(&schema, &name);
    if array {
        sql_type.push_str("[]");
    }
    Ok(sql_type)
}

fn enum_labels(
    connection: &mut Connection,
    dsn: &Dsn,
    enums: Vec<(String, String)>,
) -> Result<Vec<EnumType>, CopyError> {
    if enums.is_empty() {
        return Ok(Vec::new());
    }
    let mut pairs = Vec::new();
    for (schema, name) in &enums {
        pairs.push(format!(
            "({}, {})",
            escape_literal(schema),
            escape_literal(name)
        ));
    }
    let rows = query(
        connection,
        dsn,
        &format!(
            "SELECT n.nspname, t.typname, e.enumlabel
               FROM pg_enum e
               JOIN pg_type t ON t.oid = e.enumtypid
               JOIN pg_namespace n ON n.oid = t.typnamespace
              WHERE (n.nspname, t.typname) IN (VALUES {})
              ORDER BY n.nspname, t.typname, e.enumsortorder",
            pairs.join(", ")
        ),
    )?;
    let mut copied = Vec::new();
    for (schema, name) in enums {
        let mut labels = Vec::new();
        for row in &rows {
            if row.first().and_then(Option::as_deref) != Some(schema.as_str()) {
                continue;
            }
            if row.get(1).and_then(Option::as_deref) != Some(name.as_str()) {
                continue;
            }
            labels.push(field(row, 2, dsn)?);
        }
        copied.push(EnumType {
            schema,
            name,
            labels,
        });
    }
    Ok(copied)
}

fn create(
    connection: &mut Connection,
    dsn: &Dsn,
    tables: &[Table],
    enums: &[EnumType],
) -> Result<(), CopyError> {
    for schema in schemas(tables, enums) {
        run(
            connection,
            dsn,
            &format!("CREATE SCHEMA IF NOT EXISTS {}", escape_identifier(&schema)),
        )?;
    }
    for enum_type in enums {
        let mut labels = Vec::new();
        for label in &enum_type.labels {
            labels.push(escape_literal(label));
        }
        run(
            connection,
            dsn,
            &format!(
                "CREATE TYPE {} AS ENUM ({})",
                qualified(&enum_type.schema, &enum_type.name),
                labels.join(", ")
            ),
        )?;
    }
    for table in tables {
        run(connection, dsn, &create_table(table))?;
    }
    Ok(())
}

fn schemas(tables: &[Table], enums: &[EnumType]) -> Vec<String> {
    let mut schemas = Vec::new();
    for table in tables {
        remember(&mut schemas, &table.schema);
    }
    for enum_type in enums {
        remember(&mut schemas, &enum_type.schema);
    }
    schemas
}

fn remember(schemas: &mut Vec<String>, schema: &str) {
    if !schemas.iter().any(|have| have == schema) {
        schemas.push(schema.to_owned());
    }
}

fn create_table(table: &Table) -> String {
    let mut columns = Vec::new();
    for column in &table.columns {
        columns.push(format!(
            "{} {}",
            escape_identifier(&column.name),
            column.sql_type
        ));
    }
    format!(
        "CREATE TABLE {} ({})",
        qualified(&table.schema, &table.name),
        columns.join(", ")
    )
}

fn copy_table(
    source: &mut Connection,
    from: &Dsn,
    dest: &mut Connection,
    to: &Dsn,
    table: &Table,
) -> Result<(), CopyError> {
    let list = column_list(table);
    let qualified = qualified(&table.schema, &table.name);
    dest.start_copy_in(&format!("COPY {qualified} ({list}) FROM STDIN"))
        .map_err(|error| failed(to, &error))?;
    let copied = source.copy_out(
        &format!("COPY (SELECT {list} FROM {qualified}) TO STDOUT"),
        |chunk| dest.send_copy_data(chunk),
    );
    if let Err(error) = copied {
        return Err(failed(from, &error));
    }
    dest.finish_copy_in().map_err(|error| failed(to, &error))
}

fn column_list(table: &Table) -> String {
    let mut names = Vec::new();
    for column in &table.columns {
        names.push(escape_identifier(&column.name));
    }
    names.join(", ")
}

fn table_pairs(spec: &Spec) -> String {
    let mut pairs = Vec::new();
    for table in spec.tables() {
        pairs.push(format!(
            "({}, {})",
            escape_literal(table.schema.as_str()),
            escape_literal(table.name.as_str())
        ));
    }
    pairs.join(", ")
}

fn qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", escape_identifier(schema), escape_identifier(name))
}

fn field(row: &[Option<String>], index: usize, dsn: &Dsn) -> Result<String, CopyError> {
    match row.get(index).and_then(Option::as_deref) {
        Some(text) => Ok(text.to_owned()),
        None => Err(CopyError::Database {
            database: dsn.to_string(),
            message: "a catalog row was short".into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_name_the_table() {
        let cases = [
            (
                CopyError::Missing {
                    schema: "public".into(),
                    table: "events".into(),
                },
                "public.events does not exist",
            ),
            (
                CopyError::Unreadable {
                    schema: "public".into(),
                    table: "events".into(),
                },
                "public.events: the role cannot SELECT it",
            ),
            (
                CopyError::Empty {
                    schema: "public".into(),
                    table: "events".into(),
                },
                "public.events has no columns",
            ),
            (
                CopyError::Unsupported {
                    schema: "public".into(),
                    table: "events".into(),
                    column: "place".into(),
                    type_name: "public.address".into(),
                },
                "public.events.place has type public.address, which a copy does not create",
            ),
            (
                CopyError::Database {
                    database: "postgres://u@db:5432/app?sslmode=disable".into(),
                    message: "connection to PostgreSQL: gone".into(),
                },
                "postgres://u@db:5432/app?sslmode=disable: connection to PostgreSQL: gone",
            ),
        ];
        for (error, message) in cases {
            assert_eq!(error.to_string(), message);
        }
    }
}
