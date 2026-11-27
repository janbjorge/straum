//! The physical stream end to end (`--wal physical`): Straum reads the
//! physical WAL (the write-ahead log PostgreSQL writes every page change
//! to) at `wal_level = replica`, starts from a snapshot, and follows on.
//! Also what holds a start back.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use straum_app::{App, Source, Writer};

use super::logical::{a_wide_transaction_stops, stream_capped_writer};
use super::references::*;
use super::streams::*;
use super::support::*;
use crate::catalog::Checked;
use crate::physical::{Physical, read_layouts_seeing};
use crate::source::Replication;
use crate::wire::{Connection, ConnectionKind};

/// The physical stream holds the pages changed since the last
/// checkpoint's redo point and the changes of open transactions, not the
/// table: after a checkpoint and a few changes, a few pages' worth.
#[test]
fn holds_only_what_changed_since_the_checkpoint() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "bounded", TABLES);
    let (mut stream, _) = Physical::start(&pg.dsn).unwrap();
    db.run("SELECT pg_log_standby_snapshot()");
    let spec = db.spec(SPEC);
    let mut pipeline = Pipeline::new(&pg, &spec, &db.name, stream.major());
    // Under the transaction limit, a statement at a time.
    for from in (1..30_000).step_by(6_000) {
        db.run(&format!(
            "INSERT INTO $S.trips SELECT i, timestamptz '2024-01-01' + i * interval '1 minute',
                    'cab', 1 + i % 3, i % 70 + 0.25, md5(i::text), i % 9
             FROM generate_series({from}, {}) AS i",
            from + 5_999
        ));
        db.run(&format!(
            "UPDATE $S.trips SET tip = tip + 1 WHERE id BETWEEN {from} AND {}",
            from + 5_999
        ));
    }
    pipeline.through_now(&mut db, &mut stream);
    let table: usize = db.rows("SELECT pg_relation_size('$S.trips')")[0][0]
        .as_deref()
        .unwrap()
        .parse()
        .unwrap();
    let everything = pipeline.inner.bytes();
    assert!(
        everything > table / 2,
        "{everything} bytes for a table of {table}"
    );
    db.run("CHECKPOINT");
    // Two pages: the first rows and the last.
    db.run("UPDATE $S.trips SET tip = 0 WHERE id IN (1, 2, 30000)");
    // An open transaction's changes count until it ends.
    let mut open = Connection::connect(&pg.dsn, ConnectionKind::Sql).unwrap();
    open.execute("BEGIN").unwrap();
    open.execute(&db.sql("UPDATE $S.trips SET fare = 1 WHERE id <= 100"))
        .unwrap();
    pipeline.through_now(&mut db, &mut stream);
    let held = pipeline.inner.bytes();
    assert!(
        held < 8 * HELD_PAGE_BYTES,
        "{held} bytes held after the checkpoint"
    );
    open.execute("COMMIT").unwrap();
    db.run("CHECKPOINT");
    pipeline.through_now(&mut db, &mut stream);
    assert!(
        pipeline.inner.bytes() < 2 * HELD_PAGE_BYTES,
        "{}",
        pipeline.inner.bytes()
    );
}

/// The bytes allowed per page the physical stream holds: the 8 KiB page
/// and 4 KiB to spare.
const HELD_PAGE_BYTES: usize = 8192 + 4096;

/// A `--max-page-memory` of 64 KiB, which 5000 rows pass.
const SMALL_PAGE_MEMORY_BYTES: usize = 64 << 10;

/// Past `--max-page-memory` the physical stream stops and says so, and
/// Straum stops rather than grow.
#[test]
fn stops_past_the_page_memory() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "capped", TABLES);
    let spec = db.spec(SPEC);
    let small = Replication::Physical {
        max_page_memory: SMALL_PAGE_MEMORY_BYTES,
    };
    let mut source = source_over(db.dsn.clone(), &db.name, small);
    source.open(&spec).unwrap();
    db.run(
        "INSERT INTO $S.trips SELECT i, now(), 'cab', 1, 1.5, md5(i::text), 1
         FROM generate_series(1, 5000) AS i",
    );
    let wait = Wait::new("the stream to stop");
    let error = loop {
        wait.check();
        match source.next_commit(Duration::from_millis(100)) {
            Ok(_) => {}
            Err(error) => break error.to_string(),
        }
    };
    assert!(
        error.starts_with("the physical stream holds ")
            && error.ends_with(" bytes of rows, past the --max-page-memory limit of 65536"),
        "{error}"
    );
}

