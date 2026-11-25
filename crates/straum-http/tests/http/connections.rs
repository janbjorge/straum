//! What a client may hold of the server: the connections served at once,
//! and how long one stays open. Each test serves with a small limit and
//! waits on what the socket gives back: a reply, or the connection's end.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use straum_http::ServeLimits;

use crate::fake::{loaded, writer};
use crate::{SERVE_LIMITS, STATUS_CODE, first_hour, read_reply, serve_limited};

/// Fires only if the server stalls: every wait below ends far sooner.
const HANG_GUARD: Duration = Duration::from_secs(20);

/// How many replies the first connection gets while the second waits:
/// a server that served the second would have answered it by then.
const ROUND_TRIPS: usize = 100;

/// One connection served at once.
const ONE_CONNECTION: ServeLimits = ServeLimits {
    max_connections: 1,
    ..SERVE_LIMITS
};

/// `GET /livez`, closing the connection after the reply or not.
fn livez(address: SocketAddr, connection: &str) -> String {
    format!("GET /livez HTTP/1.1\r\nhost: {address}\r\nconnection: {connection}\r\n\r\n")
}

/// Asks `GET /livez` on a connection kept open, and reads the reply by
/// its length: the server does not close it.
fn livez_kept_open(stream: &mut TcpStream, address: SocketAddr) -> u16 {
    stream
        .write_all(livez(address, "keep-alive").as_bytes())
        .unwrap();
    let mut read = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0, "closed mid-reply");
        read.extend_from_slice(&buffer[..count]);
        let text = String::from_utf8_lossy(&read);
        let Some((head, body)) = text.split_once("\r\n\r\n") else {
            continue;
        };
        let length = content_length(head);
        if body.len() >= length {
            return head[STATUS_CODE].parse().unwrap();
        }
    }
}

fn content_length(head: &str) -> usize {
    for line in head.lines() {
        if let Some(length) = line.strip_prefix("content-length: ") {
            return length.parse().unwrap();
        }
    }
    panic!("no content-length: {head}");
}

#[test]
fn a_connection_past_the_limit_waits_until_one_closes() {
    let server = serve_limited(writer().app().clone(), ONE_CONNECTION);
    let mut first = TcpStream::connect(server.address).unwrap();
    assert_eq!(livez_kept_open(&mut first, server.address), 200);

    // The second connects, as the backlog holds it, and asks.
    let mut second = TcpStream::connect(server.address).unwrap();
    second
        .write_all(livez(server.address, "close").as_bytes())
        .unwrap();
    // The first asks again and again after the second: it is answered
    // each time, and the second is not.
    for _ in 0..ROUND_TRIPS {
        assert_eq!(livez_kept_open(&mut first, server.address), 200);
    }
    second.set_nonblocking(true).unwrap();
    let mut byte = [0; 1];
    let unanswered = second.read(&mut byte).unwrap_err();
    assert_eq!(unanswered.kind(), ErrorKind::WouldBlock);

    // Once the first closes, the second is served.
    second.set_nonblocking(false).unwrap();
    second.set_read_timeout(Some(HANG_GUARD)).unwrap();
    drop(first);
    assert_eq!(read_reply(&mut second).status, 200);
    server.stop();
}

#[test]
fn a_stop_does_not_wait_for_a_connection_past_the_limit() {
    let server = serve_limited(writer().app().clone(), ONE_CONNECTION);
    let mut first = TcpStream::connect(server.address).unwrap();
    assert_eq!(livez_kept_open(&mut first, server.address), 200);
    let mut second = TcpStream::connect(server.address).unwrap();
    second
        .write_all(livez(server.address, "close").as_bytes())
        .unwrap();
    second.set_read_timeout(Some(HANG_GUARD)).unwrap();

    let (stopped, stop_done) = mpsc::channel();
    thread::spawn(move || {
        server.stop();
        stopped.send(()).unwrap();
    });
    stop_done.recv_timeout(HANG_GUARD).unwrap();
    // The second was never served, so it gets no reply.
    let mut reply = Vec::new();
    let read = second.read_to_end(&mut reply);
    assert!(read.is_err() || reply.is_empty(), "{read:?}");
}

#[test]
fn a_connection_past_its_deadline_is_closed() {
    let limits = ServeLimits {
        connection_deadline: Duration::from_secs(1),
        connection_grace: Duration::from_secs(1),
        ..SERVE_LIMITS
    };
    let server = serve_limited(writer().app().clone(), limits);
    let mut stalled = TcpStream::connect(server.address).unwrap();
    // A head never finished, so the request outlives the deadline and
    // its grace: hyper alone would wait 30 s for the rest.
    stalled.write_all(b"GET /livez HTTP/1.1\r\n").unwrap();
    stalled.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let mut reply = Vec::new();
    stalled.read_to_end(&mut reply).unwrap();
    assert!(reply.is_empty(), "{}", String::from_utf8_lossy(&reply));
    server.stop();
}

/// Long enough for a test to send a request before it, short enough to
/// wait for.
const SHORT_DEADLINE: Duration = Duration::from_secs(3);

#[test]
fn a_request_in_flight_at_the_deadline_is_answered_whole() {
    let limits = ServeLimits {
        connection_deadline: SHORT_DEADLINE,
        ..SERVE_LIMITS
    };
    let server = serve_limited(loaded().app().clone(), limits);
    let mut busy = TcpStream::connect(server.address).unwrap();
    busy.set_read_timeout(Some(HANG_GUARD)).unwrap();
    // A reply shows that the busy connection's deadline is counting.
    assert_eq!(livez_kept_open(&mut busy, server.address), 200);
    let ask = first_hour();
    let head = format!(
        "POST /ask HTTP/1.1\r\nhost: {}\r\ncontent-type: application/json\r\n\
         content-length: {}\r\n\r\n",
        server.address,
        ask.len()
    );
    busy.write_all(head.as_bytes()).unwrap();
    let (sent, rest) = ask.as_bytes().split_at(ask.len() / 2);
    busy.write_all(sent).unwrap();

    // An idle connection opened later closes at its own deadline, so by
    // then the busy one's deadline has passed too.
    let mut idle = TcpStream::connect(server.address).unwrap();
    idle.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let mut nothing = Vec::new();
    idle.read_to_end(&mut nothing).unwrap();
    assert_eq!(nothing, b"");

    // The ask in flight gets its whole reply, then the connection closes.
    busy.write_all(rest).unwrap();
    let reply = read_reply(&mut busy);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.body.starts_with(r#"{"as_of":"#), "{}", reply.body);
    server.stop();
}
