//! The watchdog: whatever the writer does, Straum's slot never holds more
//! WAL than `--max-retained-wal` for longer than a watchdog round, and a
//! watchdog that can't read the slot ends the stream. The tests against
//! PostgreSQL run as a role with only `REPLICATION` and `SELECT`, as on a
//! managed server, so ending its own session must work without a
//! superuser. The clock's test runs against fake servers that never
//! answer.

use std::fmt::Write as _;
use std::thread;
use std::time::{Duration, Instant};

use straum_app::Source;
use straum_domain::{Spec, Store};

use super::references::*;
use super::support::*;
use crate::dsn::Dsn;
use crate::error::Error;
use crate::source::{Postgres, Replication};
use crate::watchdog::{UNWATCHED, Verdict, Watchdog, Watching, past_limit};
use crate::wire::{Connection, ConnectionKind, HANDSHAKE_TIMEOUT};

/// A `--max-retained-wal` the filler passes: 8 MiB.
const SMALL_LIMIT_BYTES: u64 = 8 << 20;
/// About 20 MiB of WAL, written by the superuser beside Straum.
const FILLER: &str = "CREATE TABLE $S.filler AS
    SELECT repeat('x', 1000) AS pad FROM generate_series(1, 20000)";
/// Commits more than the stream's queue holds, so its thread waits on a
/// writer that applies nothing.
const QUEUE_FILLING_COMMITS: u64 = 100;
/// Watchdog rounds a slot within its limit is left to run.
const ROUNDS: Duration = Duration::from_secs(3);
/// How long the watchdog may go unread in the tests that cut it off.
const SHORT_UNWATCHED: Duration = Duration::from_secs(2);
/// How long the fakes in the clock's test stay silent: longer than any
/// timeout of Straum's.
const SILENCE: Duration = Duration::from_secs(60);

/// The limit itself is within it; one byte more is past.
#[test]
fn a_slot_is_past_the_limit_only_above_it() {
    let limit = SMALL_LIMIT_BYTES;
    assert_eq!(past_limit(0, limit), None);
    assert_eq!(past_limit(limit, limit), None);
    assert_eq!(
        past_limit(limit + 1, limit),
        Some(Verdict::Retained {
            retained: limit + 1,
            limit
        })
    );
}

/// A reader stuck in a start that never ends still leaves the stream a
/// bound: the clock ends it after `unwatched`, long before the handshake
/// gives up, and shuts the stream's sockets.
#[test]
fn the_clock_ends_the_stream_while_the_reader_hangs() {
    let stalled = scripted(vec![later(SILENCE, Vec::new())]);
    let stream_server = scripted(vec![now(signed_in()), later(SILENCE, Vec::new())]);
    let mut stream = Connection::connect(&stream_server.dsn, ConnectionKind::Sql).unwrap();
    stream.set_read_timeout(SHORT_UNWATCHED).unwrap();
    let watching = Watching {
        max_retained_wal: SMALL_LIMIT_BYTES,
        unwatched: SHORT_UNWATCHED,
    };
    let clock = Instant::now();
    let watchdog = Watchdog::start(
        &stalled.dsn,
        "straum_stalled",
        watching,
        vec![stream.closer().unwrap()],
    )
    .unwrap();
    let verdict = loop {
        if let Some(verdict) = watchdog.verdict() {
            break verdict;
        }
        assert!(
            clock.elapsed() < HANDSHAKE_TIMEOUT,
            "the clock waited on the reader"
        );
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        verdict,
        Verdict::Unwatched {
            unwatched: SHORT_UNWATCHED
        }
    );
    // A shut socket fails at once rather than at its read timeout.
    let clock = Instant::now();
    assert!(stream.execute("SELECT 1").is_err());
    assert!(
        clock.elapsed() < SHORT_UNWATCHED,
        "the stream's socket stayed open"
    );
}

