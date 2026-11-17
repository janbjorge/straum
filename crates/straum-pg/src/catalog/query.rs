//! Reading PostgreSQL's catalog (its system tables: `pg_class`,
//! `pg_attribute`, `pg_type`, `pg_enum`, `pg_publication_tables`) into a
//! `Catalog` for the setup check: the server's settings, then the spec's
//! tables, their columns and their enums' labels, a query each.

use postgres_protocol::escape::escape_literal;
use straum_domain::Spec;

use super::{Catalog, Class, Column, EnumLabel, OLDEST_VERSION_NUM, ReadScope, is_true, shape};
use crate::error::Error;
use crate::wire::Connection;

/// Reads what the setup check needs from the catalog: settings, the spec's
/// tables, their columns and their enums' labels.
pub(crate) fn query(
    connection: &mut Connection,
    spec: &Spec,
    publication: &str,
) -> Result<Catalog, Error> {
    let publication = escape_literal(publication);
    let tables = table_list(spec);
    let mut catalog = settings(connection, &publication)?;
    // Older servers lack columns the queries below read; the check says
    // the server is too old, and no more.
    if catalog.server_version < OLDEST_VERSION_NUM {
        return Ok(catalog);
    }
    catalog.classes = classes(connection, &publication, &tables)?;
    read_columns(connection, &tables, &mut catalog.classes)?;
    catalog.labels = labels(connection, &tables)?;
    Ok(catalog)
}

