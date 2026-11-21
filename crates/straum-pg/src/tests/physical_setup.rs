//! What the setup check asks under `--wal physical` alone: a server
//! version and settings whose WAL Straum reads, no publication, and no
//! table or column whose changes the physical WAL doesn't hold as the load
//! reads them. Each case reads a real server's catalog, then checks it.

use straum_domain::Spec;

use super::references::{self, TABLES};
use super::support::*;
use crate::catalog;
use crate::source::Replication;

/// A fact of `columns` besides its time.
fn tables(columns: &str) -> String {
    format!("CREATE TABLE $S.f (at timestamptz, {columns});")
}

/// Counts the fact by the hour and by `kind`.
const SPEC: &str = "
CREATE VIEW f AS
SELECT buckets(t.at, '1 hour') AS bucket, t.kind AS kind, count(*) AS n
FROM $S.f AS t GROUP BY 1, 2;";

/// What the check for `--wal physical` finds wrong with the catalog `db`
/// reads for `spec`.
fn physical_problems(db: &mut Db, spec: &Spec) -> Option<Vec<String>> {
    let catalog = catalog::query(&mut db.connection, spec, "unpublished").unwrap();
    check_lines(spec, &catalog, "unpublished", PHYSICAL)
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
        Some(vec![
            "f: unlogged.f is unlogged, so the WAL holds none of its changes: \
          ALTER TABLE unlogged.f SET LOGGED"
                .to_owned()
        ])
    );
    db.run("ALTER TABLE $S.f SET LOGGED");
    assert_eq!(physical_problems(&mut db, &spec), None);
}

/// A virtual generated column, from 18, is computed as it is read and the
/// pages hold no value of it; a stored one is read like any other.
#[test]
fn a_virtual_generated_column_is_refused() {
    let pg = postgres_at_replica(&tag());
    let generated = "raw text, kind text GENERATED ALWAYS AS (upper(raw))";
    let mut db = Db::new(&pg, "stored", &tables(&format!("{generated} STORED")));
    let spec = db.spec(SPEC);
    assert_eq!(physical_problems(&mut db, &spec), None);
    if major() < 18 {
        return;
    }
    let mut db = Db::new(&pg, "virtual", &tables(&format!("{generated} VIRTUAL")));
    let spec = db.spec(SPEC);
    assert_eq!(
        physical_problems(&mut db, &spec),
        Some(vec![
            "f: column kind of virtual.f is virtual, so the WAL holds no value of it; \
          --wal physical needs it STORED"
                .to_owned()
        ])
    );
}

/// The stream reads text from the pages as UTF-8, so a database in
/// another encoding is refused.
#[test]
fn a_database_not_in_utf8_is_refused() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "encoding", &tables("kind text"));
    let spec = db.spec(SPEC);
    let mut catalog = catalog::query(&mut db.connection, &spec, "unpublished").unwrap();
    assert_eq!(catalog.server_encoding, "UTF8");
    assert_eq!(check_lines(&spec, &catalog, "unpublished", PHYSICAL), None);
    catalog.server_encoding = "LATIN1".into();
    assert_eq!(
        check_lines(&spec, &catalog, "unpublished", PHYSICAL),
        Some(vec![
            "the database's encoding is LATIN1; --wal physical reads UTF8 text".to_owned()
        ])
    );
}

/// `--wal physical` needs neither the publication nor `REPLICA IDENTITY
/// FULL`, takes `wal_level` replica, and needs PostgreSQL 17 or newer,
/// full-page writes and a `wal_compression` it reads.
#[test]
fn checks_what_the_physical_wal_needs() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(
        &pg,
        "needs",
        &TABLES
            .replace("REPLICA IDENTITY FULL", "REPLICA IDENTITY DEFAULT")
            .replace(
                "CREATE PUBLICATION $S FOR TABLE $S.zones, $S.trips, $S.logins;",
                "",
            ),
    );
    let spec = db.spec(references::SPEC);
    let mut catalog = catalog::query(&mut db.connection, &spec, "needs").unwrap();
    assert_eq!(
        (catalog.full_page_writes.as_str(), catalog.publication),
        ("on", catalog::PublicationState::Missing)
    );
    assert_eq!(check_lines(&spec, &catalog, "needs", PHYSICAL), None);
    let logical = catalog::check(&spec, &catalog, "needs", Replication::Logical).unwrap_err();
    let mut publication_missing = false;
    for problem in &logical {
        if problem
            .to_string()
            .contains("publication needs does not exist")
        {
            publication_missing = true;
        }
    }
    assert!(publication_missing);
    for server in servers_and_their_problems() {
        catalog.server_version = server.version;
        catalog.wal_level = server.wal_level.into();
        catalog.full_page_writes = server.full_page_writes.into();
        catalog.wal_compression = server.wal_compression.into();
        let mut expected = None;
        if !server.problems.is_empty() {
            let mut problems = Vec::new();
            for problem in &server.problems {
                problems.push((*problem).to_owned());
            }
            expected = Some(problems);
        }
        let Server {
            version,
            wal_level,
            full_page_writes,
            wal_compression,
            ..
        } = server;
        assert_eq!(
            check_lines(&spec, &catalog, "needs", PHYSICAL),
            expected,
            "{version} {wal_level} {full_page_writes} {wal_compression}"
        );
    }
}

/// A server's version and settings as the catalog reads them, and the
/// problems `--wal physical` finds with them.
// `'static`: every setting and problem is a literal written here.
struct Server {
    version: u32,
    wal_level: &'static str,
    full_page_writes: &'static str,
    wal_compression: &'static str,
    problems: Vec<&'static str>,
}

/// Servers `--wal physical` takes, then three it doesn't.
fn servers_and_their_problems() -> [Server; 6] {
    [
        server(170_000, ["replica", "on", "lz4"], vec![]),
        server(180_000, ["logical", "on", "pglz"], vec![]),
        server(190_000, ["replica", "on", "on"], vec![]),
        server(
            160_009,
            ["minimal", "on", "off"],
            vec![
                "PostgreSQL 16 is too old; Straum needs 17 or newer",
                "wal_level is minimal; Straum needs replica or logical, which takes a restart",
            ],
        ),
        server(
            200_000,
            ["replica", "on", "lz4"],
            vec![
                "--wal physical reads the WAL of PostgreSQL 17 to 19, not 20; read it through logical decoding",
            ],
        ),
        server(
            180_000,
            ["replica", "off", "zstd"],
            vec![
                "full_page_writes is off; --wal physical needs on",
                "wal_compression is zstd; --wal physical reads off, pglz and lz4",
            ],
        ),
    ]
}

/// A server of `version` with `wal_level`, `full_page_writes` and
/// `wal_compression` as `settings` lists them.
fn server(version: u32, settings: [&'static str; 3], problems: Vec<&'static str>) -> Server {
    let [wal_level, full_page_writes, wal_compression] = settings;
    Server {
        version,
        wal_level,
        full_page_writes,
        wal_compression,
        problems,
    }
}
