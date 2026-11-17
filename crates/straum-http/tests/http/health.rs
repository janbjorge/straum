//! The routes besides asks and views, end to end: `/livez`, `/readyz`,
//! `/status`, the replies to a wrong method or path, caching, and a
//! shutdown with requests still in flight.

use std::io::Write;
use std::net::TcpStream;
use std::thread;
use std::time::Duration;

use straum_app::{Limits, Lsn};

use crate::fake::{LIMITS, START, fake, writer, writer_with};
use crate::{call, first_hour, read_reply, serve, started};

#[test]
fn no_reply_may_be_cached() {
    let (_writer, server) = started(writer());
    let answer = call(server.address, "POST", "/ask", &first_hour());
    let error = call(server.address, "POST", "/ask", "{");
    let status = call(server.address, "GET", "/status", "");
    for reply in [answer, error, status] {
        assert!(
            reply.head.contains("cache-control: no-store"),
            "{}",
            reply.head
        );
    }
    server.stop();
}

#[test]
fn routes() {
    let server = serve(writer().app().clone());
    let reply = call(server.address, "GET", "/livez", "");
    assert_eq!((reply.status, reply.body.as_str()), (200, "live\n"));
    let reply = call(server.address, "GET", "/nope", "");
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (404, "no /nope here\n")
    );
    for (method, path, allow) in [
        ("GET", "/ask", "POST"),
        ("POST", "/livez", "GET"),
        ("DELETE", "/readyz", "GET"),
        ("POST", "/status", "GET"),
        ("PUT", "/views", "GET"),
    ] {
        let reply = call(server.address, method, path, "");
        assert_eq!(reply.status, 405, "{method} {path}");
        assert!(
            reply.head.contains(&format!("allow: {allow}")),
            "{}",
            reply.head
        );
        assert_eq!(reply.body, format!("use {allow}\n"));
    }
    server.stop();
}

#[test]
fn status_before_a_load_says_nothing_yet() {
    let server = serve(writer().app().clone());
    let reply = call(server.address, "GET", "/status", "");
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.body,
        "\
ready:        no, loading
applied:      nothing yet
as of:        unknown until every table is loaded
lag:          unknown until every table is loaded
WAL retained: unknown until every table is loaded
"
    );
    server.stop();
}

#[test]
fn status_says_what_the_stream_applied() {
    let (mut writer, server) = started(writer());
    writer.step().unwrap();
    let reply = call(server.address, "GET", "/status", "");
    let lines: Vec<&str> = reply.body.lines().collect();
    assert_eq!(lines[0], "ready:        yes");
    assert_eq!(
        lines[1],
        "applied:      17/0, committed 2024-01-02T00:00:00.123456Z"
    );
    assert!(
        lines[2].ends_with("Z, every commit before it applied"),
        "{}",
        lines[2]
    );
    assert!(
        lines[3].starts_with("lag:          0.0 s, at most 30 s"),
        "{}",
        lines[3]
    );
    assert_eq!(
        lines[4],
        "WAL retained: 0.0 MiB, at most 4096.0 MiB; PostgreSQL is at 16/B374D848"
    );
    assert_eq!(lines.len(), 5);
    server.stop();
}

#[test]
fn readiness_says_why_not() {
    let mut writer = writer();
    let server = serve(writer.app().clone());
    let reply = call(server.address, "GET", "/readyz", "");
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (503, "not ready: loading\n")
    );
    writer.step().unwrap();
    let reply = call(server.address, "GET", "/readyz", "");
    assert_eq!((reply.status, reply.body.as_str()), (200, "ready\n"));
    // A failed writer's app is not ready; the process ends with it.
    writer.step().unwrap();
    writer.step().unwrap_err();
    let reply = call(server.address, "GET", "/readyz", "");
    assert_eq!(reply.status, 503);
    server.stop();
}

#[test]
fn readiness_says_how_far_behind() {
    // WAL written while the load took a second, which the slot hasn't
    // confirmed.
    let mut fake = fake();
    fake.wal.written = Lsn(START.0 + 1);
    fake.pause = Duration::from_secs(1);
    let limits = Limits {
        max_lag: Duration::ZERO,
        ..LIMITS
    };
    let (_writer, server) = started(writer_with(limits, fake));
    let reply = call(server.address, "GET", "/readyz", "");
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (503, "not ready: 1 s behind PostgreSQL\n")
    );
    // Asks still answer.
    assert_eq!(
        call(server.address, "POST", "/ask", &first_hour()).status,
        200
    );
    server.stop();
}

/// Long enough for the server to read what was sent, or to begin a
/// shutdown, before the test goes on.
const SETTLE: Duration = Duration::from_millis(200);

/// How much of the ask's body is sent before the shutdown begins.
const SENT_BEFORE_SHUTDOWN: usize = 10;

#[test]
fn shutdown_finishes_requests_in_flight() {
    let (_writer, server) = started(writer());
    let ask = first_hour();

    // An idle keep-alive connection doesn't hold the shutdown up, and a
    // request whose body is still coming finishes.
    let idle = TcpStream::connect(server.address).unwrap();
    let mut busy = TcpStream::connect(server.address).unwrap();
    let head = format!(
        "POST /ask HTTP/1.1\r\nhost: straum\r\ncontent-type: application/json\r\n\
         content-length: {}\r\n\r\n",
        ask.len()
    );
    busy.write_all(head.as_bytes()).unwrap();
    let (sent, rest) = ask.as_bytes().split_at(SENT_BEFORE_SHUTDOWN);
    busy.write_all(sent).unwrap();
    thread::sleep(SETTLE);
    let address = server.address;
    let stopping = thread::spawn(move || server.stop());
    thread::sleep(SETTLE);
    busy.write_all(rest).unwrap();
    let reply = read_reply(&mut busy);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.head.contains("connection: close"), "{}", reply.head);
    stopping.join().unwrap();
    drop(idle);
    assert!(TcpStream::connect(address).is_err(), "still listening");
}
