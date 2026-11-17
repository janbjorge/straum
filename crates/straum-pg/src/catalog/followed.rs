//! What a running stream checks of its tables with each reading of the
//! WAL: changes no message the stream reads says. A table is gone from
//! under its name, another is there, or the publication left it out.

use postgres_protocol::escape::escape_literal;

use super::{Checked, is_true, shape};
use crate::error::Error;
use crate::problem::{TableNames, TableProblem};
use crate::source::Replication;
use crate::value::Oid;
use crate::wire::Connection;

/// The spec's tables a stream follows, and how.
pub(crate) struct Followed<'a> {
    pub(crate) tables: &'a [Checked],
    pub(crate) publication: &'a str,
    pub(crate) replication: Replication,
}

/// Fails once a table the stream follows is gone from under its name,
/// another table is there, or the publication left it out: none of it
/// sends a message the stream reads. The setup check of the next start
/// says what is wrong.
pub(crate) fn still_followed(
    connection: &mut Connection,
    followed: &Followed<'_>,
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
        escape_literal(followed.publication)
    ))?;
    if rows.len() != followed.tables.len() {
        return Err(shape("followed tables"));
    }
    let mut problems = Vec::new();
    for (table, row) in followed.tables.iter().zip(rows) {
        let [oid, published] = row.try_into().map_err(|_| shape("followed tables"))?;
        let names = TableNames {
            name: table.name.clone(),
            qualified: table.qualified(),
        };
        let Some(oid) = oid else {
            problems.push(names.table_problem(TableProblem::Missing));
            continue;
        };
        let oid: Oid = oid.parse().map_err(|_| shape("followed tables"))?;
        let problem = match oid {
            oid if oid == table.oid => {
                let logical = followed.replication == Replication::Logical;
                if !logical || published.as_deref().is_some_and(is_true) {
                    continue;
                }
                TableProblem::NotPublished {
                    publication: followed.publication.to_owned(),
                }
            }
            _ => TableProblem::Replaced,
        };
        problems.push(names.table_problem(problem));
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(Error::Setup(problems))
}
