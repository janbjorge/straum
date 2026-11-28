//! What a running stream checks of its tables with each reading of the
//! WAL: changes no message the stream reads says. A table is gone from
//! under its name, another is there, a table inherits from it, the
//! publication left it out, filters its rows, leaves out an action or
//! was altered since the load, a label of an enum it reads was renamed,
//! or a column it reads was altered. A `synchronous_standby_names`
//! reloaded since the start names Straum's connection.

use std::collections::HashMap;

use postgres_protocol::escape::escape_literal;

use super::standby::names_standby;
use super::{Checked, EnumLabel, is_true, shape};
use crate::error::Error;
use crate::problem::{Problem, ServerProblem, TableProblem};
use crate::source::Replication;
use crate::value::Oid;
use crate::wire::{APPLICATION_NAME, Connection, Row};

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
    /// The version of the publication, as loaded; none on the physical
    /// stream, which reads no publication.
    pub(crate) publication: Option<String>,
}

/// Fails once a table the stream follows is gone from under its name,
/// another table is there, a table inherits from it, the publication left
/// it out, filters its rows, leaves out an action or was altered since the
/// load, or a label of an enum it reads was renamed or dropped: none of it
/// sends a message the stream reads, and a renamed label leaves the rows
/// loaded with the old one. It fails too once `synchronous_standby_names`
/// names Straum's connection. The setup check of the next start says what
/// is wrong.
pub(crate) fn still_followed(
    connection: &mut Connection,
    followed: &Followed,
    publication: &str,
    replication: Replication,
) -> Result<(), Error> {
    if followed.tables.is_empty() {
        return Ok(());
    }
    let rows = publication_rows(connection, followed, publication)?;
    if rows.len() != followed.tables.len() {
        return Err(shape("followed tables"));
    }
    let logical = replication == Replication::Logical;
    let mut problems = Vec::new();
    let mut some_actions = false;
    for (table, row) in followed.tables.iter().zip(rows) {
        let [oid, published, row_filter, all_actions, children] =
            row.try_into().map_err(|_| shape("followed tables"))?;
        if let Some(all_actions) = &all_actions
            && !is_true(all_actions)
        {
            some_actions = true;
        }
        let names = table.names();
        let Some(oid) = oid else {
            problems.push(names.table_problem(TableProblem::Missing));
            continue;
        };
        let oid: Oid = oid.parse().map_err(|_| shape("followed tables"))?;
        let publication = publication.to_owned();
        let problem = match oid {
            oid if oid != table.oid => TableProblem::Replaced,
            // A query of the table counts a child's rows, but their changes
            // come under the child's name, on both streams.
            _ if children.as_deref().is_some_and(is_true) => TableProblem::Children,
            _ if !logical => continue,
            _ if !published.as_deref().is_some_and(is_true) => {
                TableProblem::NotPublished { publication }
            }
            _ if row_filter.as_deref().is_some_and(is_true) => {
                TableProblem::RowFilter { publication }
            }
            _ => continue,
        };
        problems.push(names.table_problem(problem));
    }
    if logical && some_actions {
        problems.push(Problem::Server(ServerProblem::PublicationActions {
            publication: publication.to_owned(),
        }));
    }
    // A change made while the publication left it out did not come.
    if problems.is_empty()
        && let Some(loaded) = &followed.publication
        && *loaded != publication_version(connection, &followed.tables, publication)?
    {
        problems.push(Problem::Server(ServerProblem::PublicationChanged {
            publication: publication.to_owned(),
        }));
    }
    relabelled(connection, followed, &mut problems)?;
    altered(connection, followed, &mut problems)?;
    standby_named(connection, &mut problems)?;
    if problems.is_empty() {
        return Ok(());
    }
    Err(Error::Setup(problems))
}

/// Per table followed, in order: its oid, whether the publication sends
/// it, whether it filters its rows, whether the publication sends all
/// four actions, and whether a table inherits from it.
fn publication_rows(
    connection: &mut Connection,
    followed: &Followed,
    publication: &str,
) -> Result<Vec<Row>, Error> {
    let mut values = Vec::new();
    for (index, table) in followed.tables.iter().enumerate() {
        values.push(format!(
            "({index}, {}, {})",
            escape_literal(table.schema.as_str()),
            escape_literal(table.table_name.as_str())
        ));
    }
    let rows = connection.rows(&format!(
        "SELECT c.oid, p.tablename IS NOT NULL, p.rowfilter IS NOT NULL,
                pub.pubinsert AND pub.pubupdate AND pub.pubdelete AND pub.pubtruncate,
                EXISTS (SELECT 1 FROM pg_inherits i WHERE i.inhparent = c.oid)
           FROM (VALUES {}) AS t(at, schema, name)
           LEFT JOIN pg_namespace n ON n.nspname = t.schema
           LEFT JOIN pg_class c ON c.relnamespace = n.oid AND c.relname = t.name
           LEFT JOIN pg_publication_tables p
                  ON p.pubname = {publication} AND p.schemaname = t.schema AND p.tablename = t.name
           LEFT JOIN pg_publication pub ON pub.pubname = {publication}
          ORDER BY t.at",
        values.join(", "),
        publication = escape_literal(publication)
    ))?;
    Ok(rows)
}

