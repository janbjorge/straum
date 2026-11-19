//! A spec table that stops being the one Straum loaded, with no change
//! the stream reads to say so: left out of the publication, renamed away
//! with another table made under its name, a label of an enum it reads
//! renamed, a column it reads rewritten to its own type. Each reading of
//! the WAL checks the tables, so Straum stops, and the setup check of the
//! next start names the problem. The physical stream reads the catalog's
//! own changes and stops on the DDL itself.

use std::thread;
use std::time::{Duration, Instant};

use straum_app::{Change, Source, Step};
use straum_domain::{Ask, Store, Value};

use super::references::{SPEC, TABLES, TASKS, TASKS_SPEC, compare, task_oracles};
use super::support::*;
use crate::catalog;
use crate::source::{AfterRead, Replication, hold_tables, read_in_time};
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
    load_all(&mut source, &spec);
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
    load_all(&mut source, &spec);
    db.run(MADE_AGAIN);
    db.run(&Rng::new(1).trip(1).replace("$S.trips", "$S.trips_old"));
    let commit = source.next_commit(LONG).unwrap().unwrap();
    assert_eq!(commit.changes, [Change::Unfit { table: id("trips") }]);
    let error = source.wal().unwrap_err().to_string();
    assert_eq!(
        error,
        db.sql("trips: $S.trips is another table than the one Straum loaded")
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
    load_all(&mut source, &spec);
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
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    db.run("ALTER TYPE $S.kind ADD VALUE 'bin'");
    db.run("INSERT INTO $S.tasks VALUES (3, 1, 1, '2024-01-15 12:00', NULL, 5, 'bin')");
    let written = db.insert_lsn();
    while writer.app().status().applied < Some(written) {
        applied(&mut writer);
    }
    compare(&mut db, writer.app(), &task_oracles()).expect("the added label reads as itself");

    db.run(
        "ALTER TYPE $S.kind RENAME VALUE 'shelf' TO 'rack';
         INSERT INTO $S.tasks VALUES (4, 1, 2, '2024-01-15 13:00', NULL, 6, 'rack')",
    );
    let deadline = Instant::now() + LONG;
    let error = loop {
        assert!(Instant::now() < deadline, "never stopped");
        match writer.step() {
            Ok(_) => {}
            Err(error) => break error.to_string(),
        }
    };
    assert!(
        error.starts_with("a change no longer fits tasks: ")
            || error.contains("a label of an enum"),
        "{error}"
    );
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
    let mut store = Store::new(spec.clone());
    for dim in spec.dims() {
        source.load(&dim.name, &mut store).unwrap();
    }
    for fact in spec.facts() {
        source.load(&fact.name, &mut store).unwrap();
    }
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
    let clock = Instant::now();
    let read = read_in_time(
        &mut connection,
        &spec,
        &db.name,
        logical,
        AfterRead::HoldTables,
    );
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
    read_in_time(
        &mut connection,
        &spec,
        &db.name,
        logical,
        AfterRead::HoldTables,
    )
    .unwrap();
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
    load_all(&mut source, &spec);
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
    load_all(&mut source, &spec);
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
