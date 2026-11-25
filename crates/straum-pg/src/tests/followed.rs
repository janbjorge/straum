//! A spec table that stops being the one Straum loaded, with no change
//! the stream reads to say so: left out of the publication, its rows or
//! an action filtered out by the publication, renamed away
//! with another table made under its name, a parent of a table that
//! inherits from it, a label of an enum it reads renamed, a column it
//! reads rewritten to its own type. Each reading of
//! the WAL checks the tables, so Straum stops, and the setup check of the
//! next start names the problem. The physical stream reads the catalog's
//! own changes and stops on the DDL itself.

use std::thread;
use std::time::{Duration, Instant};

use straum_app::{Change, Drift, Source};
use straum_domain::{Ask, Value};

use super::references::{SPEC, TABLES, TASKS, TASKS_SPEC, compare, task_oracles};
use super::streams::*;
use super::support::*;
use crate::catalog;
use crate::source::{AfterRead, Replication, Settings, hold_tables, read_in_time};
use crate::wire::{Connection, ConnectionKind};

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
    load_all(&mut source, &spec).unwrap();
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

/// A publication altered to leave out an action, or to filter a table's
/// rows, sends nothing the stream reads, and the totals would drift: the
/// reading of the WAL finds it. All four actions again, unfiltered, keep
/// the stream.
#[test]
fn a_publication_narrowed_stops_the_stream() {
    let pg = postgres();
    let mut db = Db::new(&pg, "narrowed", TABLES);
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    source.open(&spec).unwrap();
    load_all(&mut source, &spec).unwrap();
    db.run("ALTER PUBLICATION $S SET (publish = 'insert')");
    let error = source.wal().unwrap_err().to_string();
    assert_eq!(
        error,
        db.sql("publication $S must publish insert, update, delete and truncate")
    );
    db.run("ALTER PUBLICATION $S SET (publish = 'insert, update, delete, truncate')");
    assert!(source.wal().is_ok());
    db.run(
        "ALTER PUBLICATION $S DROP TABLE $S.trips;
         ALTER PUBLICATION $S ADD TABLE $S.trips WHERE (fare > 0)",
    );
    let error = source.wal().unwrap_err().to_string();
    assert_eq!(
        error,
        db.sql("trips: publication $S filters the rows of $S.trips; Straum needs every row")
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
    load_all(&mut source, &spec).unwrap();
    db.run(MADE_AGAIN);
    db.run(&Rng::new(1).trip(1).replace("$S.trips", "$S.trips_old"));
    let commit = source.next_commit(LONG).unwrap().unwrap();
    assert_eq!(
        commit.changes,
        [Change::Unfit {
            table: id("trips"),
            drift: Drift::Altered,
        }]
    );
    let error = source.wal().unwrap_err().to_string();
    assert_eq!(
        error,
        db.sql("trips: $S.trips is another table than the one Straum loaded")
    );
}

/// The error of a spec table that another table inherits from.
const CHILDREN: &str = "trips: tables inherit from $S.trips; Straum reads a table without children";

/// A child made after the load: a query of `trips` counts the child's
/// rows, but their changes come under the child's name. The reading of
/// the WAL finds the child.
#[test]
fn a_child_made_after_the_load_stops_the_logical_stream() {
    let pg = postgres();
    let mut db = Db::new(&pg, "child_logical", TABLES);
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    source.open(&spec).unwrap();
    load_all(&mut source, &spec).unwrap();
    assert!(source.wal().is_ok());
    db.run("CREATE TABLE $S.trips_child () INHERITS ($S.trips)");
    db.run(&Rng::new(7).trip(100).replace("$S.trips", "$S.trips_child"));
    let error = source.wal().unwrap_err().to_string();
    assert_eq!(error, db.sql(CHILDREN));
}

/// The physical stream reads the WAL of the spec table's file alone, so
/// the child's rows never come to it either: the reading of the WAL, once
/// a second, finds the child.
#[test]
fn a_child_made_after_the_load_stops_the_physical_stream() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "child_physical", TABLES);
    let mut writer = db.physical_writer(SPEC);
    started(&mut writer);
    db.run("CREATE TABLE $S.trips_child () INHERITS ($S.trips)");
    let mut rng = Rng::new(7);
    db.run(&rng.trip(100).replace("$S.trips", "$S.trips_child"));
    // A trip of the parent each turn, so each step has a commit to apply.
    let wait = Wait::new("the writer to stop");
    let mut trip = 1;
    let error = loop {
        wait.check();
        db.run(&rng.trip(trip));
        trip += 1;
        if let Err(error) = writer.step() {
            break error.to_string();
        }
    };
    assert_eq!(error, db.sql(CHILDREN));
}

