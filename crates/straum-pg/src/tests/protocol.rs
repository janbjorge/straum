//! The PostgreSQL wire protocol against a fake server: signing in,
//! queries, copies and the replication stream, with each error a broken
//! or hostile server can cause, and how the stream answers keepalives and
//! reports what was applied.

use std::thread;
use std::time::{Duration, Instant};

use straum_app::{Lsn, Source};
use straum_domain::{Spec, Store};

use super::references::*;
use super::support::*;
use crate::decode::Decoder;
use crate::dsn::Dsn;
use crate::source::{Postgres, Replication, WAL_READ_TIMEOUT, plain, read_wal};
use crate::stream::{Reader, Stream};
use crate::wire::{Connection, ConnectionKind, HANDSHAKE_TIMEOUT};

#[test]
fn connection_errors() {
    let pg = postgres();
    let spec = straum_notation::parse(&SPEC.replace("$S", "p")).unwrap();
    sign_in_needs_the_right_password(&pg, &spec);
    let mut db = Db::new(&pg, "errors", "");
    a_failed_query_keeps_the_connection(&mut db);
    nothing_listens_on_port_1(&spec);
    an_error_brings_its_detail(&mut db);
    nothing_to_read_before_open(&db, &spec);
}

/// A wrong password, and a DSN with none, fail to sign in.
fn sign_in_needs_the_right_password(pg: &PostgresContainer, spec: &Spec) {
    let wrong = pg.dsn.to_string().replace("postgres@", "postgres:wrong@");
    let mut source = Postgres::new(
        Dsn::parse(&wrong).unwrap(),
        "p",
        ROW_LIMIT,
        MAX_RETAINED_WAL_BYTES,
        Replication::Logical,
    );
    let error = source.open(spec).unwrap_err().to_string();
    assert!(
        error.contains("password authentication failed") && error.contains("(28P01)"),
        "{error}"
    );

    let bare = Dsn::parse(&pg.dsn.to_string()).unwrap();
    let error = Postgres::new(
        bare,
        "p",
        ROW_LIMIT,
        MAX_RETAINED_WAL_BYTES,
        Replication::Logical,
    )
    .open(spec)
    .unwrap_err()
    .to_string();
    assert_eq!(
        error,
        "PostgreSQL sent a password request; the DSN has none"
    );
}

fn a_failed_query_keeps_the_connection(db: &mut Db) {
    let error = db
        .connection
        .execute("SELECT nope")
        .unwrap_err()
        .to_string();
    assert!(
        error.starts_with("PostgreSQL: column \"nope\" does not exist (42703)"),
        "{error}"
    );
    assert_eq!(db.rows("SELECT 1"), [[Some("1".to_owned())]]);
}

fn nothing_listens_on_port_1(spec: &Spec) {
    let closed = Dsn::parse("postgres://straum@127.0.0.1:1/straum").unwrap();
    let error = Postgres::new(
        closed,
        "p",
        ROW_LIMIT,
        MAX_RETAINED_WAL_BYTES,
        Replication::Logical,
    )
    .open(spec)
    .unwrap_err()
    .to_string();
    assert!(error.starts_with("connection to PostgreSQL: "), "{error}");
}

fn an_error_brings_its_detail(db: &mut Db) {
    db.run("CREATE TABLE $S.u (id int PRIMARY KEY); INSERT INTO $S.u VALUES (1)");
    let error = db
        .connection
        .execute("INSERT INTO errors.u VALUES (1)")
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "PostgreSQL: duplicate key value violates unique constraint \"u_pkey\": \
         Key (id)=(1) already exists. (23505)"
    );
}

