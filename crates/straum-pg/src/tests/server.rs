//! The server's part of the setup check: its version, its `wal_level`,
//! whether it is a standby and whether `synchronous_standby_names` could
//! name Straum, from the catalog Straum reads at each start, and the
//! timeouts each connection turns off.

use straum_app::Source;
use straum_domain::Spec;

use super::references::*;
use super::support::*;
use crate::catalog::{self, Catalog};
use crate::source::Replication;
use crate::wire::{Connection, ConnectionKind};

#[test]
fn says_to_set_wal_level_logical() {
    for tag in ["17", "18"] {
        let pg = postgres_at_replica(tag);
        let db = Db::new(&pg, "replica", TABLES);
        let error = db.pg().open(&db.spec(SPEC)).unwrap_err().to_string();
        assert!(
            error.contains("wal_level is replica; Straum needs logical, which takes a restart"),
            "{tag}: {error}"
        );
    }
}

/// PostgreSQL 16 lacks `transaction_timeout`, so Straum leaves it unset
/// there and the check says the server is too old, not that a setting is
/// unknown.
#[test]
fn says_postgresql_16_is_too_old() {
    let pg = postgres_at_replica("16");
    let db = Db::new(&pg, "old", "");
    let error = db.pg().open(&db.spec(SPEC)).unwrap_err().to_string();
    assert!(
        error.contains("PostgreSQL 16 is too old; Straum needs 17 or newer"),
        "{error}"
    );
}

/// A `synchronous_standby_names` that could name Straum's connection
/// fails the check, naming the setting; one naming other standbys
/// passes. Nothing commits while either is set: no standby answers.
#[test]
fn refuses_synchronous_standby_names_that_name_straum() {
    let pg = postgres();
    let mut db = Db::new(&pg, "syncnames", TABLES);
    let spec = db.spec(SPEC);
    let named = "synchronous_standby_names is 'ANY 1 (*)', which names Straum's connection, \
                 application_name straum: a commit only Straum confirmed could be lost on \
                 failover; name your standbys there, not * or straum";
    for (setting, expected) in [("ANY 1 (*)", Some(named)), ("FIRST 1 (replica1)", None)] {
        db.run(&format!(
            "ALTER SYSTEM SET synchronous_standby_names = '{setting}'"
        ));
        db.run("SELECT pg_reload_conf()");
        let wait = Wait::new("the reloaded setting");
        let shown = Some(setting.to_owned());
        while db
            .connection
            .rows("SHOW synchronous_standby_names")
            .unwrap()[0][0]
            != shown
        {
            wait.poll();
        }
        let catalog = catalog::query(&mut db.connection, &spec, &db.name).unwrap();
        let lines = check_lines(&spec, &catalog, &db.name, Replication::Logical);
        assert_eq!(
            lines,
            expected.map(|line| vec![line.to_owned()]),
            "{setting}"
        );
    }
}

/// A timeout set on the database would cancel a long load: from 17 on,
/// each connection turns `transaction_timeout` off for itself.
#[test]
fn a_connection_turns_the_transaction_timeout_off() {
    let pg = postgres();
    let mut db = Db::new(&pg, "timeouts", "");
    db.run("ALTER DATABASE postgres SET transaction_timeout = '5s'");
    let mut fresh = db.another_connection();
    assert_eq!(
        fresh.rows("SHOW transaction_timeout"),
        [[Some("0".to_owned())]]
    );
    let mut replication = Connection::connect(&pg.dsn, ConnectionKind::Replication).unwrap();
    let shown = replication.rows("SHOW transaction_timeout").unwrap();
    assert_eq!(shown, [[Some("0".to_owned())]]);
}

/// The catalog read gives the server's version and passes; changed by
/// hand, it gives each problem a version, a `wal_level` or a standby
/// makes.
#[test]
fn reports_the_server_version_and_wal_level() {
    let pg = postgres();
    let mut db = Db::new(&pg, "access", TABLES);
    let spec = db.spec(SPEC);
    let mut catalog = catalog::query(&mut db.connection, &spec, "access").unwrap();
    assert!(!catalog.in_recovery);
    assert!(catalog::check(&spec, &catalog, "access", Replication::Logical).is_ok());
    assert_eq!(catalog.server_version / 10_000, major());
    the_wal_level_each_major_needs(&spec, &mut catalog);
    too_old_and_standby_say_so_alone(&spec, &mut catalog);
}

/// Logical decoding needs `wal_level = logical` before 19, and `replica`
/// from 19 on.
fn the_wal_level_each_major_needs(spec: &Spec, catalog: &mut Catalog) {
    for (version, wal_level, needs) in [
        (170_000, "replica", Some("logical")),
        (170_000, "logical", None),
        (180_000, "replica", Some("logical")),
        (180_000, "logical", None),
        (190_000, "replica", None),
        (190_000, "logical", None),
        (190_000, "minimal", Some("replica or logical")),
        (200_000, "replica", None),
    ] {
        catalog.server_version = version;
        catalog.wal_level = wal_level.into();
        let mut expected = None;
        if let Some(needs) = needs {
            expected = Some(vec![format!(
                "wal_level is {wal_level}; Straum needs {needs}, which takes a restart"
            )]);
        }
        assert_eq!(
            check_lines(spec, catalog, "access", Replication::Logical),
            expected,
            "{version} {wal_level}"
        );
    }
}

/// A server too old, or a standby, is all the check says.
fn too_old_and_standby_say_so_alone(spec: &Spec, catalog: &mut Catalog) {
    catalog.server_version = 160_009;
    catalog.wal_level = "logical".into();
    assert_eq!(
        check_lines(spec, catalog, "access", Replication::Logical),
        Some(vec![
            "PostgreSQL 16 is too old; Straum needs 17 or newer".to_owned()
        ])
    );
    // A standby is no source, whatever else it is.
    catalog.server_version = 180_000;
    catalog.in_recovery = true;
    assert_eq!(
        check_lines(spec, catalog, "access", Replication::Logical),
        Some(vec![
            "the server is a standby in recovery; give Straum the primary's DSN".to_owned()
        ])
    );
    catalog.in_recovery = false;
    // A server too old has its tables left unread, whose columns its
    // catalog may lack: the check names the version alone.
    catalog.server_version = 160_009;
    let tables = std::mem::take(&mut catalog.classes);
    let too_old = catalog::check(spec, catalog, "access", Replication::Logical);
    assert_eq!(too_old.unwrap_err().len(), 1);
    catalog.server_version = 170_000;
    let unread = catalog::check(spec, catalog, "access", Replication::Logical);
    assert!(unread.unwrap_err().len() > 1);
    catalog.classes = tables;
}