/// A table made a child of a spec table after the load is a child too.
#[test]
fn a_table_made_to_inherit_from_a_spec_table_stops_the_logical_stream() {
    let pg = postgres();
    let mut db = Db::new(&pg, "inherit_logical", TABLES);
    db.run("CREATE TABLE $S.extra (LIKE $S.trips)");
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    source.open(&spec).unwrap();
    load_all(&mut source, &spec).unwrap();
    assert!(source.wal().is_ok());
    db.run("ALTER TABLE $S.extra INHERIT $S.trips");
    let error = source.wal().unwrap_err().to_string();
    assert_eq!(error, db.sql(CHILDREN));
}

/// A spec table made a child of another table still reads its own rows
/// alone: the stream keeps on.
#[test]
fn a_spec_table_made_a_child_keeps_the_logical_stream() {
    let pg = postgres();
    let mut db = Db::new(&pg, "inherit_parent", TABLES);
    db.run("CREATE TABLE $S.base ()");
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    source.open(&spec).unwrap();
    load_all(&mut source, &spec).unwrap();
    db.run("ALTER TABLE $S.trips INHERIT $S.base");
    source.wal().unwrap();
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
    load_all(&mut source, &spec).unwrap();
    assert!(source.wal().is_ok());
    // A label added is nothing: no row loaded holds it.
    db.run("ALTER TYPE $S.kind ADD VALUE 'bin'");
    assert!(source.wal().is_ok());
    db.run("ALTER TYPE $S.kind RENAME VALUE 'shelf' TO 'rack'");
    let error = source.wal().unwrap_err().to_string();
    assert_eq!(
        error,
        db.sql("tasks: a label of an enum $S.tasks reads was renamed")
    );
}

/// The physical stream reads an enum's labels again after a commit that
/// changes one: a label added reads as itself, and a label renamed
/// stops Straum at the rename's commit, before the rows it holds. The
/// reading of the WAL, once a second, may find the rename first.
#[test]
fn the_physical_stream_reads_a_label_added_and_stops_on_one_renamed() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "relabelled_physical", TASKS);
    db.run(SOME_TASKS);
    let mut writer = db.physical_writer(TASKS_SPEC);
    started(&mut writer);
    db.run("ALTER TYPE $S.kind ADD VALUE 'bin'");
    let written = db
        .run_committed("INSERT INTO $S.tasks VALUES (3, 1, 1, '2024-01-15 12:00', NULL, 5, 'bin')");
    apply_through(
        &mut writer,
        written,
        &Wait::new(&format!("{written} applied")),
    );
    compare(&mut db, writer.app(), &task_oracles()).expect("the added label reads as itself");

    db.run(
        "ALTER TYPE $S.kind RENAME VALUE 'shelf' TO 'rack';
         INSERT INTO $S.tasks VALUES (4, 1, 2, '2024-01-15 13:00', NULL, 6, 'rack')",
    );
    let wait = Wait::new("the writer to stop");
    let error = loop {
        wait.check();
        match writer.step() {
            Ok(_) => {}
            Err(error) => break error.to_string(),
        }
    };
    // The stream marks the table unfit at the rename's commit, and the
    // catalog check once a second sees the label too: whichever comes
    // first, the error names the rename.
    assert!(error.contains("was renamed"), "{error}");
}

/// A rewrite gives its rows the xid of the transaction that made it, so
/// a snapshot taken before sees the table empty, and the logical stream
/// sends nothing of it. A start holds its tables until the load ends: a
/// rewrite asked for meanwhile waits for it, and the load reads every row.
#[test]
fn a_rewrite_during_the_load_waits_for_it() {
    let pg = postgres();
    let mut db = Db::new(&pg, "rewritten", TABLES);
    let mut rng = Rng::new(3);
    for id in 1..=5 {
        db.run(&rng.trip(id));
    }
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    source.open(&spec).unwrap();
    let mut other = db.another_connection();
    let rewrite = thread::spawn(move || {
        other.run("ALTER TABLE $S.trips ALTER COLUMN tip TYPE int USING tip + 0");
    });
    // Time for the rewrite to reach the lock, or, without one, to end.
    thread::sleep(Duration::from_millis(500));
    let store = load_all(&mut source, &spec).unwrap();
    rewrite.join().unwrap();
    let every_trip = Ask {
        view: id("trips"),
        measures: vec![id("trips")],
        keys: Vec::new(),
        filter: Vec::new(),
        having: Vec::new(),
        from: None,
        to: None,
        bounded: None,
        bound_zone: straum_domain::BoundZone::Utc,
    };
    let counted = store.ask(&every_trip).unwrap().rows[0].values[0].clone();
    assert_eq!(counted, Value::Int(5), "the load read the table empty");
}

