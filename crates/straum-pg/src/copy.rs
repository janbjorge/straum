//! Copies the tables a spec names from one database into another.
//! `straum copy` does this for a try: the source only answers a snapshot
//! read, and the destination gets the columns, their enums and the rows.
//! Indexes, keys and defaults stay behind. Text `COPY`, not binary: a
//! binary array carries the element type's oid, and an enum's oid differs
//! between databases.

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

struct Relation {
    schema: String,
    name: String,
    select: bool,
}

struct Column {
    name: String,
    sql_type: String,
}

struct TableCopy {
    schema: String,
    name: String,
    columns: Vec<Column>,
}

struct EnumCopy {
    schema: String,
    name: String,
    labels: Vec<String>,
}

struct Read {
    tables: Vec<TableCopy>,
    enums: Vec<EnumCopy>,
}

/// Copies the spec's tables from `from` into `to`.
pub fn copy(spec: &Spec, from: &Dsn, to: &Dsn) -> Result<(), CopyError> {
    let mut source = connect(from)?;
    // Read only, so this cannot write to their database. One snapshot, so
    // the tables agree with each other.
    run(&mut source, from, "SET default_transaction_read_only = on")?;
    run(
        &mut source,
        from,
        "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY",
    )?;
    let read = read(&mut source, from, spec)?;
    let mut dest = connect(to)?;
    run(&mut dest, to, "BEGIN")?;
    create(&mut dest, to, &read)?;
    for table in &read.tables {
        copy_rows(&mut source, from, &mut dest, to, table)?;
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

fn read(connection: &mut Connection, dsn: &Dsn, spec: &Spec) -> Result<Read, CopyError> {
    let pairs = table_pairs(spec);
    let relations = relations(connection, dsn, &pairs)?;
    let rows = columns(connection, dsn, &pairs)?;
    let mut enums = Vec::new();
    let mut tables = Vec::new();
    for table in spec.tables() {
        let schema = table.schema.as_str();
        let name = table.name.as_str();
        let Some(relation) = find_relation(&relations, schema, name) else {
            return Err(CopyError::Missing {
                schema: schema.to_owned(),
                table: name.to_owned(),
            });
        };
        if !relation.select {
            return Err(CopyError::Unreadable {
                schema: schema.to_owned(),
                table: name.to_owned(),
            });
        }
        let columns = take_columns(&rows, dsn, schema, name, &mut enums)?;
        if columns.is_empty() {
            return Err(CopyError::Empty {
                schema: schema.to_owned(),
                table: name.to_owned(),
            });
        }
        tables.push(TableCopy {
            schema: schema.to_owned(),
            name: name.to_owned(),
            columns,
        });
    }
    let enums = enum_labels(connection, dsn, enums)?;
    Ok(Read { tables, enums })
}

fn relations(
    connection: &mut Connection,
    dsn: &Dsn,
    pairs: &str,
) -> Result<Vec<Relation>, CopyError> {
    let rows = query(
        connection,
        dsn,
        &format!(
            "SELECT n.nspname, c.relname, has_table_privilege(c.oid, 'SELECT')
               FROM pg_class c
               JOIN pg_namespace n ON n.oid = c.relnamespace
              WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f')
                AND (n.nspname, c.relname) IN (VALUES {pairs})"
        ),
    )?;
    let mut relations = Vec::new();
    for row in rows {
        let schema = field(&row, 0, dsn)?;
        let name = field(&row, 1, dsn)?;
        let select = field(&row, 2, dsn)?;
        relations.push(Relation {
            schema,
            name,
            select: select == "t",
        });
    }
    Ok(relations)
}

fn columns(
    connection: &mut Connection,
    dsn: &Dsn,
    pairs: &str,
) -> Result<Vec<Vec<Option<String>>>, CopyError> {
    query(
        connection,
        dsn,
        &format!(
            "SELECT n.nspname, c.relname, a.attname,
                    format_type(u.oid, CASE WHEN t.typtype = 'd' THEN t.typtypmod
                                            ELSE a.atttypmod END),
                    u.typtype, un.nspname, u.typname,
                    et.typtype, etn.nspname, et.typname
               FROM pg_attribute a
               JOIN pg_class c ON c.oid = a.attrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
               JOIN pg_type t ON t.oid = a.atttypid
               JOIN pg_type u ON u.oid = CASE WHEN t.typtype = 'd' THEN t.typbasetype
                                              ELSE t.oid END
               JOIN pg_namespace un ON un.oid = u.typnamespace
               LEFT JOIN pg_type et ON et.oid = u.typelem
               LEFT JOIN pg_namespace etn ON etn.oid = et.typnamespace
              WHERE a.attnum > 0 AND NOT a.attisdropped
                AND (n.nspname, c.relname) IN (VALUES {pairs})
              ORDER BY n.nspname, c.relname, a.attnum"
        ),
    )
}

/// The columns of `schema.table`, and any enum they need, from `rows`.
fn take_columns(
    rows: &[Vec<Option<String>>],
    dsn: &Dsn,
    schema: &str,
    table: &str,
    enums: &mut Vec<(String, String)>,
) -> Result<Vec<Column>, CopyError> {
    let mut columns = Vec::new();
    for row in rows {
        if row.first().and_then(Option::as_deref) != Some(schema) {
            continue;
        }
        if row.get(1).and_then(Option::as_deref) != Some(table) {
            continue;
        }
        let name = field(row, 2, dsn)?;
        let formatted = field(row, 3, dsn)?;
        let sql_type = match kind(row, dsn)? {
            Kind::Builtin => formatted,
            Kind::Enum { array } => enum_type(row, array, enums, dsn)?,
        };
        columns.push(Column { name, sql_type });
    }
    Ok(columns)
}

enum Kind {
    Builtin,
    Enum { array: bool },
}

fn kind(row: &[Option<String>], dsn: &Dsn) -> Result<Kind, CopyError> {
    if row.get(7).and_then(Option::as_deref) == Some("e") {
        return Ok(Kind::Enum { array: true });
    }
    if field(row, 4, dsn)? == "e" {
        return Ok(Kind::Enum { array: false });
    }
    if field(row, 5, dsn)? == "pg_catalog" {
        return Ok(Kind::Builtin);
    }
    Err(CopyError::Unsupported {
        schema: field(row, 0, dsn)?,
        table: field(row, 1, dsn)?,
        column: field(row, 2, dsn)?,
        type_name: field(row, 3, dsn)?,
    })
}

fn enum_type(
    row: &[Option<String>],
    array: bool,
    enums: &mut Vec<(String, String)>,
    dsn: &Dsn,
) -> Result<String, CopyError> {
    let (schema, name) = if array {
        (field(row, 8, dsn)?, field(row, 9, dsn)?)
    } else {
        (field(row, 5, dsn)?, field(row, 6, dsn)?)
    };
    remember(enums, &schema, &name);
    let mut sql_type = qualified(&schema, &name);
    if array {
        sql_type.push_str("[]");
    }
    Ok(sql_type)
}

fn remember(enums: &mut Vec<(String, String)>, schema: &str, name: &str) {
    for (have_schema, have_name) in enums.iter() {
        if have_schema == schema && have_name == name {
            return;
        }
    }
    enums.push((schema.to_owned(), name.to_owned()));
}

fn enum_labels(
    connection: &mut Connection,
    dsn: &Dsn,
    enums: Vec<(String, String)>,
) -> Result<Vec<EnumCopy>, CopyError> {
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
        copied.push(EnumCopy {
            schema,
            name,
            labels,
        });
    }
    Ok(copied)
}

fn create(connection: &mut Connection, dsn: &Dsn, read: &Read) -> Result<(), CopyError> {
    for schema in schemas(read) {
        run(
            connection,
            dsn,
            &format!("CREATE SCHEMA IF NOT EXISTS {}", escape_identifier(&schema)),
        )?;
    }
    for enum_type in &read.enums {
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
    for table in &read.tables {
        run(connection, dsn, &create_table(table))?;
    }
    Ok(())
}

fn schemas(read: &Read) -> Vec<String> {
    let mut schemas = Vec::new();
    for table in &read.tables {
        remember_schema(&mut schemas, &table.schema);
    }
    for enum_type in &read.enums {
        remember_schema(&mut schemas, &enum_type.schema);
    }
    schemas
}

fn remember_schema(schemas: &mut Vec<String>, schema: &str) {
    if !schemas.iter().any(|have| have == schema) {
        schemas.push(schema.to_owned());
    }
}

fn create_table(table: &TableCopy) -> String {
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

fn copy_rows(
    source: &mut Connection,
    from: &Dsn,
    dest: &mut Connection,
    to: &Dsn,
    table: &TableCopy,
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

fn column_list(table: &TableCopy) -> String {
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

fn find_relation<'a>(relations: &'a [Relation], schema: &str, name: &str) -> Option<&'a Relation> {
    for relation in relations {
        if relation.schema == schema && relation.name == name {
            return Some(relation);
        }
    }
    None
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