fn nothing_to_read_before_open(db: &Db, spec: &Spec) {
    let mut idle = db.pg();
    let stopped = "the replication stream stopped";
    let error = idle.next_commit(Duration::ZERO).unwrap_err();
    assert_eq!(error.to_string(), stopped);
    assert_eq!(idle.wal().unwrap_err().to_string(), stopped);
    let mut store = Store::new(spec.clone());
    let trips = spec.fact("trips").unwrap();
    let error = idle.load(&trips.name, &mut store);
    assert_eq!(error.unwrap_err().to_string(), stopped);
}

#[test]
fn sign_in_errors() {
    let cases = [
        (
            message(b'R', &3_i32.to_be_bytes()),
            "PostgreSQL sent an authentication request Straum doesn't support; \
             Straum signs in with SCRAM-SHA-256 or trust",
        ),
        (
            sasl("SCRAM-SHA-256-PLUS\0\0"),
            "PostgreSQL sent SASL mechanisms [\"SCRAM-SHA-256-PLUS\"]; Straum needs SCRAM-SHA-256",
        ),
        (
            message(b'R', &11_i32.to_be_bytes()),
            "PostgreSQL sent SASL continue",
        ),
        (
            message(b'R', &12_i32.to_be_bytes()),
            "PostgreSQL sent SASL final",
        ),
        (
            error_response("28000", "no pg_hba.conf entry"),
            "PostgreSQL: no pg_hba.conf entry (28000)",
        ),
    ];
    for (reply, expected) in cases {
        let error = connect(vec![now(reply)], ConnectionKind::Sql)
            .err()
            .unwrap();
        assert_eq!(error.to_string(), expected);
    }
    // A SCRAM exchange begun must end with the server's proof, which the
    // real server gives: signed in without it, Straum refuses.
    let mut unproven = sasl("SCRAM-SHA-256\0\0");
    unproven.extend(signed_in());
    let server = scripted(vec![now(unproven)]);
    let dsn = server.dsn.to_string().replace("straum@", "straum:secret@");
    let error = Connection::connect(&Dsn::parse(&dsn).unwrap(), ConnectionKind::Sql);
    assert_eq!(
        error.err().unwrap().to_string(),
        "PostgreSQL sent AuthenticationOk before its SCRAM proof that it knows the password"
    );
    // Notices and parameters along the way are fine.
    let mut chatty = message(b'N', b"SNOTICE\0Mhello\0\0");
    chatty.extend(message(b'S', b"TimeZone\0UTC\0"));
    chatty.extend(signed_in());
    assert!(connect(vec![now(chatty)], ConnectionKind::Replication).is_ok());
}

/// A server that takes the socket and then says nothing fails the start
/// at the handshake timeout, not never.
#[test]
fn a_silent_server_fails_the_start() {
    let silence = HANDSHAKE_TIMEOUT * 6;
    let clock = Instant::now();
    let error = connect(vec![later(silence, signed_in())], ConnectionKind::Sql)
        .err()
        .unwrap();
    assert!(clock.elapsed() < silence, "the start waited out the server");
    assert_eq!(
        error.to_string(),
        "connection to PostgreSQL: PostgreSQL sent nothing for 10 s while the connection started"
    );
}

/// Once started, a connection waits on a quiet server past the handshake
/// timeout, as a replication connection on a quiet database must.
#[test]
fn a_started_connection_waits_past_the_handshake_timeout() {
    let mut done = message(b'C', b"SELECT 1\0");
    done.extend(message(b'Z', b"I"));
    let quiet = HANDSHAKE_TIMEOUT + Duration::from_secs(1);
    let mut connection = connect(
        vec![now(signed_in()), later(quiet, done)],
        ConnectionKind::Sql,
    )
    .unwrap();
    connection.execute("SELECT 1").unwrap();
}