/// `straum run --wal physical` at `wal_level = replica`: it loads while
/// writers insert, update and delete, starts from a snapshot every later
/// commit streams after, follows under each `wal_compression` it reads,
/// and every view equals SQL's.
#[test]
fn follows_the_physical_wal() {
    let checksums = checksums_by_default();
    follow(&postgres_at_replica(&tag()), checksums);
}

/// As above, with data checksums the other way from the major's default.
/// With checksums a page's first hint bits after a checkpoint log a
/// full-page image.
#[test]
fn follows_the_physical_wal_with_checksums_flipped() {
    let checksums = !checksums_by_default();
    let initdb = if checksums {
        "--data-checksums"
    } else {
        "--no-data-checksums"
    };
    let request = image(&tag())
        .with_env_var("POSTGRES_INITDB_ARGS", initdb)
        .with_cmd(&["-c", "wal_level=replica"]);
    follow(&plain(request), checksums);
}

/// On 19 without data checksums, a plain `VACUUM` after a checkpoint
/// logs each visibility-map set as a prune that removes nothing and
/// brings no page image: the stream passes it and goes on.
#[test]
fn follows_a_vacuum_on_19_without_checksums() {
    let request = image(NINETEEN)
        .with_env_var("POSTGRES_INITDB_ARGS", "--no-data-checksums")
        .with_cmd(&["-c", "wal_level=replica"]);
    let pg = plain(request);
    let mut db = Db::new(&pg, "vacuumed", TABLES);
    let mut rng = Rng::new(3);
    for id in 0..20 {
        db.run(&rng.trip(id));
    }
    let mut writer = db.physical_writer(SPEC);
    started(&mut writer);
    db.run("CHECKPOINT");
    db.run("VACUUM $S.trips");
    db.run(&rng.trip(100));
    applied(&mut writer);
}

/// Whether the major under test turns data checksums on by default, as
/// 18 first does.
fn checksums_by_default() -> bool {
    major() >= 18
}

