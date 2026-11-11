//! The setup SQL an admin runs once, which `straum plan` prints.

use postgres_protocol::escape::{escape_identifier, escape_literal};
use postgres_protocol::password::scram_sha_256;
use straum_domain::Spec;

use crate::catalog::escaped;

/// Turns on logical decoding, creates `role` with the replication
/// attribute, lets it read the spec's tables, and publishes every one of
/// them as `publication`. Each gets `REPLICA IDENTITY FULL`, so an update
/// or delete carries the old row Straum takes out of its cell or its
/// projection.
///
/// With a `password`, the role gets it as a SCRAM-SHA-256 verifier, so
/// neither the SQL nor the server's statement log holds the password
/// itself. Without one, the SQL says how to set it.
pub fn plan(spec: &Spec, publication: &str, role: &str, password: Option<&str>) -> String {
    let id = |s: &str| escape_identifier(s);
    let qualified = |t: &straum_domain::Table| escaped(&t.schema, &t.name);
    let tables = spec.tables();
    let schemas = once(tables.iter().map(|t| id(t.schema.as_str())));
    let list = tables
        .iter()
        .map(|t| qualified(t))
        .collect::<Vec<_>>()
        .join(", ");
    let (publication, role) = (id(publication), id(role));

    let mut sql = vec![
        "-- Straum setup: run once, as an owner of the tables.".to_owned(),
        "-- wal_level takes a restart. On Azure, set the server parameter instead.".to_owned(),
        "ALTER SYSTEM SET wal_level = logical;".to_owned(),
    ];
    match password {
        Some(password) => sql.push(format!(
            "CREATE ROLE {role} LOGIN REPLICATION PASSWORD {};",
            escape_literal(&scram_sha_256(password.as_bytes()))
        )),
        None => {
            sql.push(format!("CREATE ROLE {role} LOGIN REPLICATION;"));
            sql.push(format!("-- Then give {role} a password: \\password {role}"));
        }
    }
    sql.extend([
        format!("GRANT USAGE ON SCHEMA {} TO {role};", schemas.join(", ")),
        format!("GRANT SELECT ON {list} TO {role};"),
        format!("CREATE PUBLICATION {publication} FOR TABLE {list};"),
    ]);
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

/// Each item at its first place only.
fn once(items: impl Iterator<Item = String>) -> Vec<String> {
    let mut kept = Vec::new();
    for item in items {
        if !kept.contains(&item) {
            kept.push(item);
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A schema or a table named again apart from its first place still
    /// comes once.
    #[test]
    fn schemas_and_tables_come_once() {
        let view = |name: &str, table: &str| {
            format!(
                "CREATE VIEW {name} WITH (buckets = '1 hour') AS
                 SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket, count(*) AS n
                 FROM {table} AS f GROUP BY 1;"
            )
        };
        let views = [view("x", "a.t"), view("y", "b.u"), view("z", "a.t")].concat();
        let sql = plan(&straum_notation::parse(&views).unwrap(), "p", "r", None);
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
        let sql = plan(&spec, "p", "r", Some("s'cret"));
        let line = sql.lines().find(|l| l.starts_with("CREATE ROLE")).unwrap();
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
        assert_ne!(sql, plan(&spec, "p", "r", Some("s'cret")));
    }
}