#[test]
fn query_errors() {
    // CopyInResponse: Straum never copies in.
    let mut connection = connect(
        vec![now(signed_in()), now(message(b'G', &[0, 0, 0]))],
        ConnectionKind::Sql,
    )
    .unwrap();
    let error = connection.execute("SELECT 1").unwrap_err().to_string();
    assert_eq!(error, "PostgreSQL sent a message during a query");

    let mut row = 1_i16.to_be_bytes().to_vec();
    row.extend(1_i32.to_be_bytes());
    row.push(0xff);
    let mut connection = connect(
        vec![now(signed_in()), now(message(b'D', &row))],
        ConnectionKind::Sql,
    )
    .unwrap();
    let error = connection.execute("SELECT 1").unwrap_err().to_string();
    assert!(
        error.starts_with("connection to PostgreSQL: invalid utf-8"),
        "{error}"
    );

    // The server hangs up mid-query.
    let mut connection = connect(vec![now(signed_in())], ConnectionKind::Sql).unwrap();
    drop(connection.execute("SELECT 1"));
    let mut connection = connect(
        vec![now(signed_in()), now(b"D".to_vec())],
        ConnectionKind::Sql,
    )
    .unwrap();
    let error = connection.execute("SELECT 1").unwrap_err().to_string();
    assert_eq!(
        error,
        "connection to PostgreSQL: PostgreSQL closed the connection"
    );
    // A FATAL error, then the hang-up: the error says why.
    let fatal = error_response("57P01", "terminating connection");
    let mut connection = connect(vec![now(signed_in()), now(fatal)], ConnectionKind::Sql).unwrap();
    let error = connection.execute("SELECT 1").unwrap_err().to_string();
    assert_eq!(error, "PostgreSQL: terminating connection (57P01)");
}

#[test]
fn copy_both_errors() {
    let cases = [
        (
            error_response("42704", "replication slot \"x\" does not exist"),
            "PostgreSQL: replication slot \"x\" does not exist (42704)",
        ),
        (
            message(b'C', b"SELECT 1\0"),
            "PostgreSQL sent a message instead of CopyBothResponse",
        ),
    ];
    for (reply, expected) in cases {
        let mut connection = connect(
            vec![now(signed_in()), now(reply)],
            ConnectionKind::Replication,
        )
        .unwrap();
        let error = connection.copy_both("START_REPLICATION").unwrap_err();
        assert_eq!(error.to_string(), expected);
    }
}

/// A stream from a fake server, and the thread that returns what the
/// client sent the server.
struct FakeStream {
    stream: Stream,
    received: thread::JoinHandle<Vec<u8>>,
}

/// A stream from a fake server that sends `parts` once it has started.
fn scripted_stream(parts: Vec<Part>) -> FakeStream {
    let mut start = signed_in();
    start.extend(message(b'N', b"SNOTICE\0Mstarting\0\0"));
    start.extend(message(b'W', &[0, 0, 0]));
    let mut script = vec![now(start)];
    script.extend(parts);
    let server = scripted(script);
    let mut connection = Connection::connect(&server.dsn, ConnectionKind::Replication).unwrap();
    connection.copy_both("START_REPLICATION").unwrap();
    let stream = Stream::start(
        connection,
        Reader::Logical(Decoder::new(Vec::new(), ROW_LIMIT)),
        Lsn(0x10),
    )
    .unwrap();
    FakeStream {
        stream,
        received: server.received,
    }
}

#[test]
fn stream_errors() {
    let cases = [
        (copy_data(b"x"), "unknown pgoutput tag 'x'"),
        (copy_data(b"w"), "pgoutput message ends early"),
        (
            error_response("57P01", "terminating connection"),
            "PostgreSQL: terminating connection (57P01)",
        ),
        (
            message(b'c', b""),
            "PostgreSQL sent a message that ended the stream",
        ),
        (
            Vec::new(),
            "connection to PostgreSQL: PostgreSQL closed the connection",
        ),
    ];
    for (reply, expected) in cases {
        let mut fake = scripted_stream(vec![now(reply)]);
        assert_eq!(fake.stream.next(LONG).unwrap_err().to_string(), expected);
    }
}

