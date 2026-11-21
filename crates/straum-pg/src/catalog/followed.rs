//! What a running stream checks of its tables with each reading of the
//! WAL: changes no message the stream reads says. A table is gone from
//! under its name, another is there, the publication left it out, a
//! label of an enum it reads was renamed, or a column it reads was
//! altered.

use std::collections::HashMap;

use postgres_protocol::escape::escape_literal;

use super::{Checked, EnumLabel, is_true, shape};
use crate::error::Error;
use crate::problem::{Problem, TableProblem};
use crate::source::Replication;
use crate::value::Oid;
use crate::wire::Connection;

/// The spec's tables a stream follows, as loaded.
#[derive(Debug, Clone, Default)]
pub(crate) struct Followed {
    /// The facts, then the dims.
    pub(crate) tables: Vec<Checked>,
    /// The labels of their enums, as loaded.
    pub(crate) labels: Vec<EnumLabel>,
    /// The versions of the columns read, as loaded; none on the physical
    /// stream, which reads a rewrite in the WAL.
    pub(crate) versions: Vec<ColumnVersions>,
}

/// Fails once a table the stream follows is gone from under its name,
/// another table is there, the publication left it out, or a label of
/// an enum it reads was renamed or dropped: none of it sends a message
/// the stream reads, and a renamed label leaves the rows loaded with the
/// old one. The setup check of the next start says what is wrong.
pub(crate) fn still_followed(
    connection: &mut Connection,
    followed: &Followed,
    publication: &str,
    replication: Replication,
) -> Result<(), Error> {
    if followed.tables.is_empty() {
        return Ok(());
    }
    let mut values = Vec::new();
    for (index, table) in followed.tables.iter().enumerate() {
        values.push(format!(
            "({index}, {}, {})",
            escape_literal(table.schema.as_str()),
            escape_literal(table.table_name.as_str())
        ));
    }
    let rows = connection.rows(&format!(
        "SELECT c.oid, p.tablename IS NOT NULL
           FROM (VALUES {}) AS t(at, schema, name)
           LEFT JOIN pg_namespace n ON n.nspname = t.schema
           LEFT JOIN pg_class c ON c.relnamespace = n.oid AND c.relname = t.name
           LEFT JOIN pg_publication_tables p
                  ON p.pubname = {} AND p.schemaname = t.schema AND p.tablename = t.name
          ORDER BY t.at",
        values.join(", "),
        escape_literal(publication)
    ))?;
    if rows.len() != followed.tables.len() {
        return Err(shape("followed tables"));
    }
    let mut problems = Vec::new();
    for (table, row) in followed.tables.iter().zip(rows) {
        let [oid, published] = row.try_into().map_err(|_| shape("followed tables"))?;
        let names = table.names();
        let Some(oid) = oid else {
            problems.push(names.table_problem(TableProblem::Missing));
            continue;
        };
        let oid: Oid = oid.parse().map_err(|_| shape("followed tables"))?;
        let problem = match oid {
            oid if oid == table.oid => {
                let logical = replication == Replication::Logical;
                if !logical || published.as_deref().is_some_and(is_true) {
                    continue;
                }
                TableProblem::NotPublished {
                    publication: publication.to_owned(),
                }
            }
            _ => TableProblem::Replaced,
        };
        problems.push(names.table_problem(problem));
    }
    relabelled(connection, followed, &mut problems)?;
    altered(connection, followed, &mut problems)?;
    if problems.is_empty() {
        return Ok(());
    }
    Err(Error::Setup(problems))
}

/// A problem for each table whose enum has a label loaded that is now
/// another, or gone. A label added is no problem: no row loaded has it.
fn relabelled(
    connection: &mut Connection,
    followed: &Followed,
    problems: &mut Vec<Problem>,
) -> Result<(), Error> {
    if followed.labels.is_empty() {
        return Ok(());
    }
    let mut oids = Vec::new();
    for label in &followed.labels {
        oids.push(label.oid.to_string());
    }
    let rows = connection.rows(&format!(
        "SELECT oid, enumlabel FROM pg_enum WHERE oid IN ({})",
        oids.join(", ")
    ))?;
    let mut now = HashMap::new();
    for row in rows {
        let [oid, label] = <[Option<String>; 2]>::try_from(row)
            .map_err(|_| shape("labels now"))?
            .map(Option::unwrap_or_default);
        let oid: Oid = oid.parse().map_err(|_| shape("labels now"))?;
        now.insert(oid, label);
    }
    let mut changed = Vec::new();
    for label in &followed.labels {
        if now.get(&label.oid) != Some(&label.label) && !changed.contains(&label.type_oid) {
            changed.push(label.type_oid);
        }
    }
    for table in &followed.tables {
        for type_oid in &changed {
            if table.enums.contains(type_oid) {
                problems.push(table.names().table_problem(TableProblem::Relabelled));
                break;
            }
        }
    }
    Ok(())
}