fn follow(pg: &PostgresContainer, checksums: bool) {
    let mut db = Db::new(pg, "physical_run", TABLES);
    let expected = if checksums { "on" } else { "off" };
    assert_eq!(
        db.rows("SHOW data_checksums")[0][0].as_deref(),
        Some(expected)
    );
    let mut rng = Rng::new(17);
    db.run(
        "INSERT INTO $S.zones VALUES (1, 'Queens', 1.5), (2, 'Bronx', NULL), (3, 'Manhattan', 9.5)",
    );
    for id in 0..50 {
        db.run(&rng.trip(id));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let mut writers = Vec::new();
    for index in 0..3 {
        writers.push(write_while_starting(&stop, db.another_connection(), index));
    }
    thread::sleep(Duration::from_millis(200));
    let mut writer = db.physical_writer(SPEC);
    started(&mut writer);
    stop.store(true, Ordering::Relaxed);
    for writer in writers {
        writer.join().unwrap();
    }
    let mut next = 5_000_000;
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
    // After a checkpoint, a page's first change logs the whole page,
    // compressed as `wal_compression` says.
    for (round, compression) in ["off", "pglz", "lz4"].into_iter().enumerate() {
        db.run(&format!("ALTER SYSTEM SET wal_compression = {compression}"));
        db.run("SELECT pg_reload_conf()");
        db.run("CHECKPOINT");
        db.run(&format!(
            "UPDATE $S.trips SET kind = 'van', tip = tip + 1 WHERE id % 3 = {round}"
        ));
        db.run(&format!("DELETE FROM $S.trips WHERE id % 4 = {round}"));
        db.run(&format!(
            "UPDATE $S.zones SET borough = 'Brooklyn' WHERE id = {}",
            round + 1
        ));
        catch_up(&mut db, &mut writer, &mut rng, &mut next);
    }
}

/// A writer thread that until `stop` inserts a trip, updates and deletes
/// earlier ones and changes zone `index + 1`, a commit each; writer 0 also
/// adds and drops a column now and then.
fn write_while_starting(stop: &Arc<AtomicBool>, mut db: Db, index: u64) -> thread::JoinHandle<()> {
    let stop = Arc::clone(stop);
    thread::spawn(move || {
        let mut rng = Rng::new(100 + index);
        let mut id = 1_000_000 * (index + 1);
        while !stop.load(Ordering::Relaxed) {
            id += 1;
            let sql = format!(
                "BEGIN; {} UPDATE $S.trips SET fare = fare + 1 WHERE id = {};
                 DELETE FROM $S.trips WHERE id = {}; UPDATE $S.zones SET toll = toll + 0.5
                 WHERE id = {}; COMMIT",
                rng.trip(id),
                id - 1,
                id - 2,
                1 + index
            );
            db.run(&sql);
            // DDL while Straum starts: a column comes and goes.
            if index == 0 && id.is_multiple_of(25) {
                let ddl = format!(
                    "ALTER TABLE $S.trips ADD c{id} int DEFAULT 7;
                     UPDATE $S.trips SET c{id} = 8 WHERE id = {id};
                     ALTER TABLE $S.trips DROP c{id}"
                );
                db.run(&ddl);
            }
        }
    })
}

/// A column of an enum added while the physical stream runs reads as its
/// label: the layouts read again after DDL bring their enums' labels.
#[test]
fn an_enum_column_added_while_streaming_reads_as_its_labels() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "added_enum", TABLES);
    let mut rng = Rng::new(23);
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5)");
    db.run(&rng.trip(1));
    let mut writer = db.physical_writer(SPEC);
    started(&mut writer);
    let mut next = 1;
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
    db.run(
        "CREATE TYPE $S.mood AS ENUM ('calm', 'rushed');
         ALTER TABLE $S.trips ADD mood $S.mood",
    );
    db.run("UPDATE $S.trips SET mood = 'rushed', tip = tip + 1");
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
}

/// The connection that reads the layouts idles until the next DDL. An
/// `idle_session_timeout` that the database sets does not close it: each
/// connection turns the timeout off. The commits keep the other
/// connections busy, which read once a second.
#[test]
fn ddl_after_the_idle_session_timeout_keeps_the_physical_stream() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "idle", TABLES);
    db.run("ALTER DATABASE postgres SET idle_session_timeout = '3s'");
    let mut rng = Rng::new(29);
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5)");
    db.run(&rng.trip(1));
    let mut writer = db.physical_writer(SPEC);
    started(&mut writer);
    let mut next = 1;
    let idle = Instant::now();
    while idle.elapsed() < Duration::from_secs(5) {
        catch_up(&mut db, &mut writer, &mut rng, &mut next);
    }
    db.run("ALTER TABLE $S.trips ADD note text");
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
}

/// The layouts are read again after a commit that changes an enum, as a
/// label added does, and not after one that changes another catalog in
/// the same database, as a table Straum doesn't read made or analyzed,
/// or a temporary table, do.
#[test]
fn reads_the_layouts_again_after_an_enum_changes_alone() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "enum_changes", TASKS);
    let (mut stream, _) = Physical::start(&pg.dsn).unwrap();
    db.run("SELECT pg_log_standby_snapshot()");
    let spec = db.spec(TASKS_SPEC);
    let mut pipeline = Pipeline::new(&pg, &spec, &db.name, stream.major());
    // From the checkpoint before the tables were made, which reads them.
    pipeline.through_now(&mut db, &mut stream);
    let made = pipeline.layouts_read;
    // `ANALYZE` of a table Straum reads invalidates its relcache, as DDL
    // on it does, so it reads that table's layout again.
    for statement in [
        "INSERT INTO $S.tasks VALUES (1, 1, 1, '2024-01-15 10:00', NULL, 3, 'grid')",
        "CREATE TABLE $S.unread (id int)",
        "INSERT INTO $S.unread SELECT generate_series(1, 100)",
        "ANALYZE $S.unread",
        "CREATE TEMP TABLE scratch (id int)",
    ] {
        db.run(statement);
        pipeline.through_now(&mut db, &mut stream);
        assert_eq!(pipeline.layouts_read, made, "{statement}");
    }
    db.run("ALTER TYPE $S.kind ADD VALUE 'bin'");
    pipeline.through_now(&mut db, &mut stream);
    assert_eq!(pipeline.layouts_read, made + 1);
}