/// A session that keeps its socket open, whatever its client does, still
/// loses the slot: PostgreSQL ends it at the watchdog's word.
#[test]
fn ends_the_session_that_owns_the_slot() {
    let pg = postgres();
    let mut db = Db::new(&pg, "owner", TABLES);
    let dsn = reader(&mut db, &pg);
    let mut owner = Connection::connect(&dsn, ConnectionKind::Replication).unwrap();
    owner
        .rows("CREATE_REPLICATION_SLOT straum_owned TEMPORARY LOGICAL pgoutput")
        .unwrap();
    let watching = Watching {
        max_retained_wal: SMALL_LIMIT_BYTES,
        unwatched: UNWATCHED,
    };
    let watchdog = Watchdog::start(&dsn, "straum_owned", watching, Vec::new()).unwrap();
    db.run(FILLER);
    assert_slot_goes(&mut db, "the slot outlived the limit");
    assert!(matches!(
        watchdog.verdict(),
        Some(Verdict::Retained {
            limit: SMALL_LIMIT_BYTES,
            ..
        })
    ));
    assert!(
        owner.execute("SELECT 1").is_err(),
        "the owner's session lived"
    );
}

/// Within the limit the watchdog leaves the owner's session alone.
#[test]
fn leaves_the_session_that_owns_a_slot_within_the_limit() {
    let pg = postgres();
    let mut db = Db::new(&pg, "ownerkept", TABLES);
    let dsn = reader(&mut db, &pg);
    let mut owner = Connection::connect(&dsn, ConnectionKind::Replication).unwrap();
    owner
        .rows("CREATE_REPLICATION_SLOT straum_kept TEMPORARY LOGICAL pgoutput")
        .unwrap();
    let watching = Watching {
        max_retained_wal: MAX_RETAINED_WAL_BYTES,
        unwatched: UNWATCHED,
    };
    let watchdog = Watchdog::start(&dsn, "straum_kept", watching, Vec::new()).unwrap();
    db.run(FILLER);
    thread::sleep(ROUNDS);
    assert_eq!(watchdog.verdict(), None);
    assert_eq!(straum_slots(&mut db), 1, "the slot ended within the limit");
    owner.execute("SELECT 1").unwrap();
}

/// A load that keeps the logical slot's connection busy, so nothing reads
/// the slot: past the limit the watchdog ends it.
#[test]
fn ends_a_logical_slot_a_load_holds_past_the_limit() {
    let pg = postgres();
    load_holds(&pg, "logicalheld", Replication::Logical, SMALL_LIMIT_BYTES);
}

/// The same while the physical stream's tables load.
#[test]
fn ends_a_physical_slot_a_load_holds_past_the_limit() {
    let pg = postgres_at_replica(&tag());
    load_holds(&pg, "physicalheld", PHYSICAL, SMALL_LIMIT_BYTES);
}

/// Within the limit the slot stays and the load goes on.
#[test]
fn keeps_a_slot_a_load_holds_within_the_limit() {
    let pg = postgres_at_replica(&tag());
    load_holds(&pg, "withinlimit", PHYSICAL, MAX_RETAINED_WAL_BYTES);
}

/// A writer that stops applying leaves the stream's thread answering the
/// server with the same position, which keeps `wal_sender_timeout` from
/// ever ending the session: only the watchdog ends the slot.
#[test]
fn ends_a_logical_slot_a_stuck_writer_holds() {
    let pg = postgres();
    writer_stuck(&pg, "logicalstuck", Replication::Logical);
}

/// The same on the physical stream.
#[test]
fn ends_a_physical_slot_a_stuck_writer_holds() {
    let pg = postgres_at_replica(&tag());
    writer_stuck(&pg, "physicalstuck", PHYSICAL);
}

/// A watchdog that can't connect again ends the stream rather than leave
/// the slot unwatched.
#[test]
fn ends_the_stream_when_the_watchdog_cannot_read_the_slot() {
    let pg = postgres_at_replica(&tag());
    watchdog_cut_off(&pg, "unwatched", Reconnect::Refused);
}

/// A watchdog whose session ends connects again, and the stream goes on.
#[test]
fn keeps_the_stream_when_the_watchdog_connects_again() {
    let pg = postgres_at_replica(&tag());
    watchdog_cut_off(&pg, "rewatched", Reconnect::Allowed);
}

