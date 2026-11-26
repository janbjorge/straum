//! The PostgreSQL wire protocol against a fake server: signing in,
//! queries, copies and the replication stream, with each error a broken
//! or hostile server can cause, and the reading of the server's WAL.
//! What the stream reports is tested apart from any server, in
//! `stream.rs`.

use std::io::Write;
use std::net::TcpListener;
use std::sync::mpsc;
use std::time::{Duration, Instant};
use std::{slice, thread};

use postgres_protocol::message::backend::Message;
use straum_app::{LoadCounts, Lsn, Source};
use straum_domain::{Spec, Store};

use super::references::*;
use super::support::*;
use crate::decode::Decoder;
use crate::dsn::Dsn;
use crate::source::{Replication, WAL_READ_TIMEOUT, plain, read_wal};
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
    let mut source = source_over(
        dsn_as(&pg.dsn, "postgres", "wrong"),
        "p",
        Replication::Logical,
    );
    let error = source.open(spec).unwrap_err().to_string();
    assert!(
        error.contains("password authentication failed") && error.contains("(28P01)"),
        "{error}"
    );

    let bare = Dsn::parse(&pg.dsn.to_string()).unwrap();
    let error = source_over(bare, "p", Replication::Logical)
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

/// A connection that fails names where it tried: a port nothing
/// listens on, and a host no name server knows.
fn nothing_listens_on_port_1(spec: &Spec) {
    for (dsn, place) in [
        ("postgres://straum@127.0.0.1:1/straum", "127.0.0.1:1"),
        (
            "postgres://straum@nowhere.invalid:5432/straum",
            "nowhere.invalid:5432",
        ),
    ] {
        let error = source_over(Dsn::parse(dsn).unwrap(), "p", Replication::Logical)
            .open(spec)
            .unwrap_err()
            .to_string();
        let start = format!("connection to PostgreSQL: {place}: ");
        assert!(error.starts_with(&start), "{error}");
    }
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
    let error = idle.load(
        slice::from_ref(&trips.name),
        &mut store,
        &LoadCounts::default(),
    );
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
    // Ready with no AuthenticationOk at all: after a SCRAM exchange begun,
    // or with no sign-in, Straum refuses. The honest exchange signs in to
    // the real server in `connection_errors`.
    let mut skipped = sasl("SCRAM-SHA-256\0\0");
    skipped.extend(message(b'Z', b"I"));
    for reply in [skipped, message(b'Z', b"I")] {
        let server = scripted(vec![now(reply)]);
        let dsn = server.dsn.to_string().replace("straum@", "straum:secret@");
        let error = Connection::connect(&Dsn::parse(&dsn).unwrap(), ConnectionKind::Sql);
        assert_eq!(
            error.err().unwrap().to_string(),
            "PostgreSQL sent ReadyForQuery before AuthenticationOk"
        );
    }
    // Notices and parameters along the way are fine.
    let mut chatty = message(b'N', b"SNOTICE\0Mhello\0\0");
    chatty.extend(message(b'S', b"TimeZone\0UTC\0"));
    chatty.extend(signed_in());
    assert!(connect(vec![now(chatty)], ConnectionKind::Replication).is_ok());
}

/// Each startup turns `transaction_timeout` off, so a setting on the
/// database or the role never ends the connection before that.
#[test]
fn the_startup_turns_the_transaction_timeout_off() {
    let server = scripted(vec![now(signed_in())]);
    drop(Connection::connect(&server.dsn, ConnectionKind::Sql).unwrap());
    let received = String::from_utf8_lossy(&server.received.join().unwrap()).into_owned();
    assert!(
        received.contains("transaction_timeout\u{0}0\u{0}"),
        "{received:?}"
    );
    assert!(
        !received.contains("SET transaction_timeout"),
        "{received:?}"
    );
}