/// A rewrite that ended after the snapshot but before the start held
/// its tables is found, and the start fails, to be made again; with
/// none, the tables are held.
#[test]
fn a_rewrite_before_the_tables_are_held_fails_the_start() {
    let pg = postgres();
    let mut db = Db::new(&pg, "rewritten_before", TABLES);
    let spec = db.spec(SPEC);
    for rewrite in [false, true] {
        let mut connection = Connection::connect(&db.dsn, ConnectionKind::Sql).unwrap();
        connection
            .execute("BEGIN READ ONLY ISOLATION LEVEL REPEATABLE READ")
            .unwrap();
        let tables = catalog::read(&mut connection, &spec, &db.name, Replication::Logical).unwrap();
        if rewrite {
            db.run("ALTER TABLE $S.trips ALTER COLUMN tip TYPE int USING tip + 0");
        }
        let held = hold_tables(&mut connection, &tables);
        if !rewrite {
            held.unwrap();
            continue;
        }
        assert_eq!(
            held.unwrap_err().to_string(),
            db.sql("trips: $S.trips was rewritten as Straum started")
        );
    }
}

/// A table another session locks, or waits to lock, against a read fails
/// the start at the lock timeout, naming the table, rather than hanging
/// it; once let go, the tables are read and held, and what follows waits
/// on no lock timeout. A whole start would wait first on the locker's
/// transaction as its slot is made, so the reading runs alone.
#[test]
fn a_table_locked_away_fails_the_start_in_time() {
    let pg = postgres();
    let db = Db::new(&pg, "lockedaway", TABLES);
    let spec = db.spec(SPEC);
    let mut locker = db.another_connection();
    locker.run("BEGIN; LOCK TABLE $S.trips IN ACCESS EXCLUSIVE MODE");
    let mut connection = Connection::connect(&db.dsn, ConnectionKind::Sql).unwrap();
    // A hang would otherwise outlast the test.
    let hang = Duration::from_secs(30);
    connection.set_read_timeout(hang).unwrap();
    let begin = "BEGIN READ ONLY ISOLATION LEVEL REPEATABLE READ";
    connection.execute(begin).unwrap();
    let logical = Replication::Logical;
    let settings = Settings::new(db.dsn.clone(), &db.name, ROW_LIMIT, logical, STREAM_MEMORY);
    let clock = Instant::now();
    let read = read_in_time(&mut connection, &spec, &settings, AfterRead::HoldTables);
    assert!(clock.elapsed() < hang, "the start waited out the lock");
    assert_eq!(
        read.err().unwrap().to_string(),
        db.sql(
            "the start waited 5 s for a lock on $S.trips; another session holds or waits \
             for one that blocks a read, as ALTER TABLE takes"
        )
    );
    locker.run("ROLLBACK");
    connection.execute(begin).unwrap();
    read_in_time(&mut connection, &spec, &settings, AfterRead::HoldTables).unwrap();
    let shown = connection.rows("SHOW lock_timeout").unwrap();
    assert_eq!(shown, [[Some("0".to_owned())]]);
}

/// `ALTER COLUMN .. TYPE` to the same type, `USING` an expression,
/// rewrites every row and sends nothing the logical stream reads: the
/// reading of the WAL finds the column's catalog row written anew. A
/// `VACUUM FULL` rewrites the rows with the same values and a column
/// added is no column Straum reads: neither stops Straum.
#[test]
fn a_column_altered_to_its_own_type_stops_the_logical_stream() {
    let pg = postgres();
    let mut db = Db::new(&pg, "usingsame", TABLES);
    let mut rng = Rng::new(3);
    for id in 1..=5 {
        db.run(&rng.trip(id));
    }
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    source.open(&spec).unwrap();
    load_all(&mut source, &spec).unwrap();
    db.run("VACUUM FULL $S.trips");
    db.run("ALTER TABLE $S.trips ADD COLUMN note text");
    assert!(source.wal().is_ok());
    db.run("ALTER TABLE $S.trips ALTER COLUMN fare TYPE numeric(10, 2) USING fare + 1");
    let error = source.wal().unwrap_err().to_string();
    assert_eq!(
        error,
        db.sql("trips: a column Straum reads of $S.trips was altered")
    );
}

/// A grant on a column, its statistics target, its storage and a first
/// default write its catalog row anew and leave its values and type as
/// they were: none stops Straum. A type of another width does.
#[test]
fn a_column_granted_keeps_the_logical_stream_and_one_retyped_stops_it() {
    let pg = postgres();
    let mut db = Db::new(&pg, "granted", TABLES);
    db.run(&Rng::new(3).trip(1));
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    source.open(&spec).unwrap();
    load_all(&mut source, &spec).unwrap();
    for kept in [
        "GRANT SELECT (fare) ON $S.trips TO PUBLIC",
        "ALTER TABLE $S.trips ALTER COLUMN fare SET STATISTICS 500",
        "ALTER TABLE $S.trips ALTER COLUMN rider SET STORAGE EXTERNAL",
        "ALTER TABLE $S.trips ALTER COLUMN tip SET DEFAULT 0",
    ] {
        db.run(kept);
        source
            .wal()
            .unwrap_or_else(|error| panic!("{kept}: {error}"));
    }
    db.run("ALTER TABLE $S.trips ALTER COLUMN fare TYPE numeric(12, 2)");
    let error = source.wal().unwrap_err().to_string();
    assert_eq!(
        error,
        db.sql("trips: a column Straum reads of $S.trips was altered")
    );
}