/// A load checks the stream between batches: a stream that failed gives
/// its error, one that is only quiet gives nothing.
#[test]
fn a_check_of_the_stream_gives_its_failure() {
    let mut quiet = scripted_stream(vec![later(LONG, Vec::new())]);
    assert!(quiet.stream.check().is_ok());
    let fatal = error_response("57P01", "terminating connection");
    let mut failed = scripted_stream(vec![now(fatal)]);
    let clock = Instant::now();
    let error = loop {
        if let Err(error) = failed.stream.check() {
            break error;
        }
        assert!(clock.elapsed() < LONG, "the stream never failed");
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        error.to_string(),
        "PostgreSQL: terminating connection (57P01)"
    );
}

/// A walsender's notice or a setting it reports, which any backend may
/// send at any time, passes by: the stream goes on.
#[test]
fn notices_and_settings_on_the_stream_pass_by() {
    let quiet = later(Duration::from_secs(2), keepalive(0x18, Reply::NotAsked));
    let mut fake = scripted_stream(vec![
        now(message(b'N', b"SWARNING\0Mwatch out\0\0")),
        now(message(b'S', b"TimeZone\0UTC\0")),
        quiet,
    ]);
    assert_eq!(fake.stream.next(Duration::from_millis(300)).unwrap(), None);
    // Any other message still ends it.
    let mut ended = scripted_stream(vec![now(message(b'c', b""))]);
    assert!(ended.stream.next(LONG).is_err());
}

/// Under `synchronous_commit = off` a commit is acknowledged before its
/// WAL is written, so the reading counts WAL as inserted: Straum is behind
/// until it has the commit, and catches up once the WAL writer writes it.
#[test]
fn an_asynchronous_commit_counts_before_it_is_written() {
    let pg = postgres();
    let mut db = Db::new(&pg, "async", TABLES);
    db.run("ALTER SYSTEM SET wal_writer_delay = '10s'");
    db.run("SELECT pg_reload_conf()");
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    source.open(&spec).unwrap();
    db.run(&format!(
        "SET synchronous_commit = off; {}",
        Rng::new(1).trip(1)
    ));
    let acknowledged = db.insert_lsn();
    let wal = source.wal().unwrap();
    assert!(wal.written >= acknowledged, "{wal:?} {acknowledged:?}");
    assert!(wal.confirmed < wal.written, "{wal:?}");

    let commit = source.next_commit(3 * LONG).unwrap().unwrap();
    assert!(commit.lsn <= acknowledged);
    assert_eq!(
        source.next_commit(Duration::from_millis(100)).unwrap(),
        None
    );
    let deadline = Instant::now() + 3 * LONG;
    loop {
        let wal = source.wal().unwrap();
        if wal.confirmed >= wal.written {
            break;
        }
        assert!(Instant::now() < deadline, "never caught up: {wal:?}");
        thread::sleep(Duration::from_millis(100));
    }
}

/// A commit with no changes, as `XLogData` messages.
#[test]
fn reads_the_servers_wal() {
    let pg = postgres();
    let mut db = Db::new(&pg, "wal", TABLES);
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    let start = source.open(&spec).unwrap();
    // A new slot has confirmed everything before its consistent point.
    let wal = source.wal().unwrap();
    assert_eq!(wal.confirmed, start);
    assert!(wal.restart <= start && start <= wal.written, "{wal:?}");

    db.run(&Rng::new(1).trip(1));
    let commit = source.next_commit(LONG).unwrap().unwrap();
    assert!(source.wal().unwrap().written >= commit.lsn);
    // Asking for the next commit says this one is applied, and the stream
    // tells the server within a status interval.
    assert_eq!(
        source.next_commit(Duration::from_millis(100)).unwrap(),
        None
    );
    let deadline = Instant::now() + LONG;
    while source.wal().unwrap().confirmed < commit.lsn {
        assert!(
            Instant::now() < deadline,
            "the slot never confirmed the commit"
        );
        thread::sleep(Duration::from_millis(100));
    }

    // The reading's connection dies; the next reading connects again.
    db.run(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
          WHERE backend_type = 'client backend' AND application_name = 'straum'
            AND pid <> pg_backend_pid()",
    );
    // The reading tries once more on a new connection, and gets through.
    assert!(source.wal().unwrap().confirmed >= commit.lsn);

    // A slot the server doesn't have.
    let mut connection = Connection::connect(&pg.dsn, ConnectionKind::Sql).unwrap();
    let error = read_wal(&mut connection, "nope").unwrap_err().to_string();
    assert_eq!(error, "PostgreSQL sent no WAL position for slot nope");
}

