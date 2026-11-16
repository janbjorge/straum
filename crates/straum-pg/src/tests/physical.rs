//! The physical stream end to end (`--wal physical`): Straum reads the
//! physical WAL (the write-ahead log PostgreSQL writes every page change
//! to) at `wal_level = replica`, starts from a snapshot, and follows on.
//! Also what holds a start back and what the setup check asks of it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use straum_app::{App, Lsn, Source, Step, Writer};
use straum_domain::Spec;
use testcontainers::ImageExt;

use super::decoding::Pipeline;
use super::references::*;
use super::support::*;
use crate::catalog::{self};
use crate::dsn::Dsn;
use crate::physical::Physical;
use crate::source::{Postgres, Replication};
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
/// Straum starts over rather than grow.
#[test]
fn stops_past_the_page_memory() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "capped", TABLES);
    let spec = db.spec(SPEC);
    let small = Replication::Physical {
        max_page_memory: SMALL_PAGE_MEMORY_BYTES,
    };
    let mut source = Postgres::new(db.dsn.clone(), &db.name, ROW_LIMIT, small);
    source.open(&spec).unwrap();
    db.run(
        "INSERT INTO $S.trips SELECT i, now(), 'cab', 1, 1.5, md5(i::text), 1
         FROM generate_series(1, 5000) AS i",
    );
    let deadline = Instant::now() + LONG;
    let error = loop {
        assert!(Instant::now() < deadline, "never stopped");
        match source.next_commit(Duration::from_millis(100)) {
            Ok(_) => {}
            Err(error) => break error.to_string(),
        }
    };
    assert!(
        error.starts_with("the physical stream holds ")
            && error.ends_with(
                " bytes of rows, past the --max-page-memory limit of 65536; Straum starts over"
            ),
        "{error}"
    );
}

/// `straum run --wal physical` at `wal_level = replica`: it loads while
/// writers insert, update and delete, starts from a snapshot every later
/// commit streams after, follows, and every view equals SQL's.
#[test]
fn follows_the_physical_wal() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "physical_run", TABLES);
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
        writers.push(write_while_starting(&stop, &db, index));
    }
    thread::sleep(Duration::from_millis(200));
    let mut writer = db.physical_writer(SPEC);
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    stop.store(true, Ordering::Relaxed);
    for writer in writers {
        writer.join().unwrap();
    }
    let mut next = 5_000_000;
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
    db.run("UPDATE $S.trips SET kind = 'van', tip = tip + 1 WHERE id % 3 = 0");
    db.run("DELETE FROM $S.trips WHERE id % 4 = 0");
    db.run("UPDATE $S.zones SET borough = 'Brooklyn' WHERE id = 2");
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
}