/// The spec's tables as an SQL list of `(schema, table)` pairs.
fn table_list(spec: &Spec) -> String {
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

/// The server's settings, in a catalog with nothing else yet.
fn settings(connection: &mut Connection, publication: &str) -> Result<Catalog, Error> {
    let settings = connection.rows(&format!(
        "SELECT current_setting('server_version_num'), current_user, current_setting('wal_level'),
                current_setting('full_page_writes'), current_setting('wal_compression'),
                current_setting('server_encoding'), pg_is_in_recovery(),
                (SELECT pubinsert AND pubupdate AND pubdelete AND pubtruncate
                   FROM pg_publication WHERE pubname = {publication})"
    ))?;
    let Some(row) = settings.into_iter().next() else {
        return Err(shape("settings"));
    };
    let [
        version,
        role,
        wal_level,
        full_page_writes,
        wal_compression,
        server_encoding,
        in_recovery,
        published,
    ] = row.try_into().map_err(|_| shape("settings"))?;
    let server_version =
        (version.as_deref().unwrap_or_default().parse()).map_err(|_| shape("settings"))?;
    Ok(Catalog {
        server_version,
        role: role.unwrap_or_default(),
        wal_level: wal_level.unwrap_or_default(),
        full_page_writes: full_page_writes.unwrap_or_default(),
        wal_compression: wal_compression.unwrap_or_default(),
        server_encoding: server_encoding.unwrap_or_default(),
        in_recovery: in_recovery.as_deref().is_some_and(is_true),
        publication: published.as_deref().map(is_true),
        ..Catalog::default()
    })
}

/// The spec's tables as `pg_class` has them, with no columns yet.
fn classes(
    connection: &mut Connection,
    publication: &str,
    tables: &str,
) -> Result<Vec<Class>, Error> {
    let rows = connection.rows(&format!(
        "SELECT c.oid, n.nspname, c.relname, c.relkind, c.relpersistence, c.relreplident,
                has_table_privilege(c.oid, 'SELECT'),
                array_to_string(p.attnames, ','), p.rowfilter IS NOT NULL,
                EXISTS (SELECT 1 FROM pg_inherits i WHERE i.inhparent = c.oid),
                row_security_active(c.oid)
           FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
           LEFT JOIN pg_publication_tables p
                  ON p.pubname = {publication}
                 AND p.schemaname = n.nspname AND p.tablename = c.relname
          WHERE (n.nspname, c.relname) IN (VALUES {tables})"
    ))?;
    let mut classes = Vec::new();
    for row in rows {
        let [
            oid,
            schema,
            name,
            relkind,
            persistence,
            identity,
            select,
            published,
            row_filter,
            children,
            row_security,
        ] = row.try_into().map_err(|_| shape("relations"))?;
        classes.push(Class {
            oid: (oid.as_deref().unwrap_or_default().parse()).map_err(|_| shape("relations"))?,
            schema: schema.unwrap_or_default(),
            name: name.unwrap_or_default(),
            relkind: relkind.unwrap_or_default(),
            persistence: persistence.unwrap_or_default(),
            scope: ReadScope {
                children: children.as_deref() == Some("t"),
                row_security: row_security.as_deref() == Some("t"),
            },
            identity: identity.unwrap_or_default(),
            select: select.as_deref() == Some("t"),
            published: published.as_deref().map(comma_list),
            row_filter: row_filter.as_deref() == Some("t"),
            columns: Vec::new(),
        });
    }
    Ok(classes)
}

fn comma_list(text: &str) -> Vec<String> {
    text.split(',').map(str::to_owned).collect()
}

/// Adds each class's columns, dropped ones left out.
fn read_columns(
    connection: &mut Connection,
    tables: &str,
    classes: &mut [Class],
) -> Result<(), Error> {
    let rows = connection.rows(&format!(
        "SELECT n.nspname, c.relname, a.attname, a.atttypid, a.atttypmod,
                format_type(a.atttypid, a.atttypmod), t.typtype, a.attgenerated
           FROM pg_attribute a
           JOIN pg_class c ON c.oid = a.attrelid
           JOIN pg_namespace n ON n.oid = c.relnamespace
           JOIN pg_type t ON t.oid = a.atttypid
          WHERE a.attnum > 0 AND NOT a.attisdropped
            AND (n.nspname, c.relname) IN (VALUES {tables})
          ORDER BY a.attnum"
    ))?;
    for row in rows {
        // None of these is ever null.
        let [
            schema,
            table,
            name,
            type_oid,
            typmod,
            type_name,
            typtype,
            generated,
        ] = <[Option<String>; 8]>::try_from(row)
            .map_err(|_| shape("columns"))?
            .map(Option::unwrap_or_default);
        let Some(class) = class_mut(classes, &schema, &table) else {
            return Err(shape("columns"));
        };
        class.columns.push(Column {
            name,
            type_oid: type_oid.parse().map_err(|_| shape("columns"))?,
            typmod: typmod.parse().map_err(|_| shape("columns"))?,
            type_name,
            typtype,
            generated,
        });
    }
    Ok(())
}

/// The class of `classes` that is `schema.table`, to add columns to.
// 'a: the class found is borrowed from `classes`, not from the names.
fn class_mut<'a>(classes: &'a mut [Class], schema: &str, table: &str) -> Option<&'a mut Class> {
    for class in classes {
        if class.schema == schema && class.name == table {
            return Some(class);
        }
    }
    None
}

/// Every label of every enum type a column of `tables`, an SQL list of
/// `(schema, table)` pairs, has.
pub(crate) fn labels(connection: &mut Connection, tables: &str) -> Result<Vec<EnumLabel>, Error> {
    let rows = connection.rows(&format!(
        "SELECT e.oid, e.enumtypid, e.enumlabel
           FROM pg_enum e
          WHERE e.enumtypid IN (SELECT a.atttypid
                                  FROM pg_attribute a
                                  JOIN pg_class c ON c.oid = a.attrelid
                                  JOIN pg_namespace n ON n.oid = c.relnamespace
                                 WHERE (n.nspname, c.relname) IN (VALUES {tables}))
          ORDER BY e.enumtypid, e.enumsortorder"
    ))?;
    let mut labels = Vec::new();
    for row in rows {
        let [oid, type_oid, label] = <[Option<String>; 3]>::try_from(row)
            .map_err(|_| shape("labels"))?
            .map(Option::unwrap_or_default);
        labels.push(EnumLabel {
            oid: oid.parse().map_err(|_| shape("labels"))?,
            type_oid: type_oid.parse().map_err(|_| shape("labels"))?,
            label,
        });
    }
    Ok(labels)
}