/// PostgreSQL 16 lacks `transaction_timeout` and ends a startup that names
/// it. Straum then starts again without it, so the setup check can say
/// that the server is too old. Another unknown name fails the start.
#[test]
fn a_server_without_the_transaction_timeout_gets_a_second_startup() {
    let unknown = error_response(
        "42704",
        "unrecognized configuration parameter \"transaction_timeout\"",
    );
    let (dsn, received) = served_twice(vec![now(unknown)], vec![now(signed_in())]);
    drop(Connection::connect(&dsn, ConnectionKind::Sql).unwrap());
    let [first, second] = received
        .join()
        .unwrap()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned());
    assert!(first.contains("transaction_timeout\0"), "{first:?}");
    assert!(!second.contains("transaction_timeout"), "{second:?}");
    assert!(
        second.contains("statement_timeout\u{0}0\u{0}"),
        "{second:?}"
    );

    let other = error_response("42704", "unrecognized configuration parameter \"nope\"");
    let error = connect(vec![now(other)], ConnectionKind::Sql)
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        "PostgreSQL: unrecognized configuration parameter \"nope\" (42704)"
    );
}

/// A fake server that plays `first` to its first client and `second` to
/// the next, and returns what each sent.
fn served_twice(first: Vec<Part>, second: Vec<Part>) -> (Dsn, thread::JoinHandle<[Vec<u8>; 2]>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let received = thread::spawn(move || [serve(&listener, first), serve(&listener, second)]);
    let dsn = format!("postgres://straum@127.0.0.1:{port}/straum?sslmode=disable");
    (Dsn::parse(&dsn).unwrap(), received)
}

/// A server that takes the socket and then says nothing fails the start
/// at the handshake timeout, not never, naming the server's host and port.
#[test]
fn a_silent_server_fails_the_start() {
    let silence = HANDSHAKE_TIMEOUT * 6;
    let server = scripted(vec![later(silence, signed_in())]);
    let clock = Instant::now();
    let error = Connection::connect(&server.dsn, ConnectionKind::Sql)
        .err()
        .unwrap();
    assert!(clock.elapsed() < silence, "the start waited out the server");
    assert_eq!(
        error.to_string(),
        format!(
            "connection to PostgreSQL: PostgreSQL at 127.0.0.1:{} sent nothing for 10 s \
             while the connection started",
            server.dsn.port
        )
    );
}

/// A server that answers the start with an error fails it in its own
/// words, with no host and port and no word of silence.
#[test]
fn a_server_that_answers_fails_the_start_without_the_place() {
    let server = scripted(vec![now(error_response("28000", "no pg_hba.conf entry"))]);
    let error = Connection::connect(&server.dsn, ConnectionKind::Sql)
        .err()
        .unwrap()
        .to_string();
    assert_eq!(error, "PostgreSQL: no pg_hba.conf entry (28000)");
    assert!(!error.contains("127.0.0.1"), "{error}");
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

    // The server hangs up mid-query, between messages and inside one.
    let mut connection = connect(vec![now(signed_in())], ConnectionKind::Sql).unwrap();
    let error = connection.execute("SELECT 1").unwrap_err().to_string();
    assert_eq!(
        error,
        "connection to PostgreSQL: PostgreSQL closed the connection"
    );
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

/// A message cut by a read timeout reads whole once the rest comes: each
/// read that times out keeps the bytes before it, and adds none.
#[test]
fn a_message_split_across_a_read_timeout_reads_whole() {
    // Its tag and its length.
    const MESSAGE_HEADER_BYTES: usize = 5;
    let whole = keepalive(0x18);
    let (first, rest) = whole.split_at(whole.len() / 2);
    let (send_rest, rest_asked) = mpsc::channel();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let first = [signed_in(), first.to_vec()].concat();
    let rest = rest.to_vec();
    // The server holds the rest back until the test asks for it, so the
    // reads before that see half a message on any machine, however slow.
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.write_all(&first).unwrap();
        rest_asked.recv().unwrap();
        socket.write_all(&rest).unwrap();
        socket
    });
    let dsn = format!("postgres://straum@127.0.0.1:{port}/straum?sslmode=disable");
    let dsn = Dsn::parse(&dsn).unwrap();
    let mut connection = Connection::connect(&dsn, ConnectionKind::Replication).unwrap();
    connection
        .set_read_timeout(Duration::from_millis(50))
        .unwrap();
    // Half a message gives nothing: once for the first half, then once
    // timed out.
    for _ in 0..2 {
        assert!(connection.try_read().unwrap().is_none());
    }
    send_rest.send(()).unwrap();
    let wait = Wait::new("the rest of the message");
    let message = loop {
        if let Some(message) = connection.try_read().unwrap() {
            break message;
        }
        wait.check();
    };
    let Message::CopyData(body) = message else {
        panic!("not CopyData");
    };
    assert_eq!(body.data(), &whole[MESSAGE_HEADER_BYTES..]);
    drop(server.join().unwrap());
}