/// The reply to the reading of the WAL: the insert position, the slot's
/// confirmed and restart positions, and the segment size.
fn wal_reply() -> Vec<u8> {
    let columns = ["0/3000000", "0/2000000", "0/1000000", "16777216"];
    let mut description = i16::try_from(columns.len()).unwrap().to_be_bytes().to_vec();
    let mut row = description.clone();
    for (column, value) in columns.iter().enumerate() {
        description.extend(format!("c{column}\0").as_bytes());
        // Table, column number, type, length, modifier, text format.
        description.extend([0; 4]);
        description.extend([0; 2]);
        description.extend(25_i32.to_be_bytes());
        description.extend((-1_i16).to_be_bytes());
        description.extend((-1_i32).to_be_bytes());
        description.extend([0; 2]);
        row.extend(i32::try_from(value.len()).unwrap().to_be_bytes());
        row.extend(value.as_bytes());
    }
    let mut reply = message(b'T', &description);
    reply.extend(message(b'D', &row));
    reply.extend(message(b'C', b"SELECT 1\0"));
    reply.extend(message(b'Z', b"I"));
    reply
}

/// The reading of the WAL fails once its server stays silent for the
/// read timeout, so the lag it no longer moves can grow; a server that
/// answers within the timeout is read.
#[test]
fn a_silent_server_fails_the_reading_of_the_wal() {
    let slack = Duration::from_secs(2);
    let just_within = WAL_READ_TIMEOUT.saturating_sub(slack);
    let silent = scripted(vec![
        now(signed_in()),
        later(WAL_READ_TIMEOUT * 3, Vec::new()),
    ]);
    let mut connection = None;
    let reading = plain(&mut connection, &silent.dsn).unwrap();
    let clock = Instant::now();
    assert!(read_wal(reading, "straum").is_err());
    assert!(clock.elapsed() >= just_within);
    assert!(
        clock.elapsed() < WAL_READ_TIMEOUT + slack,
        "the reading waited out the server"
    );

    let slow = scripted(vec![now(signed_in()), later(just_within, wal_reply())]);
    let mut connection = None;
    let reading = plain(&mut connection, &slow.dsn).unwrap();
    assert_eq!(
        read_wal(reading, "straum").unwrap().confirmed,
        Lsn(0x200_0000)
    );
}

#[test]
fn the_stream_waits_at_most_the_wait() {
    // The server stays quiet past the wait.
    let quiet = later(Duration::from_secs(2), keepalive(0x18, Reply::NotAsked));
    let mut fake = scripted_stream(vec![quiet]);
    let clock = Instant::now();
    assert_eq!(fake.stream.next(Duration::from_millis(200)).unwrap(), None);
    assert!(clock.elapsed() >= Duration::from_millis(200));
}

/// `XLogData` holding `message`: its start, end and send time are zero.
fn xlog_data(message: &[u8]) -> Vec<u8> {
    let mut body = vec![b'w'];
    body.extend([0; 24]);
    body.extend(message);
    copy_data(&body)
}

/// A transaction with no changes whose commit ends at `end`.
fn empty_commit(end: u64) -> Vec<u8> {
    let mut begin = vec![b'B'];
    begin.extend([0; 20]);
    let mut commit = vec![b'C', 0];
    commit.extend(0_u64.to_be_bytes());
    commit.extend(end.to_be_bytes());
    commit.extend(0_i64.to_be_bytes());
    [xlog_data(&begin), xlog_data(&commit)].concat()
}

