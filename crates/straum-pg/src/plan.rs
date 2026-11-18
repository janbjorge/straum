//! The setup SQL a table owner runs once, which `straum plan` prints: the
//! role, its grants, and for logical decoding the publication (the set of
//! tables PostgreSQL sends changes of) and each table's replica identity
//! (what of the old row an update or delete carries). Pure text from the
//! spec; nothing here connects.

use postgres_protocol::escape::{escape_identifier, escape_literal};
use postgres_protocol::password::scram_sha_256;
use straum_domain::Spec;

use crate::catalog::escaped;
use crate::source::Replication;

/// Turns on logical decoding, creates `role` with the replication
/// attribute, lets it read the spec's tables, and publishes every one of
/// them as `publication`. Each gets `REPLICA IDENTITY FULL`, so an update
/// or delete carries the old row Straum takes out of its cell or its
/// projection.
///
/// With [`Replication::Physical`] none of that is needed: the physical
/// WAL `wal_level = replica` writes holds every change and the pages hold
/// the old rows (ADR-0008). The role may log which transactions run, so
/// a start needn't wait for the next checkpoint.
///
/// With a `password`, the role gets it as a SCRAM-SHA-256 verifier, so
/// neither the SQL nor the server's statement log holds the password
/// itself. Without one, the SQL says how to set it.
pub fn plan(
    spec: &Spec,
    publication: &str,
    role: &str,
    password: Option<&str>,
    replication: Replication,
) -> String {
    let tables = spec.tables();
    let mut schemas = Vec::new();
    let mut qualified_tables = Vec::new();
    for table in &tables {
        let schema = escape_identifier(table.schema.as_str());
        // Each schema at its first place only.
        if !schemas.contains(&schema) {
            schemas.push(schema);
        }
        qualified_tables.push(qualified(table));
    }
    let list = qualified_tables.join(", ");
    let publication = escape_identifier(publication);
    let role = escape_identifier(role);

    let mut sql = vec![
        "-- Straum setup: run once, as an owner of the tables; a superuser runs the lines said so."
            .to_owned(),
    ];
    match replication {
        Replication::Logical => sql.extend([
            "-- A superuser: wal_level takes a restart. On Azure, set the server parameter instead."
                .to_owned(),
            "ALTER SYSTEM SET wal_level = logical;".to_owned(),
        ]),
        Replication::Physical { .. } => sql.push(
            "-- --wal physical: wal_level replica and full_page_writes on, the defaults."
                .to_owned(),
        ),
    }
    sql.extend(role_lines(&role, password));
    sql.extend(grants(&schemas, &list, &role));
    if matches!(replication, Replication::Physical { .. }) {
        sql.extend([
            "-- Lets Straum start without waiting for the next checkpoint; a superuser grants it."
                .to_owned(),
            format!("GRANT EXECUTE ON FUNCTION pg_log_standby_snapshot() TO {role};"),
        ]);
        return sql.join("\n") + "\n";
    }
    sql.push(format!(
        "CREATE PUBLICATION {publication} FOR TABLE {list};"
    ));
    for fact in spec.facts() {
        sql.push(format!(
            "-- fact {}: Straum needs the old row of every update and delete; inserts cost no more.",
            fact.name
        ));
        sql.push(format!(
            "ALTER TABLE {} REPLICA IDENTITY FULL;",
            qualified(&fact.table)
        ));
    }
    for dim in spec.dims() {
        sql.push(format!(
            "-- dim {}: Straum holds its key and the columns the views read, and follows it.",
            dim.name
        ));
        sql.push(format!(
            "ALTER TABLE {} REPLICA IDENTITY FULL;",
            qualified(&dim.table)
        ));
    }
    sql.join("\n") + "\n"
}

/// Makes `role`, quoted already, with the password's verifier, or says
/// how to set one.
fn role_lines(role: &str, password: Option<&str>) -> Vec<String> {
    let mut sql = vec![
        "-- A superuser creates the role Straum connects as.".to_owned(),
        "-- REPLICATION is a privilege of that role, not another user.".to_owned(),
    ];
    if let Some(password) = password {
        sql.push(format!(
            "CREATE ROLE {role} LOGIN REPLICATION PASSWORD {};",
            escape_literal(&scram_sha_256(password.as_bytes()))
        ));
    } else {
        sql.push(format!("CREATE ROLE {role} LOGIN REPLICATION;"));
        sql.push(format!("-- Then give {role} a password: \\password {role}"));
    }
    sql
}

/// Lets `role` read `tables`, an SQL list, in `schemas`, and says what a
/// table with row-level security needs.
fn grants(schemas: &[String], tables: &str, role: &str) -> [String; 4] {
    [
        format!("GRANT USAGE ON SCHEMA {} TO {role};", schemas.join(", ")),
        format!("GRANT SELECT ON {tables} TO {role};"),
        "-- If a policy of row-level security hides rows of a table, a superuser runs:".to_owned(),
        format!("-- ALTER ROLE {role} BYPASSRLS;"),
    ]
}