/// A stream from a fake server that sends `parts` once it has started.
fn scripted_stream(parts: Vec<Part>) -> Stream {
    let mut start = signed_in();
    start.extend(message(b'N', b"SNOTICE\0Mstarting\0\0"));
    start.extend(message(b'W', &[0, 0, 0]));
    let mut script = vec![now(start)];
    script.extend(parts);
    let server = scripted(script);
    let mut connection = Connection::connect(&server.dsn, ConnectionKind::Replication).unwrap();
    connection.copy_both("START_REPLICATION").unwrap();
    Stream::start(
        connection,
        Reader::Logical(Decoder::new(Vec::new(), ROW_LIMIT, STREAM_MEMORY)),
        Lsn(0x10),
        STREAM_MEMORY,
    )
    .unwrap()
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
        assert_eq!(fake.next(LONG).unwrap_err().to_string(), expected);
    }
}

/// A load checks the stream between batches: a stream that failed gives
/// its error, one that is only quiet gives nothing.
#[test]
fn a_check_of_the_stream_gives_its_failure() {
    let mut quiet = scripted_stream(vec![later(LONG, Vec::new())]);
    assert!(quiet.check().is_ok());
    let fatal = error_response("57P01", "terminating connection");
    let mut failed = scripted_stream(vec![now(fatal)]);
    let wait = Wait::new("the stream to fail");
    let error = loop {
        if let Err(error) = failed.check() {
            break error;
        }
        wait.poll();
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
    let quiet = later(Duration::from_secs(2), keepalive(0x18));
    let mut fake = scripted_stream(vec![
        now(message(b'N', b"SWARNING\0Mwatch out\0\0")),
        now(message(b'S', b"TimeZone\0UTC\0")),
        quiet,
    ]);
    assert_eq!(fake.next(Duration::from_millis(300)).unwrap(), None);
    // Any other message still ends it.
    let mut ended = scripted_stream(vec![now(message(b'c', b""))]);
    assert!(ended.next(LONG).is_err());
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
    assert!(wal.applied < wal.written, "{wal:?}");

    let commit = source.next_commit(3 * LONG).unwrap().unwrap();
    assert!(commit.lsn <= acknowledged);
    assert_eq!(
        source.next_commit(Duration::from_millis(100)).unwrap(),
        None
    );
    // Written out, not a `Wait`: a failure here shows the reading that
    // never caught up.
    let deadline = Instant::now() + 3 * LONG;
    loop {
        let wal = source.wal().unwrap();
        if wal.applied >= wal.written {
            break;
        }
        assert!(Instant::now() < deadline, "never caught up: {wal:?}");
        thread::sleep(WAIT_POLL);
    }
}

/// A commit with no changes, as `XLogData` messages.
#[test]
fn reads_the_servers_wal() {
    let pg = postgres();
    let mut db = Db::new(&pg, "wal", TABLES);
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    let start = source.open(&spec).unwrap().start;
    // Everything before the start is applied, and idle WAL after it may
    // be since.
    let wal = source.wal().unwrap();
    assert!(wal.applied >= start, "{wal:?}");
    assert!(wal.restart <= start && start <= wal.written, "{wal:?}");

    db.run(&Rng::new(1).trip(1));
    let commit = source.next_commit(LONG).unwrap().unwrap();
    assert!(source.wal().unwrap().written >= commit.lsn);
    // Asking for the next commit says this one is applied, and the stream
    // counts it within a status interval.
    assert_eq!(
        source.next_commit(Duration::from_millis(100)).unwrap(),
        None
    );
    let wait = Wait::new("the commit to count as applied");
    while source.wal().unwrap().applied < commit.lsn {
        wait.poll();
    }

    // The reading's connection dies; the next reading connects again.
    db.run(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
          WHERE backend_type = 'client backend' AND application_name = 'straum'
            AND pid <> pg_backend_pid()",
    );
    // The reading tries once more on a new connection, and gets through.
    assert!(source.wal().unwrap().applied >= commit.lsn);

    // A slot the server doesn't have.
    let mut connection = Connection::connect(&pg.dsn, ConnectionKind::Sql).unwrap();
    let error = read_wal(&mut connection, "nope", Lsn(0), start)
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "slot nope is gone: PostgreSQL ended its session; the server log says why"
    );
}