/// Opens a source with `limit`, writes the filler before any table loads,
/// and checks the slot ends only past the limit.
fn load_holds(pg: &PostgresContainer, name: &str, replication: Replication, limit: u64) {
    let mut db = Db::new(pg, name, TABLES);
    let mut source = Postgres::new(reader(&mut db, pg), name, ROW_LIMIT, limit, replication);
    let spec = db.spec(SPEC);
    source.open(&spec).unwrap();
    assert_eq!(straum_slots(&mut db), 1, "{name}: no slot");
    db.run(FILLER);
    if limit < MAX_RETAINED_WAL_BYTES {
        assert_slot_goes(&mut db, &format!("{name}: the slot outlived the limit"));
        let error = load_everything(&mut source, &spec).unwrap_err();
        assert!(error.contains("--max-retained-wal"), "{name}: {error}");
    } else {
        thread::sleep(ROUNDS);
        assert_eq!(
            straum_slots(&mut db),
            1,
            "{name}: the slot ended within the limit"
        );
        load_everything(&mut source, &spec).unwrap();
    }
}

/// Loads, fills the stream's queue with commits nobody applies, writes
/// the filler, and checks the slot ends and the writer then hears why.
fn writer_stuck(pg: &PostgresContainer, name: &str, replication: Replication) {
    let mut db = Db::new(pg, name, TABLES);
    let mut source = Postgres::new(
        reader(&mut db, pg),
        name,
        ROW_LIMIT,
        SMALL_LIMIT_BYTES,
        replication,
    );
    let spec = db.spec(SPEC);
    source.open(&spec).unwrap();
    load_everything(&mut source, &spec).unwrap();
    // The first call starts the stream; nothing applies after it.
    source.next_commit(Duration::ZERO).unwrap();
    for id in 1..=QUEUE_FILLING_COMMITS {
        db.run(&format!(
            "INSERT INTO $S.trips (id, at) VALUES ({id}, '2024-01-15')"
        ));
    }
    db.run(FILLER);
    if let Err(timeline) = slot_timeline(&mut db) {
        // The verdict the watchdog recorded, if any, or the stream's error.
        let watchdog = source.guard.blame(Error::Stopped);
        panic!("{name}: the slot outlived the limit; watchdog: {watchdog}\n{timeline}");
    }
    // The writer wakes, takes what was queued, then the reason.
    let deadline = Instant::now() + LONG;
    let error = loop {
        assert!(Instant::now() < deadline, "{name}: the stream never ended");
        match source.next_commit(Duration::from_millis(100)) {
            Ok(_) => {}
            Err(error) => break error.to_string(),
        }
    };
    assert!(error.contains("--max-retained-wal"), "{name}: {error}");
}

/// Whether the watchdog may connect again after its session ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reconnect {
    Allowed,
    Refused,
}

/// Ends the watchdog's session, refusing the role new connections or
/// not, and checks the stream ends only when the watchdog can't come back.
fn watchdog_cut_off(pg: &PostgresContainer, name: &str, reconnect: Reconnect) {
    let mut db = Db::new(pg, name, TABLES);
    let mut source = Postgres::new(
        reader(&mut db, pg),
        name,
        ROW_LIMIT,
        MAX_RETAINED_WAL_BYTES,
        PHYSICAL,
    );
    source.guard.watching.unwatched = SHORT_UNWATCHED;
    let spec = db.spec(SPEC);
    source.open(&spec).unwrap();
    if reconnect == Reconnect::Refused {
        db.run("ALTER ROLE reader CONNECTION LIMIT 0");
    }
    end_watchdog_session(&mut db);
    if reconnect == Reconnect::Refused {
        assert_slot_goes(&mut db, &format!("{name}: the slot outlived its watchdog"));
        let error = load_everything(&mut source, &spec).unwrap_err();
        assert!(
            error.contains("couldn't read Straum's slot"),
            "{name}: {error}"
        );
    } else {
        thread::sleep(SHORT_UNWATCHED + ROUNDS);
        assert_eq!(straum_slots(&mut db), 1, "{name}: the stream ended");
        load_everything(&mut source, &spec).unwrap();
    }
}