/// A writer thread that until `stop` inserts a trip, updates and deletes
/// earlier ones and changes zone `index + 1`, a commit each; writer 0 also
/// adds and drops a column now and then.
fn write_while_starting(stop: &Arc<AtomicBool>, db: &Db, index: u64) -> thread::JoinHandle<()> {
    let stop = Arc::clone(stop);
    let dsn = db.dsn.clone();
    let schema = db.name.clone();
    thread::spawn(move || {
        let mut connection = Connection::connect(&dsn, ConnectionKind::Sql).unwrap();
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
            connection.execute(&sql.replace("$S", &schema)).unwrap();
            // DDL while Straum starts: a column comes and goes.
            if index == 0 && id.is_multiple_of(25) {
                let ddl = format!(
                    "ALTER TABLE $S.trips ADD c{id} int DEFAULT 7;
                     UPDATE $S.trips SET c{id} = 8 WHERE id = {id};
                     ALTER TABLE $S.trips DROP c{id}"
                );
                connection.execute(&ddl.replace("$S", &schema)).unwrap();
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
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    let mut next = 1;
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
    db.run(
        "CREATE TYPE $S.mood AS ENUM ('calm', 'rushed');
         ALTER TABLE $S.trips ADD mood $S.mood",
    );
    db.run("UPDATE $S.trips SET mood = 'rushed', tip = tip + 1");
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
}

/// A transaction prepared before the start holds it back until it ends,
/// as it does a logical slot's creation: the WAL before the redo point
/// may hold its changes. The error names it; once it ends Straum starts.
#[test]
fn a_prepared_transaction_holds_the_start_back() {
    let pg = plain(image(&tag()).with_cmd([
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
    let mut source = Postgres::new(db.dsn.clone(), &db.name, ROW_LIMIT, PHYSICAL);
    source.consistent_wait = Duration::from_secs(2);
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
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
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
    db.run("ALTER SYSTEM SET synchronous_standby_names = 'nobody'");
    db.run("SELECT pg_reload_conf()");
    // A session reads its configuration again between commands.
    let deadline = Instant::now() + LONG;
    while held.rows("SHOW synchronous_standby_names").unwrap()[0][0].as_deref() != Some("nobody") {
        assert!(Instant::now() < deadline, "the setting never came");
        thread::sleep(Duration::from_millis(20));
    }
    let committing = thread::spawn(move || held.execute("COMMIT").unwrap());
    while db
        .rows("SELECT 1 FROM pg_stat_activity WHERE wait_event = 'SyncRep'")
        .is_empty()
    {
        assert!(Instant::now() < deadline, "the commit never waited");
        thread::sleep(Duration::from_millis(20));
    }
    let mut source = Postgres::new(db.dsn.clone(), &db.name, ROW_LIMIT, PHYSICAL);
    source.consistent_wait = Duration::from_secs(2);
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
        connection
            .execute("ALTER SYSTEM RESET synchronous_standby_names")
            .unwrap();
        connection.execute("SELECT pg_reload_conf()").unwrap();
    });
    drop(writer);
    let mut writer = db.physical_writer(SPEC);
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
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
    db.run("ALTER SYSTEM SET synchronous_standby_names = 'nobody'");
    db.run("SELECT pg_reload_conf()");
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
        let deadline = Instant::now() + LONG;
        while committing.rows("SHOW synchronous_standby_names").unwrap()[0][0].as_deref()
            != Some("nobody")
        {
            assert!(Instant::now() < deadline, "the setting never came");
            thread::sleep(Duration::from_millis(20));
        }
        let commit = thread::spawn(move || {
            committing.execute(&format!("BEGIN; {sql} COMMIT")).unwrap();
        });
        wait_for(
            &mut watch,
            "SELECT 1 FROM pg_stat_activity WHERE wait_event = 'SyncRep'",
        );
        holding.execute("ROLLBACK").unwrap();
        thread::sleep(Duration::from_secs(1));
        watch
            .execute("ALTER SYSTEM RESET synchronous_standby_names")
            .unwrap();
        watch.execute("SELECT pg_reload_conf()").unwrap();
        commit.join().unwrap();
    });
    let mut writer = db.physical_writer(SPEC);
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    helper.join().unwrap();
    let mut next = 1_000;
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
}

/// Waits until `query` returns a row.
fn wait_for(watch: &mut Connection, query: &str) {
    let deadline = Instant::now() + LONG;
    while watch.rows(query).unwrap().is_empty() {
        assert!(Instant::now() < deadline, "never: {query}");
        thread::sleep(Duration::from_millis(20));
    }
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
        busy.push(run_without_waiting_for_flush(&stop, &db, sql));
    }
    for _ in 0..100 {
        let (mut physical, start) = Physical::start(&pg.dsn).unwrap();
        if let Some(record) = physical.next_record().unwrap() {
            assert!(record.lsn >= start.0);
        }
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
    db: &Db,
    sql: &str,
) -> thread::JoinHandle<()> {
    let (stop, dsn, sql) = (Arc::clone(stop), db.dsn.clone(), db.sql(sql));
    thread::spawn(move || {
        let mut connection = Connection::connect(&dsn, ConnectionKind::Sql).unwrap();
        connection.execute("SET synchronous_commit = off").unwrap();
        while !stop.load(Ordering::Relaxed) {
            connection.execute(&sql).unwrap();
        }
    })
}

/// A walsender that dies takes the physical stream with it; Straum
/// starts over from a new slot and follows on.
#[test]
fn starts_over_when_the_physical_stream_dies() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "dies", TABLES);
    let mut rng = Rng::new(4);
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5)");
    for id in 0..10 {
        db.run(&rng.trip(id));
    }
    let mut writer = db.physical_writer(SPEC);
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    let mut next = 1_000;
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
    let killed = db.rows("SELECT count(pg_terminate_backend(pid)) FROM pg_stat_replication");
    assert_eq!(killed[0][0].as_deref(), Some("1"));
    db.run(&rng.trip(500));
    let deadline = Instant::now() + LONG;
    loop {
        assert!(Instant::now() < deadline, "never noticed");
        if writer.step().is_err() {
            break;
        }
    }
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
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
    let spec = db.spec(SPEC);
    let mut catalog = catalog::query(&mut db.connection, &spec, "needs").unwrap();
    assert_eq!(
        (catalog.full_page_writes.as_str(), catalog.publication),
        ("on", None)
    );
    assert_eq!(physical_problems(&spec, &catalog), None);
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
            physical_problems(&spec, &catalog),
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

/// Servers `--wal physical` takes, then two it doesn't.
fn servers_and_their_problems() -> [Server; 5] {
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

/// What the check for `--wal physical` finds wrong with `catalog`.
fn physical_problems(spec: &Spec, catalog: &catalog::Catalog) -> Option<Vec<String>> {
    catalog::check(spec, catalog, "needs", PHYSICAL)
        .err()
        .as_deref()
        .map(crate::problem::lines)
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
    let dsn = pg.dsn.to_string().replace("postgres@", "reader:r@");
    let mut source = Postgres::new(Dsn::parse(&dsn).unwrap(), "waits", ROW_LIMIT, PHYSICAL);
    source.consistent_wait = Duration::from_secs(2);
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

/// Applies commits until Straum is ready and its views equal SQL's. A
/// trip goes in each round, so there is a commit to wait for.
pub(super) fn catch_up(db: &mut Db, writer: &mut Writer<Postgres>, rng: &mut Rng, next: &mut u64) {
    let deadline = Instant::now() + LONG;
    loop {
        assert!(Instant::now() < deadline, "never caught up");
        *next += 1;
        db.run(&rng.trip(*next));
        let written = db.insert_lsn();
        while !applied_through(writer, written) {
            assert!(Instant::now() < deadline, "never applied {written}");
            applied(writer);
        }
        if compare(db, writer.app(), &taxi_oracles(2)).is_some() {
            return;
        }
    }
}

/// Whether `writer` has applied every commit up to `written`.
fn applied_through(writer: &Writer<Postgres>, written: Lsn) -> bool {
    match writer.app().status().applied {
        Some(applied) => applied >= written,
        None => false,
    }
}
