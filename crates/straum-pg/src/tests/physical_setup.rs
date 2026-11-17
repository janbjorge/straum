//! What the setup check refuses under `--wal physical` alone: tables and
//! columns whose changes the physical WAL doesn't hold as the load reads
//! them. Each case reads a real server's catalog, then checks it.

use straum_domain::Spec;

use super::support::*;
use crate::catalog;

/// A fact of `columns` besides its time.
fn tables(columns: &str) -> String {
    format!("CREATE TABLE $S.f (at timestamptz, {columns});")
}

/// Counts the fact by the hour and by `kind`.
const SPEC: &str = "
CREATE VIEW f WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket, t.kind AS kind, count(*) AS n
FROM $S.f AS t GROUP BY 1, 2;";

/// What the check for `--wal physical` finds wrong with the catalog `db`
/// reads for `spec`.
fn physical_problems(db: &mut Db, spec: &Spec) -> Vec<String> {
    let catalog = catalog::query(&mut db.connection, spec, "unpublished").unwrap();
    match catalog::check(spec, &catalog, "unpublished", PHYSICAL) {
        Ok(_) => Vec::new(),
        Err(problems) => crate::problem::lines(&problems),
    }
}

/// An unlogged table writes no WAL, so the stream would never see it
/// change; a logged one is followed.
#[test]
fn an_unlogged_table_is_refused() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(
        &pg,
        "unlogged",
        &tables("kind text").replace("TABLE", "UNLOGGED TABLE"),
    );
    let spec = db.spec(SPEC);
    assert_eq!(
        physical_problems(&mut db, &spec),
        [
            "f: unlogged.f is unlogged, so the WAL holds none of its changes: \
          ALTER TABLE unlogged.f SET LOGGED"
        ]
    );
    db.run("ALTER TABLE $S.f SET LOGGED");
    assert_eq!(physical_problems(&mut db, &spec), Vec::<String>::new());
}