/// A transaction prepared before the start holds it back until it ends,
/// as it does a logical slot's creation: the WAL before the redo point
/// may hold its changes. The error names it; once it ends Straum starts.
#[test]
fn a_prepared_transaction_holds_the_start_back() {
    let pg = plain(image(&tag()).with_cmd(&[
        "-c",
        "wal_level=replica",
        "-c",
        "max_prepared_transactions=4",
    ]));
    let mut db = Db::new(&pg, "prepared", TABLES);
    let mut rng = Rng::new(3);
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5), (2, 'Bronx', NULL)");
    for id in 0..10 {
        db.run(&rng.trip(id));
    }
    db.run(&format!(
        "BEGIN; {} PREPARE TRANSACTION 'held'",
        rng.trip(10)
    ));
    db.run("CHECKPOINT");
    let xid = db.rows("SELECT transaction FROM pg_prepared_xacts WHERE gid = 'held'")[0][0]
        .clone()
        .unwrap();
    let mut source = source_over(db.dsn.clone(), &db.name, PHYSICAL);
    source.settings.consistent_wait = Duration::from_secs(2);
    let mut writer = Writer::new(App::new(db.spec(SPEC), LIMITS), source);
    let error = writer.step().unwrap_err().to_string();
    assert!(
        error.contains(&format!(
            "--wal physical waited 2 s for transactions open before it started to end: xid {xid}; \
             a long or prepared one (pg_prepared_xacts) holds the start back"
        )),
        "{error}"
    );
    db.run("COMMIT PREPARED 'held'");
    writer = db.physical_writer(SPEC);
    started(&mut writer);
    let mut next = 1_000;
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
}

/// PostgreSQL logs a commit, then waits for synchronous standbys, then
/// ends the transaction for snapshots. The stream reads such a commit as
/// before its start, so the load must see it: Straum takes the load's
/// snapshot again until it does, and says what holds it back.
#[test]
fn takes_the_snapshot_again_until_it_sees_what_ended() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "synced", TABLES);
    let mut rng = Rng::new(5);
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5)");
    for id in 0..10 {
        db.run(&rng.trip(id));
    }
    let mut held = Connection::connect(&db.dsn, ConnectionKind::Sql).unwrap();
    held.execute(&db.sql(&format!("BEGIN; {}", rng.trip(10))))
        .unwrap();
    let xid = held.rows("SELECT pg_current_xact_id()").unwrap()[0][0]
        .clone()
        .unwrap();
    ask_for_a_standby(&mut db);
    // A session reads its configuration again between commands.
    let wait = Wait::new("the setting");
    session_reads_the_standby(&mut held, &wait);
    checkpointer_reads_the_setting(&mut db);
    let committing = thread::spawn(move || held.execute("COMMIT").unwrap());
    let waiting = wait.then("the commit to wait");
    while db
        .rows("SELECT 1 FROM pg_stat_activity WHERE wait_event = 'SyncRep'")
        .is_empty()
    {
        waiting.poll();
    }
    let mut source = source_over(db.dsn.clone(), &db.name, PHYSICAL);
    source.settings.consistent_wait = Duration::from_secs(2);
    let mut writer = Writer::new(App::new(db.spec(SPEC), LIMITS), source);
    let error = writer.step().unwrap_err().to_string();
    assert!(
        error.contains(&format!(
            "--wal physical waited 2 s for transactions it read the commits of to end: \
             xid {xid}; a synchronous standby that doesn't answer holds them"
        )),
        "{error}"
    );
    // The standby gives up while Straum takes snapshots again.
    let dsn = db.dsn.clone();
    let release = thread::spawn(move || {
        thread::sleep(Duration::from_secs(1));
        let mut connection = Connection::connect(&dsn, ConnectionKind::Sql).unwrap();
        release_the_standby(&mut connection);
    });
    drop(writer);
    let mut writer = db.physical_writer(SPEC);
    started(&mut writer);
    release.join().unwrap();
    committing.join().unwrap();
    let mut next = 1_000;
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
}