/// The spec's tables, each as `plan` quotes it, in the order `plan` lists
/// them. A dump takes them as they are, so it needs no other parsing.
pub fn table_names(spec: &Spec) -> Vec<String> {
    spec.tables().into_iter().map(qualified).collect()
}

fn qualified(table: &straum_domain::Table) -> String {
    escaped(&table.schema, &table.name)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A schema or a table named again apart from its first place still
    /// comes once.
    /// A view `name` counting the rows of `table` by the hour.
    fn view(name: &str, table: &str) -> String {
        format!(
            "CREATE VIEW {name} WITH (buckets = '1 hour') AS
             SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket, count(*) AS n
             FROM {table} AS f GROUP BY 1;"
        )
    }

    /// The first line of `sql` that starts with `start`.
    // 'a: the line is borrowed from `sql`, not from `start`.
    fn line_starting<'a>(sql: &'a str, start: &str) -> &'a str {
        let mut lines = Vec::new();
        for line in sql.lines() {
            if line.starts_with(start) {
                lines.push(line);
            }
        }
        lines[0]
    }

    #[test]
    fn table_names_are_the_spec_tables_quoted_once_each() {
        let views = [view("x", "a.t"), view("y", "b.u"), view("z", "a.t")].concat();
        let spec = straum_notation::parse(&views).unwrap();
        assert_eq!(
            table_names(&spec),
            vec!["\"a\".\"t\"".to_owned(), "\"b\".\"u\"".to_owned()]
        );
    }

    #[test]
    fn schemas_and_tables_come_once() {
        let views = [view("x", "a.t"), view("y", "b.u"), view("z", "a.t")].concat();
        let sql = plan(
            &straum_notation::parse(&views).unwrap(),
            "p",
            "r",
            None,
            Replication::Logical,
        );
        assert!(
            sql.contains("GRANT USAGE ON SCHEMA \"a\", \"b\" TO \"r\";\n"),
            "{sql}"
        );
        assert!(
            sql.contains("CREATE PUBLICATION \"p\" FOR TABLE \"a\".\"t\", \"b\".\"u\";\n"),
            "{sql}"
        );
        assert!(
            sql.contains("CREATE ROLE \"r\" LOGIN REPLICATION;\n-- Then give \"r\" a password: \\password \"r\"\n"),
            "{sql}"
        );
    }

    /// The password goes in as its verifier, never as itself, and the
    /// quoting is PostgreSQL's.
    #[test]
    fn a_password_becomes_a_scram_verifier() {
        let spec = straum_notation::parse(
            "CREATE VIEW x WITH (buckets = '1 hour') AS
             SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket, count(*) AS n
             FROM a.t AS f GROUP BY 1;",
        )
        .unwrap();
        let sql = plan(&spec, "p", "r", Some("s'cret"), Replication::Logical);
        let line = line_starting(&sql, "CREATE ROLE");
        assert!(
            line.starts_with("CREATE ROLE \"r\" LOGIN REPLICATION PASSWORD 'SCRAM-SHA-256$4096:"),
            "{line}"
        );
        assert!(line.ends_with("';"), "{line}");
        assert!(
            !sql.contains("s'cret") && !sql.contains("\\password"),
            "{sql}"
        );
        // A fresh salt each time, so two plans never share a verifier.
        assert_ne!(
            sql,
            plan(&spec, "p", "r", Some("s'cret"), Replication::Logical)
        );
    }

    const PHYSICAL: Replication = Replication::Physical { max_page_memory: 1 };

    /// Physical: no `wal_level` change, publication or replica identity;
    /// the grants, and the one that lets a start skip the wait.
    #[test]
    fn physical_needs_only_the_role_and_grants() {
        let spec = straum_notation::parse(
            "CREATE VIEW x WITH (buckets = '1 hour') AS
             SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket, count(*) AS n
             FROM a.t AS f GROUP BY 1;",
        )
        .unwrap();
        let sql = plan(&spec, "p", "r", None, PHYSICAL);
        for absent in ["wal_level =", "PUBLICATION", "REPLICA IDENTITY"] {
            assert!(!sql.contains(absent), "{absent}: {sql}");
        }
        for present in [
            "CREATE ROLE \"r\" LOGIN REPLICATION;\n",
            "GRANT SELECT ON \"a\".\"t\" TO \"r\";\n",
            "-- ALTER ROLE \"r\" BYPASSRLS;\n",
            "GRANT EXECUTE ON FUNCTION pg_log_standby_snapshot() TO \"r\";\n",
        ] {
            assert!(sql.contains(present), "{present}: {sql}");
        }
    }
}
