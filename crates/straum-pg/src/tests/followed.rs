//! A spec table that stops being the one Straum loaded, with no change
//! the stream reads to say so: left out of the publication, renamed away
//! with another table made under its name, a label of an enum it reads
//! renamed. Each reading of the WAL
//! checks the tables, so Straum starts over, and the setup check of the
//! next start names the problem. The physical stream reads the catalog's
//! own changes and starts over on the DDL itself.

use std::time::Instant;

use straum_app::{Change, Source, Step};

use super::references::{SPEC, TABLES, TASKS, TASKS_SPEC, compare, task_oracles};
use super::support::*;

/// The table made again under its name: the same columns as the one
/// renamed away, published.
const MADE_AGAIN: &str = "
ALTER TABLE $S.trips RENAME TO trips_old;
CREATE TABLE $S.trips (LIKE $S.trips_old INCLUDING ALL);
ALTER TABLE $S.trips REPLICA IDENTITY FULL;
ALTER PUBLICATION $S ADD TABLE $S.trips;
";

#[test]
fn a_table_left_out_of_the_publication_stops_the_stream() {
    let pg = postgres();
    let mut db = Db::new(&pg, "unpublished", TABLES);
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    source.open(&spec).unwrap();
    assert!(source.wal().is_ok());
    db.run("ALTER PUBLICATION $S DROP TABLE $S.trips");
    let error = source.wal().unwrap_err().to_string();
    assert_eq!(
        error,
        db.sql(
            "trips: publication $S leaves out $S.trips: ALTER PUBLICATION $S ADD TABLE $S.trips"
        )
    );
}

/// The table renamed away still sends its changes, under its new name:
/// they no longer read as the spec's. The reading of the WAL finds
/// another table under the name.
#[test]
fn a_table_renamed_away_and_made_again_stops_the_stream() {
    let pg = postgres();
    let mut db = Db::new(&pg, "replaced", TABLES);
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    source.open(&spec).unwrap();
    db.run(MADE_AGAIN);
    db.run(&Rng::new(1).trip(1).replace("$S.trips", "$S.trips_old"));
    let commit = source.next_commit(LONG).unwrap().unwrap();
    assert_eq!(commit.changes, [Change::Unfit { table: id("trips") }]);
    let error = source.wal().unwrap_err().to_string();
    assert_eq!(
        error,
        db.sql(
            "trips: $S.trips is another table than the one Straum loaded; Straum loads it again"
        )
    );
}

/// Tasks in two groups, one of each kind of task, so the view holds both
/// labels of `kind`.
const SOME_TASKS: &str = "
INSERT INTO $S.taskgroups VALUES (1, 1, 'pick'), (1, 2, 'pack');
INSERT INTO $S.tasks VALUES
    (1, 1, 1, '2024-01-15 10:00', NULL, 3, 'grid'),
    (2, 1, 2, '2024-01-15 11:00', NULL, 4, 'shelf');
";

/// A label renamed leaves the rows loaded with the old one, and sends
/// nothing the logical stream reads: the reading of the WAL finds it.
#[test]
fn a_label_renamed_stops_the_logical_stream() {
    let pg = postgres();
    let mut db = Db::new(&pg, "relabelled", TASKS);
    db.run(SOME_TASKS);
    let spec = db.spec(TASKS_SPEC);
    let mut source = db.pg();
    source.open(&spec).unwrap();
    assert!(source.wal().is_ok());
    // A label added is nothing: no row loaded holds it.
    db.run("ALTER TYPE $S.kind ADD VALUE 'bin'");
    assert!(source.wal().is_ok());
    db.run("ALTER TYPE $S.kind RENAME VALUE 'shelf' TO 'rack'");
    let error = source.wal().unwrap_err().to_string();
    assert_eq!(
        error,
        db.sql("tasks: a label of an enum $S.tasks reads was renamed; Straum loads it again")
    );
}

/// The physical stream reads an enum's labels again after a commit that
/// changes the catalog: a label added reads as itself, with no start
/// over, and a label renamed starts Straum over.
#[test]
fn the_physical_stream_reads_a_label_added_and_starts_over_on_one_renamed() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "relabelled_physical", TASKS);
    db.run(SOME_TASKS);
    let mut writer = db.physical_writer(TASKS_SPEC);
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    db.run("ALTER TYPE $S.kind ADD VALUE 'bin'");
    db.run("INSERT INTO $S.tasks VALUES (3, 1, 1, '2024-01-15 12:00', NULL, 5, 'bin')");
    let written = db.insert_lsn();
    while writer.app().status().applied < Some(written) {
        applied(&mut writer);
    }
    compare(&mut db, writer.app(), &task_oracles()).expect("the added label reads as itself");

    db.run("ALTER TYPE $S.kind RENAME VALUE 'shelf' TO 'rack'");
    let deadline = Instant::now() + LONG;
    let error = loop {
        assert!(Instant::now() < deadline, "never started over");
        match writer.step() {
            Ok(_) => {}
            Err(error) => break error.to_string(),
        }
    };
    assert!(error.contains("a label of an enum"), "{error}");
}