/// A whole commit the stream read before the load's snapshot, which
/// that snapshot doesn't see yet since PostgreSQL waits for a synchronous
/// standby, goes out on the stream when the snapshot is taken: the load
/// doesn't have it.
#[test]
fn streams_a_commit_logged_before_the_snapshot_saw_it() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "unseen", TABLES);
    let mut rng = Rng::new(6);
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5)");
    for id in 0..10 {
        db.run(&rng.trip(id));
    }
    // Open before the start, it holds Straum back until it rolls back,
    // which waits for no standby.
    let mut holding = Connection::connect(&db.dsn, ConnectionKind::Sql).unwrap();
    holding
        .execute(&db.sql(&format!("BEGIN; {}", rng.trip(10))))
        .unwrap();
    ask_for_a_standby(&mut db);
    let (dsn, sql) = (db.dsn.clone(), db.sql(&rng.trip(11)));
    let helper = thread::spawn(move || {
        let mut watch = Connection::connect(&dsn, ConnectionKind::Sql).unwrap();
        // Straum logged which transactions run, the one holding it among
        // them.
        wait_for(
            &mut watch,
            "SELECT 1 FROM pg_stat_activity WHERE query = 'SELECT pg_log_standby_snapshot()'",
        );
        let mut committing = Connection::connect(&dsn, ConnectionKind::Sql).unwrap();
        session_reads_the_standby(&mut committing, &Wait::new("the setting"));
        let commit = thread::spawn(move || {
            committing.execute(&format!("BEGIN; {sql} COMMIT")).unwrap();
        });
        wait_for(
            &mut watch,
            "SELECT 1 FROM pg_stat_activity WHERE wait_event = 'SyncRep'",
        );
        holding.execute("ROLLBACK").unwrap();
        thread::sleep(Duration::from_secs(1));
        release_the_standby(&mut watch);
        commit.join().unwrap();
    });
    let mut writer = db.physical_writer(SPEC);
    started(&mut writer);
    helper.join().unwrap();
    let mut next = 1_000;
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
}

/// PostgreSQL logs a commit before snapshots see it, so the stream may
/// read an `ALTER TABLE`'s commit while the catalog still shows the old
/// columns. Here a synchronous standby that doesn't answer holds the DDL
/// there: Straum takes snapshots until one sees it, and the rows written
/// after it, with the added column, read.
#[test]
fn reads_the_columns_of_ddl_snapshots_dont_see_yet() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "unseen_ddl", TABLES);
    let mut rng = Rng::new(7);
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5)");
    db.run(&rng.trip(1));
    let mut writer = db.physical_writer(SPEC);
    started(&mut writer);
    let mut next = 1;
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
    let mut altering = db.another_connection();
    db.run("SET synchronous_commit = local");
    ask_for_a_standby(&mut db);
    let wait = Wait::new("the setting");
    session_reads_the_standby(&mut altering.connection, &wait);
    checkpointer_reads_the_setting(&mut db);
    let alter = thread::spawn(move || {
        altering.run("ALTER TABLE $S.trips ADD late text DEFAULT 'late'");
    });
    wait_for(
        &mut db.connection,
        "SELECT 1 FROM pg_stat_activity WHERE wait_event = 'SyncRep'",
    );
    let dsn = db.dsn.clone();
    let (applied_later, later_applied) = std::sync::mpsc::channel();
    let waiting = wait.then("Straum to wait or go on");
    let release = thread::spawn(move || {
        let mut watch = Connection::connect(&dsn, ConnectionKind::Sql).unwrap();
        // Straum read the DDL's commit and waits for a snapshot to see it,
        // or it went on past it without one.
        let snapshot_query =
            "SELECT 1 FROM pg_stat_activity WHERE query = 'SELECT pg_current_snapshot()'";
        while watch.rows(snapshot_query).unwrap().is_empty() && later_applied.try_recv().is_err() {
            waiting.poll();
        }
        release_the_standby(&mut watch);
    });
    // A commit after the DDL's, which comes out once Straum has the
    // layouts the DDL left.
    let written = db.run_committed(&format!(
        "INSERT INTO $S.logins VALUES (1, to_timestamp({START}), 1)"
    ));
    apply_through(
        &mut writer,
        written,
        &wait.then(&format!("{written} applied")),
    );
    // Gone if it released the standby when Straum waited.
    let _ = applied_later.send(());
    release.join().unwrap();
    alter.join().unwrap();
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
    reads_layouts_once_a_snapshot_sees(&mut db);
}