/// The version of a table's columns Straum reads: each one's type, the
/// transaction that last wrote its catalog row, and the table's file.
/// `ALTER COLUMN .. TYPE` writes the row anew and rewrites the table's
/// rows, to the same type too, which the logical stream sends nothing
/// of. `VACUUM FULL` and `CLUSTER` rewrite the rows alone, with the same
/// values, and leave the catalog row. A column-level `GRANT`, `SET
/// STATISTICS`, `SET STORAGE` or a first `SET DEFAULT` write the catalog
/// row alone and leave the file. So a column is altered when its type
/// changed, or its catalog row and the file both did; the last also
/// takes such a write and a `VACUUM FULL` both since the load, and stops
/// Straum for nothing, which is safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnVersions {
    pub(crate) oid: Oid,
    /// `name:atttypid:atttypmod:attnotnull` of each column read, in
    /// column order.
    pub(crate) types: String,
    /// The `xmin` of each column's catalog row, in column order.
    pub(crate) rows: String,
    /// `pg_class.relfilenode`: the table's file.
    pub(crate) file: String,
}

impl ColumnVersions {
    /// Whether `now` is a version Straum did not load: see the type.
    fn is_altered_in(&self, now: &ColumnVersions) -> bool {
        self.types != now.types || (self.rows != now.rows && self.file != now.file)
    }
}

/// The versions of `tables`' columns read, as `connection` sees them.
pub(crate) fn column_versions(
    connection: &mut Connection,
    tables: &[Checked],
) -> Result<Vec<ColumnVersions>, Error> {
    let mut wanted = Vec::new();
    for table in tables {
        for column in &table.columns {
            wanted.push(format!(
                "({}::oid, {}::name)",
                table.oid,
                escape_literal(column.as_str())
            ));
        }
    }
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let rows = connection.rows(&format!(
        "SELECT a.attrelid,
                string_agg(a.attname || ':' || a.atttypid || ':' || a.atttypmod
                           || ':' || a.attnotnull, ',' ORDER BY a.attnum),
                string_agg(a.xmin::text, ',' ORDER BY a.attnum),
                c.relfilenode
           FROM pg_attribute a
           JOIN pg_class c ON c.oid = a.attrelid
           JOIN (VALUES {}) AS t(rel, col) ON a.attrelid = t.rel AND a.attname = t.col
          GROUP BY a.attrelid, c.relfilenode",
        wanted.join(", ")
    ))?;
    let mut found = Vec::new();
    for row in rows {
        let [oid, types, rows, file] = <[Option<String>; 4]>::try_from(row)
            .map_err(|_| shape("column versions"))?
            .map(Option::unwrap_or_default);
        found.push(ColumnVersions {
            oid: oid.parse().map_err(|_| shape("column versions"))?,
            types,
            rows,
            file,
        });
    }
    Ok(found)
}

/// A problem for each table whose columns read have another version now.
/// A table gone or replaced has its own problem already.
fn altered(
    connection: &mut Connection,
    followed: &Followed,
    problems: &mut Vec<Problem>,
) -> Result<(), Error> {
    if followed.versions.is_empty() {
        return Ok(());
    }
    let now = column_versions(connection, &followed.tables)?;
    for table in &followed.tables {
        let mut loaded = None;
        for versions in &followed.versions {
            if versions.oid == table.oid {
                loaded = Some(versions);
            }
        }
        let mut current = None;
        for versions in &now {
            if versions.oid == table.oid {
                current = Some(versions);
            }
        }
        if let (Some(loaded), Some(current)) = (loaded, current)
            && loaded.is_altered_in(current)
        {
            problems.push(table.names().table_problem(TableProblem::Altered));
        }
    }
    Ok(())
}
