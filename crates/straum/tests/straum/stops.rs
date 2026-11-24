//! How Straum fails and stops: exit 1 on a failure to load or follow or
//! past `--max-memory`, not ready while PostgreSQL stays silent, a stop
//! that waits for the requests in flight at most ten seconds, and output
//! to a reader that has left.

use std::io::{self, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use crate::{
    ASK, SPEC, Scratch, TABLES, Until, no_slot_left, not_ready_exits_with_3, output, postgres, run,
    set_up, straum,
};

/// Straum fails hard: a setup missing at the start, or a table that
/// stops fitting while it runs, ends it with exit 1 naming the problem,
/// for whatever runs it to start it again. Its slot goes with it. With
/// the setup whole it serves until the table stops fitting.
#[test]
fn a_failure_ends_straum_with_exit_1() {
    let pg = postgres();
    pg.psql(TABLES);
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    let publication = set_up(&pg, &spec, Some("CREATE PUBLICATION"));
    let args = ["run", "--wal", "logical", &spec, "--listen", "127.0.0.1:0"];
    let (code, log) = run(&args, &pg.dsn("straum")).exited();
    assert_eq!(code, Some(1), "{log}");
    assert!(log.contains("publication"), "{log}");
    for line in log.lines() {
        assert!(line.starts_with("straum: "), "{line}");
    }
    no_slot_left(&pg);

    pg.psql(&publication[0]);
    let mut straum = run(&args, &pg.dsn("straum"));
    straum.wait_for("ready", &Until::Ready);
    let (status, body) = straum.call("POST", "/ask", ASK);
    assert_eq!(status, 200, "{body}");
    pg.psql("ALTER PUBLICATION straum DROP TABLE shop.trips");
    let (code, log) = straum.exited();
    assert_eq!(code, Some(1), "{log}");
    assert!(log.contains("leaves out shop.trips"), "{log}");
    no_slot_left(&pg);
}

/// A server that takes the connection and never answers: Straum's start
/// waits on it.
fn silent_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || hold_connections(&listener));
    format!("postgres://straum:straum@127.0.0.1:{port}/postgres?sslmode=disable")
}

fn hold_connections(listener: &TcpListener) {
    let mut held = Vec::new();
    for stream in listener.incoming() {
        held.push(stream);
    }
}

/// While its start waits, Straum answers not ready; a server silent for
/// 10 s then ends it with exit 1. Needs no PostgreSQL.
#[test]
fn answers_not_ready_until_a_silent_server_ends_it() {
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    let dsn = silent_server();
    let straum = run(
        &["run", "--wal", "logical", &spec, "--listen", "127.0.0.1:0"],
        &dsn,
    );
    let (status, body) = straum.call("GET", "/readyz", "");
    assert_eq!(status, 503);
    assert!(body.starts_with("not ready: "), "{body}");
    let (status, body) = straum.call("POST", "/ask", ASK);
    assert_eq!(
        (status, body.as_str()),
        (503, r#"{"errors":["not ready"]}"#)
    );
    not_ready_exits_with_3(&format!("http://{}", straum.address));
    let (code, log) = straum.exited();
    assert_eq!(code, Some(1), "{log}");
    // The DSN's host and port lie between `@` and the database.
    let (_, place) = dsn.split_once('@').unwrap();
    let (place, _) = place.split_once('/').unwrap();
    assert!(place.starts_with("127.0.0.1:"), "{place}");
    let named = format!("PostgreSQL at {place} sent nothing for 10 s");
    assert!(log.contains(&named), "{log}");
}

/// The `straum run` line with `--max-memory 1Ki`, under what any process
/// holds, against a server that never answers. Straum on Linux holds
/// about 1.1 MiB as it starts, too close to a limit of 1Mi.
fn run_with_one_kibibyte(spec: &str, dsn: &str) -> (Option<i32>, String) {
    let args = [
        "run",
        "--wal",
        "logical",
        spec,
        "--listen",
        "127.0.0.1:0",
        "--max-memory",
        "1Ki",
    ];
    let (code, _, log) = output(straum(&args).env("STRAUM_DSN", dsn));
    (code, log)
}

/// Past `--max-memory` Straum stops with exit 1, from the start on, so
/// a load is watched too; the line names the flag and both sizes. Linux
/// gives the figure in `/proc`, macOS through `ps`.
#[test]
fn past_max_memory_straum_stops() {
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    let (code, log) = run_with_one_kibibyte(&spec, &silent_server());
    assert_eq!(code, Some(1), "{log}");
    // The main thread may log `listening on` before or after it.
    let mut stops = Vec::new();
    for line in log.lines() {
        if line.starts_with("straum: Straum holds ") {
            stops.push(line);
        }
    }
    let [stop] = stops.as_slice() else {
        panic!("{log}");
    };
    assert!(
        stop.ends_with(" MiB of resident memory, past the --max-memory limit of 0.0 MiB"),
        "{log}"
    );
}

/// A stop waits for a request still coming in, but at most 10 s, and a
/// second signal ends Straum at once.
#[test]
fn a_stop_waits_for_requests_in_flight_at_most_ten_seconds() {
    let pg = postgres();
    pg.psql(TABLES);
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    set_up(&pg, &spec, None);
    for twice in [false, true] {
        let mut straum = run(
            &["run", "--wal", "logical", &spec, "--listen", "127.0.0.1:0"],
            &pg.dsn("straum"),
        );
        straum.wait_for("ready", &Until::Ready);
        let mut held = TcpStream::connect(straum.address).unwrap();
        // A head never ended: hyper waits 30 s for the rest.
        let unended = format!("POST /ask HTTP/1.1\r\nhost: {}\r\n", straum.address);
        held.write_all(unended.as_bytes()).unwrap();
        thread::sleep(Duration::from_millis(200));
        let clock = Instant::now();
        straum.signal("-TERM");
        thread::sleep(Duration::from_secs(1));
        assert!(straum.running(), "stopped with a request in flight");
        if twice {
            straum.signal("-TERM");
        }
        let (code, log) = straum.stop("-0");
        let waited = clock.elapsed();
        assert_eq!(code, Some(0), "{log}");
        for line in log.lines() {
            assert!(line.starts_with("straum: "), "{line}");
        }
        let within = if twice { 1..3 } else { 9..15 };
        assert!(within.contains(&waited.as_secs()), "{twice}: {waited:?}");
        drop(held);
    }
}

/// Output to a reader that has left, as `head` leaves, is no failure:
/// each command that prints exits 0, and `straum run` keeps running with
/// no one reading its log.
#[test]
fn a_reader_that_left_is_no_failure() {
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    for args in [
        &["help"][..],
        &["version"],
        &["plan", "--wal", "logical", &spec],
    ] {
        let (reader, writer) = io::pipe().unwrap();
        drop(reader);
        let status = straum(args).stdout(writer).status().unwrap();
        assert_eq!(status.code(), Some(0), "{args:?}");
    }
    let (reader, writer) = io::pipe().unwrap();
    drop(reader);
    let mut child = straum(&["run", "--wal", "logical", &spec, "--listen", "127.0.0.1:0"])
        .env("STRAUM_DSN", silent_server())
        .stderr(writer)
        .spawn()
        .unwrap();
    // It logs that it listens, and waits on the silent server for 10 s.
    thread::sleep(Duration::from_secs(2));
    assert_eq!(child.try_wait().unwrap(), None, "straum run exited");
    child.kill().unwrap();
    child.wait().unwrap();
}