/// The layouts wait for a snapshot that sees the transaction asked
/// about: one still open times out, named; once it commits they read,
/// the column it added among them.
fn reads_layouts_once_a_snapshot_sees(db: &mut Db) {
    let mut open = db.another_connection();
    open.run("BEGIN; ALTER TABLE $S.trips ADD later text");
    let xid = open.rows("SELECT pg_current_xact_id()::xid")[0][0].clone();
    let xid: u32 = xid.unwrap().parse().unwrap();
    let checked = [Checked {
        name: id("trips"),
        oid: 0,
        schema: id(&db.name),
        table_name: id("trips"),
        columns: Vec::new(),
        types: Vec::new(),
        enums: Vec::new(),
    }];
    let mut connection = Connection::connect(&db.dsn, ConnectionKind::Sql).unwrap();
    let wait = Duration::from_secs(1);
    let error =
        read_layouts_seeing(&mut connection, &checked, xid, wait, &mut || Ok(())).unwrap_err();
    let expected = format!("waited 1 s for transactions it read the commits of to end: xid {xid}");
    assert!(error.to_string().contains(&expected), "{error}");
    open.run("COMMIT");
    let layouts =
        read_layouts_seeing(&mut connection, &checked, xid, wait, &mut || Ok(())).unwrap();
    let columns = &layouts.tables[0].relation.columns;
    assert_eq!(columns.last().unwrap().name, "later");
}

/// While Straum waits for a snapshot that sees DDL, the stream thread
/// reads nothing, so it answers the server there too. A wait past a
/// `wal_sender_timeout` of 2 s keeps the stream, and the commits after
/// the DDL apply. The thread answers about once a second, not at each
/// look for the snapshot. The test holds the DDL for 3 s only. A commit
/// becomes visible only after the checkpointer reads the release, and a
/// checkpoint can delay that by seconds.
#[test]
fn the_ddl_wait_outlasts_wal_sender_timeout() {
    let pg = replica_with_short_sender_timeout();
    let mut db = Db::new(&pg, "outlasts", TABLES);
    let mut rng = Rng::new(7);
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5)");
    db.run(&rng.trip(1));
    let mut writer = db.physical_writer(SPEC);
    started(&mut writer);
    let mut next = 1;
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
    let alter = hold_ddl(&mut db);
    let dsn = db.dsn.clone();
    let release = thread::spawn(move || {
        let mut watch = Connection::connect(&dsn, ConnectionKind::Sql).unwrap();
        let snapshot_query =
            "SELECT 1 FROM pg_stat_activity WHERE query = 'SELECT pg_current_snapshot()'";
        let wait = Wait::new("Straum to wait for a snapshot");
        while watch.rows(snapshot_query).unwrap().is_empty() {
            wait.poll();
        }
        // Each status update has its own send time, so the distinct reply
        // times count the updates that the walsender read.
        let mut replies = BTreeSet::new();
        let window = Instant::now();
        while window.elapsed() < Duration::from_secs(3) {
            replies.extend(
                watch
                    .rows("SELECT reply_time::text FROM pg_stat_replication")
                    .unwrap(),
            );
            thread::sleep(Duration::from_millis(20));
        }
        let senders = watch.rows("SELECT count(*)::text FROM pg_stat_replication");
        release_the_standby(&mut watch);
        (senders.unwrap(), replies.len())
    });
    let written = db.run_committed(&format!(
        "INSERT INTO $S.logins VALUES (1, to_timestamp({START}), 1)"
    ));
    let wait = Wait::within(Duration::from_secs(30), &format!("{written} applied"));
    apply_through(&mut writer, written, &wait);
    let (senders, replies) = release.join().unwrap();
    assert_eq!(senders, vec![vec![Some("1".to_owned())]]);
    // About one update each second of the 3 s: some, but not a tight loop.
    assert!(
        (2..=5).contains(&replies),
        "{replies} status updates in 3 s"
    );
    alter.join().unwrap();
}