/// The version of `publication` for `tables`: the `oid` and `xmin` of its
/// catalog row, of the row of each table in `pg_publication_rel`, and of
/// the row of each table's schema in `pg_publication_namespace`. `ALTER
/// PUBLICATION` writes these rows anew when it drops, adds or filters a
/// table, or sets the actions it publishes. pgoutput reads each change
/// with the publication of its time, so a change made between two such
/// statements does not come, though the publication is then as it was.
/// The read takes no lock on a table.
pub(crate) fn publication_version(
    connection: &mut Connection,
    tables: &[Checked],
    publication: &str,
) -> Result<String, Error> {
    let mut oids = Vec::new();
    for table in tables {
        oids.push(table.oid.to_string());
    }
    if oids.is_empty() {
        return Ok(String::new());
    }
    let rows = connection.rows(&format!(
        "SELECT string_agg(v, ',' ORDER BY v) FROM (
             SELECT 'publication ' || p.oid || ':' || p.xmin
               FROM pg_publication p WHERE p.pubname = {publication}
             UNION ALL
             SELECT 'table ' || r.oid || ':' || r.xmin
               FROM pg_publication_rel r JOIN pg_publication p ON p.oid = r.prpubid
              WHERE p.pubname = {publication} AND r.prrelid IN ({oids})
             UNION ALL
             SELECT DISTINCT 'schema ' || s.oid || ':' || s.xmin
               FROM pg_publication_namespace s JOIN pg_publication p ON p.oid = s.pnpubid
               JOIN pg_class c ON c.relnamespace = s.pnnspid
              WHERE p.pubname = {publication} AND c.oid IN ({oids})
         ) AS t(v)",
        publication = escape_literal(publication),
        oids = oids.join(", ")
    ))?;
    let Some(Ok([version])) = rows.into_iter().next().map(<[Option<String>; 1]>::try_from) else {
        return Err(shape("publication version"));
    };
    Ok(version.unwrap_or_default())
}

/// A problem when `synchronous_standby_names` names Straum's connection:
/// a reload changes it while Straum runs.
fn standby_named(connection: &mut Connection, problems: &mut Vec<Problem>) -> Result<(), Error> {
    let rows = connection.rows("SHOW synchronous_standby_names")?;
    let Some(Ok([setting])) = rows.into_iter().next().map(<[Option<String>; 1]>::try_from) else {
        return Err(shape("synchronous_standby_names"));
    };
    let setting = setting.unwrap_or_default();
    if names_standby(&setting, APPLICATION_NAME) {
        problems.push(Problem::Server(ServerProblem::SynchronousStandby {
            setting,
        }));
    }
    Ok(())
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
/// STATISTICS`, `SET STORAGE`, a first `SET DEFAULT`, `SET NOT NULL` or
/// `DROP NOT NULL` write the catalog row alone and leave the file. A
/// rename over a column, or a drop and an add again, puts another column
/// under its name at another `attnum`. So a column is altered when its
/// type or `attnum` changed, or its catalog row and the file both did;
/// the last also takes such a write and a `VACUUM FULL` both since the
/// load, and stops Straum for nothing, which is safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ColumnVersions {
    pub(crate) oid: Oid,
    /// `name:attnum:atttypid:atttypmod` of each column read, in column order.
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
                string_agg(a.attname || ':' || a.attnum || ':' || a.atttypid || ':' || a.atttypmod,
                           ',' ORDER BY a.attnum),
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
        let loaded = versions_of(&followed.versions, table.oid);
        let current = versions_of(&now, table.oid);
        if let (Some(loaded), Some(current)) = (loaded, current)
            && loaded.is_altered_in(current)
        {
            problems.push(table.names().table_problem(TableProblem::Altered));
        }
    }
    Ok(())
}

/// The versions of `all` of the table `oid`, if it has any.
fn versions_of(all: &[ColumnVersions], oid: Oid) -> Option<&ColumnVersions> {
    let mut found = None;
    for versions in all {
        if versions.oid == oid {
            found = Some(versions);
        }
    }
    found
}
