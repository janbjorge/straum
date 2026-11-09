//! The setup SQL an admin runs once, which `straum plan` prints.

use postgres_protocol::escape::escape_identifier;
use straum_domain::Spec;

/// Turns on logical decoding, creates `role` with the replication
/// attribute, lets it read the spec's tables, and publishes them as
/// `publication`. Facts get `REPLICA IDENTITY FULL`, so an update or
/// delete carries the old row straum takes out of its cell; dims need their
/// key as replica identity, which only the schema can say, so the SQL says it
/// in a comment.
pub fn plan(spec: &Spec, publication: &str, role: &str) -> String {
    let id = |s: &str| escape_identifier(s);
    let qualified =
        |t: &straum_domain::Table| format!("{}.{}", id(t.schema.as_str()), id(t.name.as_str()));
    let tables = spec.tables();
    let mut schemas: Vec<String> = tables.iter().map(|t| id(t.schema.as_str())).collect();
    schemas.dedup();
    let list = tables
        .iter()
        .map(|t| qualified(t))
        .collect::<Vec<_>>()
        .join(", ");
    let (publication, role) = (id(publication), id(role));

    let mut sql = vec![
        "-- straum setup: run once, as an owner of the tables.".to_owned(),
        "-- wal_level takes a restart. On Azure, set the server parameter instead.".to_owned(),
        "ALTER SYSTEM SET wal_level = logical;".to_owned(),
        format!("CREATE ROLE {role} LOGIN REPLICATION;"),
        format!("-- Then give {role} a password: \\password {role}"),
        format!("GRANT USAGE ON SCHEMA {} TO {role};", schemas.join(", ")),
        format!("GRANT SELECT ON {list} TO {role};"),
        format!("CREATE PUBLICATION {publication} FOR TABLE {list};"),
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
        sql.push(format!(
            "-- dim {}: {} needs a primary key on {} alone, or REPLICA IDENTITY FULL and a unique index on it.",
            dim.name,
            qualified(&dim.table),
            dim.key
        ));
    }
    sql.join("\n") + "\n"
}