/// The replies keep the stream, not the wait: past 10 s without a
/// snapshot that sees the DDL, the stream still stops and names it.
#[test]
fn the_ddl_wait_still_ends_after_ten_seconds() {
    let pg = replica_with_short_sender_timeout();
    let mut db = Db::new(&pg, "still_ends", TABLES);
    let spec = db.spec(SPEC);
    let mut source = source_over(db.dsn.clone(), &db.name, PHYSICAL);
    source.open(&spec).unwrap();
    load_all(&mut source, &spec).unwrap();
    let alter = hold_ddl(&mut db);
    // The source, not a writer: a writer also reads the WAL position, and
    // that reading waits for the lock that the ALTER TABLE holds.
    let wait = Wait::within(Duration::from_secs(30), "the wait after DDL to end");
    let mut next = source.next_commit(Duration::from_secs(1));
    while let Ok(None) = next {
        wait.check();
        next = source.next_commit(Duration::from_secs(1));
    }
    let error = next.unwrap_err().to_string();
    let expected = "waited 10 s for transactions it read the commits of to end";
    assert!(error.contains(expected), "{error}");
    release_the_standby(&mut db.connection);
    alter.join().unwrap();
}

/// PostgreSQL at `wal_level = replica` that ends a stream with no reply
/// for 2 s.
fn replica_with_short_sender_timeout() -> PostgresContainer {
    plain(image(&tag()).with_cmd(&["-c", "wal_level=replica", "-c", "wal_sender_timeout=2s"]))
}

/// Commits an `ALTER TABLE` that a synchronous standby that doesn't
/// answer holds where snapshots don't see it. The thread of the `ALTER
/// TABLE` ends once the standby is released.
fn hold_ddl(db: &mut Db) -> JoinHandle<()> {
    let mut altering = db.another_connection();
    db.run("SET synchronous_commit = local");
    ask_for_a_standby(db);
    session_reads_the_standby(&mut altering.connection, &Wait::new("the setting"));
    checkpointer_reads_the_setting(db);
    let alter = thread::spawn(move || {
        altering.run("ALTER TABLE $S.trips ADD late text DEFAULT 'late'");
    });
    wait_for(
        &mut db.connection,
        "SELECT 1 FROM pg_stat_activity WHERE wait_event = 'SyncRep'",
    );
    alter
}

/// Makes every commit wait for a synchronous standby named `nobody`,
/// which never answers.
fn ask_for_a_standby(db: &mut Db) {
    db.run("ALTER SYSTEM SET synchronous_standby_names = 'nobody'");
    db.run("SELECT pg_reload_conf()");
}

/// Returns once `session` shows the standby [`ask_for_a_standby`] asked
/// for, failing when `wait` ends first.
fn session_reads_the_standby(session: &mut Connection, wait: &Wait) {
    while session.rows("SHOW synchronous_standby_names").unwrap()[0][0].as_deref() != Some("nobody")
    {
        wait.poll();
    }
}

/// Lets commits go on without a standby again.
fn release_the_standby(connection: &mut Connection) {
    connection
        .execute("ALTER SYSTEM RESET synchronous_standby_names")
        .unwrap();
    connection.execute("SELECT pg_reload_conf()").unwrap();
}

/// Returns once the checkpointer has read the configuration reloaded
/// before: only then does a commit wait for a synchronous standby
/// (`SyncRepWaitForLSN` in `syncrep.c`), whatever a session's `SHOW`
/// says. A checkpoint requested after the reload starts only once the
/// checkpointer has read it; the second rules out joining one that was
/// already starting.
fn checkpointer_reads_the_setting(db: &mut Db) {
    db.run("CHECKPOINT");
    db.run("CHECKPOINT");
}

