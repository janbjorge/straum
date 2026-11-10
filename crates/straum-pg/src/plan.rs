//! The setup SQL an admin runs once, which `straum plan` prints.

use postgres_protocol::escape::escape_identifier;
use straum_domain::Spec;

/// Turns on logical decoding, creates `role` with the replication
/// attribute, lets it read the spec's tables, and publishes the facts as
/// `publication`. Facts get `REPLICA IDENTITY FULL`, so an update or
/// delete carries the old row straum takes out of its cell. Dims are only
/// read, by key, so they need a unique key and nothing else, which only
/// the schema can give; the SQL says it in a comment.
pub fn plan(spec: &Spec, publication: &str, role: &str) -> String {
    let id = |s: &str| escape_identifier(s);
    let qualified =
        |t: &straum_domain::Table| format!("{}.{}", id(t.schema.as_str()), id(t.name.as_str()));
    let tables = spec.tables();
    let schemas = once(tables.iter().map(|t| id(t.schema.as_str())));
    let list = tables
        .iter()
        .map(|t| qualified(t))
        .collect::<Vec<_>>()
        .join(", ");
    let facts = once(spec.facts().iter().map(|f| qualified(&f.table)));
    let (publication, role) = (id(publication), id(role));

    let mut sql = vec![
        "-- straum setup: run once, as an owner of the tables.".to_owned(),
        "-- wal_level takes a restart. On Azure, set the server parameter instead.".to_owned(),
        "ALTER SYSTEM SET wal_level = logical;".to_owned(),
        format!("CREATE ROLE {role} LOGIN REPLICATION;"),
        format!("-- Then give {role} a password: \\password {role}"),
        format!("GRANT USAGE ON SCHEMA {} TO {role};", schemas.join(", ")),
        format!("GRANT SELECT ON {list} TO {role};"),
        format!(
            "CREATE PUBLICATION {publication} FOR TABLE {};",
            facts.join(", ")
        ),
    ];
    for fact in spec.facts() {
        sql.push(format!(
            "-- fact {}: straum needs the old row of every update and delete; inserts cost no more.",
            fact.name
        ));
        sql.push(format!(
            "ALTER TABLE {} REPLICA IDENTITY FULL;",
            qualified(&fact.table)
        ));
    }
    for dim in spec.dims() {
        let key: Vec<&str> = dim.key.iter().map(|k| k.as_str()).collect();
        sql.push(format!(
            "-- dim {}: {} needs a primary key or unique index on {} alone; straum reads it and does not follow it.",
            dim.name,
            qualified(&dim.table),
            key.join(", ")
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

    /// A schema or a fact table named again apart from its first place
    /// still comes once.
    #[test]
    fn schemas_and_fact_tables_come_once() {
        let view = |name: &str, table: &str| {
            format!(
                "CREATE VIEW {name} AS
                 SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket, count(*) AS n
                 FROM {table} f GROUP BY 1;"
            )
        };
        let views = [view("x", "a.t"), view("y", "b.u"), view("z", "a.t")].concat();
        let sql = plan(&straum_notation::parse(&views).unwrap(), "p", "r");
        assert!(
            sql.contains("GRANT USAGE ON SCHEMA \"a\", \"b\" TO \"r\";\n"),
            "{sql}"
        );
        assert!(
            sql.contains("CREATE PUBLICATION \"p\" FOR TABLE \"a\".\"t\", \"b\".\"u\";\n"),
            "{sql}"
        );
    }
}
