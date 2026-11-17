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
/// About 20 MiB more WAL into the filler's table.
const FILLER_MORE: &str = "INSERT INTO $S.filler
    SELECT repeat('x', 1000) FROM generate_series(1, 20000)";
/// How often [`stream_reading_nothing`] looks at the walsender.
const STUCK_SAMPLE: Duration = Duration::from_millis(500);
/// Samples in a row a walsender must sit still to count as blocked.
const STUCK_SAMPLES: u32 = 3;
/// `net.ipv4.tcp_wmem` in the server's container: send buffers of at
/// most 16 KiB, far below one message of WAL, so a walsender whose client
/// reads nothing has no room for its FATAL either.
const SMALL_SEND_BUFFERS: &str = "4096 16384 16384";
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

/// The PostgreSQL quirk the watchdog's repeated asks answer: a walsender
/// whose client reads nothing takes one `pg_terminate_backend`, then
/// blocks for good sending the client its FATAL, and keeps the slot.
#[test]
fn one_end_leaves_a_walsender_whose_client_reads_nothing() {
    let pg = postgres_with_small_send_buffers();
    let mut db = Db::new(&pg, "endedonce", TABLES);
    let dsn = reader(&mut db, &pg);
    let (_stream, walsender) = stream_reading_nothing(&mut db, &dsn, "straum_endedonce");
    db.run(&format!("SELECT pg_terminate_backend({walsender})"));
    thread::sleep(ROUNDS);
    let waits = db.rows(&format!(
        "SELECT wait_event FROM pg_stat_activity WHERE pid = {walsender}"
    ));
    assert_eq!(
        waits,
        vec![vec![Some("ClientWrite".to_owned())]],
        "the walsender ended at the first ask"
    );
}

/// The watchdog asks each round until the slot is gone, so even that
/// walsender ends, with no socket of its stream to shut.
#[test]
fn ends_a_walsender_whose_client_reads_nothing() {
    let pg = postgres_with_small_send_buffers();
    let mut db = Db::new(&pg, "endedagain", TABLES);
    let dsn = reader(&mut db, &pg);
    let (_stream, _) = stream_reading_nothing(&mut db, &dsn, "straum_endedagain");
    let watching = Watching {
        max_retained_wal: SMALL_LIMIT_BYTES,
        unwatched: UNWATCHED,
    };
    let watchdog = Watchdog::start(&dsn, "straum_endedagain", watching, Vec::new()).unwrap();
    assert_slot_goes(&mut db, "the walsender outlived the watchdog");
    assert!(matches!(watchdog.verdict(), Some(Verdict::Retained { .. })));
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

/// PostgreSQL at `wal_level = replica` with [`SMALL_SEND_BUFFERS`].
fn postgres_with_small_send_buffers() -> PostgresContainer {
    plain(
        image(&tag())
            .with_cmd(&["-c", "wal_level=replica"])
            // The sysctl belongs to the container's own network namespace.
            .with_sysctl("net.ipv4.tcp_wmem", SMALL_SEND_BUFFERS),
    )
}

/// A physical stream on `slot` that reads nothing once started, and its
/// walsender's pid, returned once the walsender's sends have stopped on
/// the full socket. Filler goes on until they do, since how much the
/// sockets between hold differs from host to host.
fn stream_reading_nothing(db: &mut Db, dsn: &Dsn, slot: &str) -> (Connection, String) {
    let mut stream = Connection::connect(dsn, ConnectionKind::Replication).unwrap();
    stream
        .rows(&format!(
            "CREATE_REPLICATION_SLOT {slot} TEMPORARY PHYSICAL RESERVE_WAL"
        ))
        .unwrap();
    let from = db.rows("SELECT pg_current_wal_lsn()")[0][0]
        .clone()
        .unwrap();
    stream
        .copy_both(&format!("START_REPLICATION SLOT {slot} PHYSICAL {from}"))
        .unwrap();
    let walsender = db.rows(&format!(
        "SELECT active_pid FROM pg_replication_slots WHERE slot_name = '{slot}'"
    ))[0][0]
        .clone()
        .unwrap();
    db.run(FILLER);
    // Behind, idle and not sending for a while: a walsender merely slow
    // on a busy host moves within one sample, or reads WAL meanwhile.
    let sent = format!(
        "SELECT r.sent_lsn::text,
            pg_wal_lsn_diff(pg_current_wal_flush_lsn(), r.sent_lsn) > 0,
            a.wait_event = 'WalSenderMain'
         FROM pg_stat_replication r JOIN pg_stat_activity a USING (pid)
         WHERE r.pid = {walsender}"
    );
    let deadline = Instant::now() + LONG;
    let mut before = db.rows(&sent);
    let mut steady = 0;
    loop {
        assert!(
            Instant::now() < deadline,
            "the walsender's sends never stopped"
        );
        thread::sleep(STUCK_SAMPLE);
        let now = db.rows(&sent);
        let behind = now[0][1].as_deref() == Some("t");
        let idle = now[0][2].as_deref() == Some("t");
        steady = if behind && idle && now == before {
            steady + 1
        } else {
            0
        };
        if steady == STUCK_SAMPLES {
            return (stream, walsender);
        }
        if !behind {
            db.run(FILLER_MORE);
        }
        before = now;
    }
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