/// A checkpoint in progress sets the redo point, where a physical slot
/// starts, before PostgreSQL has flushed the WAL up to it; the server
/// streams nothing it hasn't flushed, so the start waits for the flush.
#[test]
fn starts_while_a_checkpoint_runs() {
    let pg = postgres_at_replica(&tag());
    let db = Db::new(&pg, "flushing", "CREATE TABLE $S.t (id int, body text);");
    let stop = Arc::new(AtomicBool::new(false));
    let mut busy = Vec::new();
    for sql in [
        "CHECKPOINT",
        "INSERT INTO $S.t SELECT i, repeat('x', 200) FROM generate_series(1, 50) AS i",
    ] {
        busy.push(run_without_waiting_for_flush(
            &stop,
            db.another_connection(),
            sql,
        ));
    }
    for _ in 0..100 {
        let (mut physical, start) = Physical::start(&pg.dsn).unwrap();
        // A read may end inside a message and give no record yet.
        let wait = Wait::new(&format!("a record past {start}"));
        let record = loop {
            if let Some(record) = physical.next_record().unwrap() {
                break record;
            }
            wait.check();
        };
        assert!(record.lsn >= start.0);
    }
    stop.store(true, Ordering::Relaxed);
    for thread in busy {
        thread.join().unwrap();
    }
}

/// A thread running `sql` over and over until `stop`, with commits that
/// don't wait for their flush, so they leave WAL unflushed.
fn run_without_waiting_for_flush(
    stop: &Arc<AtomicBool>,
    mut db: Db,
    sql: &str,
) -> thread::JoinHandle<()> {
    let (stop, sql) = (Arc::clone(stop), sql.to_owned());
    thread::spawn(move || {
        db.run("SET synchronous_commit = off");
        while !stop.load(Ordering::Relaxed) {
            db.run(&sql);
        }
    })
}

/// A walsender that dies takes the physical stream with it and stops the
/// writer; a new one, as Straum started again, follows on from a new slot.
#[test]
fn stops_when_the_physical_stream_dies() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "dies", TABLES);
    let mut rng = Rng::new(4);
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5)");
    for id in 0..10 {
        db.run(&rng.trip(id));
    }
    let mut writer = db.physical_writer(SPEC);
    started(&mut writer);
    let mut next = 1_000;
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
    let killed = db.rows("SELECT count(pg_terminate_backend(pid)) FROM pg_stat_replication");
    assert_eq!(killed[0][0].as_deref(), Some("1"));
    db.run(&rng.trip(500));
    let wait = Wait::new("the writer to notice");
    loop {
        wait.check();
        if writer.step().is_err() {
            break;
        }
    }
    assert!(!writer.app().status().ready);
    writer = db.physical_writer(SPEC);
    started(&mut writer);
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
}

/// With nothing logging which transactions run, and a role that may not
/// make PostgreSQL log it, a physical start says how to fix that.
#[test]
fn says_what_a_physical_start_waits_for() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "waits", TABLES);
    db.run(
        "CREATE ROLE reader LOGIN REPLICATION PASSWORD 'r'; GRANT USAGE ON SCHEMA $S TO reader;
         GRANT SELECT ON ALL TABLES IN SCHEMA $S TO reader",
    );
    let spec = db.spec(SPEC);
    // The server just started: its last checkpoint is the shutdown one,
    // which logs no RUNNING_XACTS, and the background writer logs the
    // first 15 seconds in. An online checkpoint would log one.
    let mut source = source_over(dsn_as(&pg.dsn, "reader", "r"), "waits", PHYSICAL);
    source.settings.consistent_wait = Duration::from_secs(2);
    let error = source.open(&spec).unwrap_err().to_string();
    assert!(
        error.contains(
            "--wal physical waited 2 s for PostgreSQL to log which transactions run; \
             GRANT EXECUTE ON FUNCTION pg_log_standby_snapshot() TO Straum's role"
        ),
        "{error}"
    );
    // Granted, it starts at once.
    db.run("GRANT EXECUTE ON FUNCTION pg_log_standby_snapshot() TO reader");
    let started = Instant::now();
    source.open(&spec).unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

/// The physical stream decodes a transaction as the logical stream does,
/// under the same limit in bytes.
#[test]
fn a_transaction_past_the_stream_memory_stops_the_physical_stream() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "wide_physical", TABLES);
    let mut writer = stream_capped_writer(&db, PHYSICAL);
    a_wide_transaction_stops(&mut db, &mut writer);
}