/// Ends the session the watchdog reads the slot on, once it has read it.
fn end_watchdog_session(db: &mut Db) {
    let deadline = Instant::now() + LONG;
    loop {
        assert!(
            Instant::now() < deadline,
            "the watchdog never read the slot"
        );
        let ended = db.rows(
            "SELECT count(pg_terminate_backend(pid)) FROM pg_stat_activity
             WHERE usename = 'reader' AND query LIKE '%pg_replication_slots%'
               AND query NOT LIKE '%pg_terminate_backend%'",
        );
        if ended[0][0].as_deref() != Some("0") {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// A DSN for a role with only what `straum plan` grants, made after a
/// checkpoint so a new slot starts out holding little WAL.
fn reader(db: &mut Db, pg: &PostgresContainer) -> Dsn {
    db.run(
        "CREATE ROLE reader LOGIN REPLICATION PASSWORD 'r'; GRANT USAGE ON SCHEMA $S TO reader;
         GRANT SELECT ON ALL TABLES IN SCHEMA $S TO reader;
         GRANT EXECUTE ON FUNCTION pg_log_standby_snapshot() TO reader;
         CHECKPOINT",
    );
    Dsn::parse(&pg.dsn.to_string().replace("postgres@", "reader:r@")).unwrap()
}

/// Every dim, then every fact, as the writer loads them.
fn load_everything(source: &mut Postgres, spec: &Spec) -> Result<(), String> {
    let mut store = Store::new(spec.clone());
    let mut tables = Vec::new();
    for dim in spec.dims() {
        tables.push(&dim.name);
    }
    for fact in spec.facts() {
        tables.push(&fact.name);
    }
    for table in tables {
        if let Err(error) = source.load(table, &mut store) {
            return Err(error.to_string());
        }
    }
    Ok(())
}

/// Straum's slots on the server.
fn straum_slots(db: &mut Db) -> usize {
    let rows = db.rows("SELECT count(*) FROM pg_replication_slots WHERE slot_name LIKE 'straum%'");
    rows[0][0].as_deref().unwrap().parse().unwrap()
}

/// Each round of [`assert_slot_goes`]: Straum's slots, what WAL they
/// hold and what their sessions wait on.
const SLOT_ROUND: &str = "SELECT s.slot_name, s.active_pid, s.restart_lsn,
        pg_wal_lsn_diff(pg_current_wal_lsn(), s.restart_lsn)::bigint,
        a.state, a.wait_event_type, a.wait_event
    FROM pg_replication_slots s LEFT JOIN pg_stat_activity a ON a.pid = s.active_pid
    WHERE s.slot_name LIKE 'straum%'";

/// Every [`SESSION_EVERY`] rounds: the sessions of Straum's role, the
/// watchdog's among them, and how long ago each began its last query.
const SESSIONS: &str = "SELECT pid, backend_type, state, wait_event,
        (extract(epoch FROM clock_timestamp() - query_start) * 1000)::bigint,
        left(query, 60)
    FROM pg_stat_activity WHERE usename = 'reader' ORDER BY pid";

/// Rounds between two lists of Straum's sessions: about a second.
const SESSION_EVERY: u32 = 10;

/// Waits for Straum's slot to go. Still there after [`LONG`], it fails
/// with `message` and what each round saw, so a failure on a slow runner
/// says where the slot stuck.
fn assert_slot_goes(db: &mut Db, message: &str) {
    if let Err(timeline) = slot_timeline(db) {
        panic!("{message}\n{timeline}");
    }
}

/// Waits for Straum's slot to go: what each round saw if it is still
/// there after [`LONG`].
fn slot_timeline(db: &mut Db) -> Result<(), String> {
    let start = Instant::now();
    let mut timeline = "ms since the wait began  slot  pid  restart_lsn  held  state  wait\n\
        then, each second, Straum's sessions: pid  type  state  wait  ms since its query  query\n"
        .to_owned();
    let mut round = 0;
    while start.elapsed() < LONG {
        let rows = db.rows(SLOT_ROUND);
        if rows.is_empty() {
            return Ok(());
        }
        write_rows(
            &mut timeline,
            &format!("{:>6} ms", start.elapsed().as_millis()),
            &rows,
        );
        if round % SESSION_EVERY == 0 {
            write_rows(&mut timeline, "        ", &db.rows(SESSIONS));
        }
        round += 1;
        thread::sleep(Duration::from_millis(100));
    }
    Err(timeline)
}

/// Writes each of `rows` as a line after `prefix`.
fn write_rows(timeline: &mut String, prefix: &str, rows: &[Vec<Option<String>>]) {
    for row in rows {
        timeline.push_str(prefix);
        for field in row {
            let _ = write!(timeline, "  {}", field.as_deref().unwrap_or("-"));
        }
        timeline.push('\n');
    }
}
