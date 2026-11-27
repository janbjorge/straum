//! The routes besides asks and views, end to end: `/livez`, `/readyz`,
//! `/status`, HEAD on each GET route, the replies to a wrong method or
//! path, caching, the health listener, and a shutdown with requests still
//! in flight.

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::Duration;

use straum_app::{Limits, Lsn, Memory};

use crate::fake::{LIMITS, START, fake, writer, writer_with};
use crate::{
    JSON, SERVE_LIMITS, call, first_hour, header, read_reply, request, serve, serve_with_health,
    started,
};

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
        ("HEAD", "/ask", "POST"),
        ("POST", "/livez", "GET, HEAD"),
        ("DELETE", "/readyz", "GET, HEAD"),
        ("POST", "/status", "GET, HEAD"),
        ("PUT", "/views", "GET, HEAD"),
        ("POST", "/views/trips", "GET, HEAD"),
        ("POST", "/openapi.json", "GET, HEAD"),
    ] {
        let reply = call(server.address, method, path, "");
        assert_eq!(reply.status, 405, "{method} {path}");
        assert!(
            reply.head.contains(&format!("allow: {allow}")),
            "{}",
            reply.head
        );
        let body = if method == "HEAD" {
            String::new()
        } else {
            format!("use {allow}\n")
        };
        assert_eq!(reply.body, body);
    }
    server.stop();
}

/// The health listener answers `/livez`, `/readyz` and `/status`, gives
/// 404 to every other route, and stops with the server. The main listener
/// still serves every route.
#[test]
fn the_health_listener_serves_health_checks_and_status_alone() {
    let (server, health) = serve_with_health(writer().app().clone(), SERVE_LIMITS);
    let reply = call(health, "GET", "/livez", "");
    assert_eq!((reply.status, reply.body.as_str()), (200, "live\n"));
    let reply = call(health, "GET", "/readyz", "");
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (503, "not ready: loading\n")
    );
    assert_eq!(call(health, "GET", "/status", "").status, 200);
    assert_eq!(call(health, "POST", "/livez", "").status, 405);
    for (method, path) in [
        ("POST", "/ask"),
        ("GET", "/views"),
        ("GET", "/views/trips/spec"),
        ("GET", "/openapi.json"),
    ] {
        let reply = call(health, method, path, "");
        let expected = (404, format!("no {path} here\n"));
        assert_eq!((reply.status, reply.body), expected, "{method} {path}");
    }
    assert_eq!(call(server.address, "GET", "/status", "").status, 200);
    assert_eq!(call(server.address, "GET", "/livez", "").status, 200);
    server.stop();
    assert!(TcpStream::connect(health).is_err(), "still listening");
}

/// The health check of Compose and ECS is `straum status`, which reads
/// the `straum-ready` header of `GET /status`. On the health listener, it
/// finds a loaded Straum ready, so a full main listener cannot fail it.
#[test]
fn the_health_listener_says_in_status_that_straum_is_ready() {
    let mut writer = writer();
    let (server, health) = serve_with_health(writer.app().clone(), SERVE_LIMITS);
    writer.step().unwrap();
    let reply = call(health, "GET", "/status", "");
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(header(&reply.head, "straum-ready"), Some("true".to_owned()));
    server.stop();
}

/// HEAD on a GET route gets the head GET gets, and no body. The server
/// has not loaded, so each reply stays the same between the two.
#[test]
fn head_gets_the_head_of_get() {
    let server = serve(writer().app().clone());
    for path in [
        "/livez",
        "/readyz",
        "/status",
        "/openapi.json",
        "/views",
        "/views/trips/spec",
    ] {
        let get = call(server.address, "GET", path, "");
        let head = call(server.address, "HEAD", path, "");
        assert_eq!(head.status, get.status, "{path}");
        for name in ["content-type", "content-length"] {
            let value = header(&get.head, name);
            assert!(value.is_some(), "{path}: {}", get.head);
            assert_eq!(header(&head.head, name), value, "{path} {name}");
        }
        assert_eq!(head.body, "", "{path}");
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
applying:     unknown until every table is loaded
WAL retained: unknown until every table is loaded
memory:       unknown; this system has neither /proc/self/status nor ps
"
        .to_owned()
            + &format!("version:      {}\n", straum_http::VERSION)
    );
    server.stop();
}

#[test]
fn status_says_what_the_stream_applied() {
    let (mut writer, server) = started(writer());
    writer.step().unwrap();
    writer.app().memory_readings().record(Memory {
        resident_bytes: 3 << 20,
        limit_bytes: Some(1 << 30),
    });
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
    assert!(
        lines[3].ends_with("; caught up to 16/B374D848"),
        "{}",
        lines[3]
    );
    assert_eq!(
        lines[4],
        "applying:     unknown for the stream's first second"
    );
    assert_eq!(
        lines[5],
        "WAL retained: 0.0 MiB, at most 4096.0 MiB; PostgreSQL is at 16/B374D848"
    );
    assert_eq!(
        lines[6],
        "memory:       3.0 MiB resident, at most 1024.0 MiB"
    );
    assert_eq!(lines[7], format!("version:      {}", straum_http::VERSION));
    assert_eq!(lines.len(), 8);
    server.stop();
}

/// The `straum-ready` header of `GET /status` as text and as JSON.
fn ready_headers(address: SocketAddr) -> [Option<String>; 2] {
    let as_text = call(address, "GET", "/status", "");
    let as_json = request(address, "GET", "/status", Some(JSON), "");
    [
        header(&as_text.head, "straum-ready"),
        header(&as_json.head, "straum-ready"),
    ]
}

#[test]
fn status_says_whether_ready_in_a_header() {
    let mut writer = writer();
    let server = serve(writer.app().clone());
    let not_ready = Some("false".to_owned());
    assert_eq!(
        ready_headers(server.address),
        [not_ready.clone(), not_ready]
    );
    writer.step().unwrap();
    let ready = Some("true".to_owned());
    assert_eq!(ready_headers(server.address), [ready.clone(), ready]);
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
    // WAL written while the load took a second, which Straum hasn't
    // applied.
    let mut fake = fake();
    fake.wal.written = Lsn(START.0 + 1);
    fake.pause = Duration::from_secs(1);
    let limits = Limits {
        max_lag: Duration::ZERO,
        ..LIMITS
    };
    let (_writer, server) = started(writer_with(limits, fake));
    let reply = call(server.address, "GET", "/readyz", "");
    assert_eq!(reply.status, 503);
    assert!(reply.body.starts_with("not ready: 1."), "{}", reply.body);
    assert!(
        reply.body.ends_with(" s behind PostgreSQL\n"),
        "{}",
        reply.body
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
        "POST /ask HTTP/1.1\r\nhost: {}\r\ncontent-type: application/json\r\n\
         content-length: {}\r\n\r\n",
        server.address,
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