/// A publication whose name holds a backslash and a quote streams: the
/// grammar of a replication command does not accept an `E'...'` string.
#[test]
fn a_publication_named_with_a_backslash_streams() {
    let pg = postgres();
    let mut db = Db::new(&pg, "backslash", TABLES);
    db.run(r#"CREATE PUBLICATION "a\b'c" FOR TABLE $S.zones, $S.trips, $S.logins"#);
    let spec = db.spec(SPEC);
    let mut source = source_over(db.dsn.clone(), r"a\b'c", Replication::Logical);
    source.open(&spec).unwrap();
    let before_commit = db.run_committed(&Rng::new(1).trip(1));
    let commit = source.next_commit(LONG).unwrap().unwrap();
    assert!(commit.lsn > before_commit, "{commit:?}");

    // A name one character off is not that publication.
    let mut other = source_over(db.dsn.clone(), r"a\b'd", Replication::Logical);
    let error = other.open(&spec).unwrap_err().to_string();
    assert_eq!(
        error,
        r"publication a\b'd does not exist; run the SQL straum plan prints"
    );
}

/// The reply to the reading of the WAL: the insert position, the slot's
/// restart position, and the segment size.
fn wal_reply() -> Vec<u8> {
    wal_reply_of(["0/3000000", "0/1000000", "16777216"])
}

/// The reply to the reading of the WAL, with `columns` as its row.
fn wal_reply_of(columns: [&str; 3]) -> Vec<u8> {
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
    assert!(read_wal(reading, "straum", Lsn(0), Lsn(0)).is_err());
    assert!(clock.elapsed() >= just_within);
    assert!(
        clock.elapsed() < WAL_READ_TIMEOUT + slack,
        "the reading waited out the server"
    );

    let slow = scripted(vec![now(signed_in()), later(just_within, wal_reply())]);
    let mut connection = None;
    let reading = plain(&mut connection, &slow.dsn).unwrap();
    let wal = read_wal(reading, "straum", Lsn(0), Lsn(0x200_0000)).unwrap();
    assert_eq!(
        (wal.applied, wal.restart),
        (Lsn(0x200_0000), Lsn(0x100_0000))
    );
}

/// Retained WAL counts from where the slot was made, as the watchdog
/// counts it: a physical slot's restart point before that is the
/// server's to recycle. A restart point after it counts from there.
#[test]
fn retained_wal_counts_from_where_the_slot_was_made() {
    let restart = Lsn(0x100_0000);
    for (from, counted_from) in [
        (Lsn(0x180_0000), Lsn(0x180_0000)),
        (Lsn(0x80_0000), restart),
    ] {
        let server = scripted(vec![now(signed_in()), now(wal_reply())]);
        let mut connection = None;
        let reading = plain(&mut connection, &server.dsn).unwrap();
        let wal = read_wal(reading, "straum", from, Lsn(0)).unwrap();
        assert_eq!(wal.restart, counted_from, "from {from}");
    }
}

/// A row for the slot whose values do not read is no WAL position, not a
/// slot that is gone.
#[test]
fn a_slot_row_that_does_not_read_is_no_wal_position() {
    let reply = wal_reply_of(["x", "0/1000000", "16777216"]);
    let server = scripted(vec![now(signed_in()), now(reply)]);
    let mut connection = None;
    let reading = plain(&mut connection, &server.dsn).unwrap();
    let error = read_wal(reading, "straum", Lsn(0), Lsn(0))
        .unwrap_err()
        .to_string();
    assert_eq!(error, "PostgreSQL sent no WAL position for slot straum");
}

#[test]
fn the_stream_waits_at_most_the_wait() {
    // The server stays quiet past the wait.
    let quiet = later(Duration::from_secs(2), keepalive(0x18));
    let mut fake = scripted_stream(vec![quiet]);
    let clock = Instant::now();
    assert_eq!(fake.next(Duration::from_millis(200)).unwrap(), None);
    assert!(clock.elapsed() >= Duration::from_millis(200));
}