/// The applied positions the client reported in standby status updates.
fn reported(received: &[u8]) -> Vec<u64> {
    // CopyData of 38 bytes holding a standby status update.
    let frame = [b'd', 0, 0, 0, 38, b'r'];
    let mut positions = Vec::new();
    for (at, window) in received.windows(frame.len()).enumerate() {
        if window == frame {
            let applied = received[at + 6..at + 14].try_into().unwrap();
            positions.push(u64::from_be_bytes(applied));
        }
    }
    positions
}

#[test]
fn the_stream_answers_and_reports() {
    let pause = Duration::from_millis(1_500);
    let commit = empty_commit(0x20);
    // A message split across a read timeout, a keepalive asking for an
    // answer between transactions, then a commit.
    let (head, tail) = commit.split_at(5);
    let mut fake = scripted_stream(vec![
        now(keepalive(0x18, Reply::NotAsked)),
        now(head.to_vec()),
        later(pause, tail.to_vec()),
        later(pause, keepalive(0x40, Reply::Asked)),
        now(empty_commit(0x50)),
    ]);
    assert_eq!(fake.stream.next(LONG).unwrap().unwrap().lsn, Lsn(0x20));
    assert_eq!(fake.stream.next(LONG).unwrap().unwrap().lsn, Lsn(0x50));
    drop(fake.stream);
    // The start, then once the first commit is applied, everything up to
    // the keepalive at 0x40.
    let reported = reported(&fake.received.join().unwrap());
    assert!(reported.contains(&0x40), "{reported:x?}");
    for position in &reported {
        assert!([0x10, 0x18, 0x20, 0x40].contains(position), "{reported:x?}");
    }
}

/// A report between transactions may name a keepalive's position past
/// the last commit; one sent inside the next transaction must not go
/// back to that commit, which would move a physical slot back.
#[test]
fn the_stream_never_reports_less_than_before() {
    let pause = Duration::from_millis(500);
    let mut begin = vec![b'B'];
    begin.extend([0; 20]);
    let mut fake = scripted_stream(vec![
        now(empty_commit(0x20)),
        later(pause, keepalive(0x40, Reply::Asked)),
        later(pause, xlog_data(&begin)),
        later(pause, keepalive(0x48, Reply::Asked)),
        // Open until the wait below ends.
        later(pause * 4, Vec::new()),
    ]);
    assert_eq!(fake.stream.next(LONG).unwrap().unwrap().lsn, Lsn(0x20));
    // Asking again applies 0x20; nothing more commits.
    assert_eq!(fake.stream.next(pause * 4).unwrap(), None);
    drop(fake.stream);
    let reported = reported(&fake.received.join().unwrap());
    assert!(reported.contains(&0x40), "{reported:x?}");
    for pair in reported.windows(2) {
        assert!(pair[0] <= pair[1], "{reported:x?}");
    }
}

#[test]
fn a_commit_handed_out_is_not_yet_applied() {
    // The writer takes the commit at 0x20 and never asks for the next, so
    // it may still be applying it: nothing past the start is reported, not
    // even the keepalive that follows.
    let mut fake = scripted_stream(vec![
        now(empty_commit(0x20)),
        later(Duration::from_millis(500), keepalive(0x40, Reply::Asked)),
    ]);
    assert_eq!(fake.stream.next(LONG).unwrap().unwrap().lsn, Lsn(0x20));
    thread::sleep(Duration::from_millis(1_500));
    drop(fake.stream);
    let reported = reported(&fake.received.join().unwrap());
    assert_ne!(reported, Vec::<u64>::new());
    for position in &reported {
        assert_eq!(*position, 0x10, "{reported:x?}");
    }
}
