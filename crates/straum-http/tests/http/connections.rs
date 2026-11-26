//! What a client may hold of the server: the connections served at once,
//! how long one stays open, and the bytes of answers that clients have not
//! read. Each test serves with a small limit and
//! waits on what the socket gives back: a reply, or the connection's end.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use straum_app::Writer;
use straum_http::{HEADER_TIMEOUT, ServeLimits};

use crate::fake::{Fake, LIMITS, fake, int, loaded, trip, writer, writer_with};
use crate::{
    SERVE_LIMITS, STATUS_CODE, first_hour, read_reply, send, serve_limited, serve_with_health,
};

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

/// The `timeoutSeconds` of the chart's probes.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// With every connection of the main listener held, the main listener
/// answers no more, and the health listener still answers a probe within
/// its timeout.
#[test]
fn a_health_check_answers_while_the_main_listener_is_full() {
    let (server, health) = serve_with_health(writer().app().clone(), ONE_CONNECTION);
    let mut held = TcpStream::connect(server.address).unwrap();
    assert_eq!(livez_kept_open(&mut held, server.address), 200);
    let mut waiting = TcpStream::connect(server.address).unwrap();
    waiting
        .write_all(livez(server.address, "close").as_bytes())
        .unwrap();

    let mut probe = TcpStream::connect(health).unwrap();
    probe.set_read_timeout(Some(PROBE_TIMEOUT)).unwrap();
    probe.write_all(livez(health, "close").as_bytes()).unwrap();
    assert_eq!(read_reply(&mut probe).status, 200);

    waiting.set_nonblocking(true).unwrap();
    let mut byte = [0; 1];
    let unanswered = waiting.read(&mut byte).unwrap_err();
    assert_eq!(unanswered.kind(), ErrorKind::WouldBlock);
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
    // its grace: the header timeout alone would wait 5 s for the rest.
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

/// How much later the idle connection's deadline comes than the busy one's.
const DEADLINE_GAP: Duration = Duration::from_millis(200);

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
    // then the busy one's deadline has passed too. The gap keeps the two
    // deadlines apart, so the busy one is closing before the rest comes.
    thread::sleep(DEADLINE_GAP);
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
    // hyper says it closes only when the deadline came before the reply.
    assert!(reply.head.contains("connection: close"), "{}", reply.head);
    server.stop();
}

/// Two connections fill the server.
const TWO_CONNECTIONS: ServeLimits = ServeLimits {
    max_connections: 2,
    ..SERVE_LIMITS
};

/// How long after the header timeout a cut may come.
const CUT_MARGIN: Duration = Duration::from_secs(1);

/// Reads until the server closes `stream`, and says when that was.
fn closed_after(stream: &mut TcpStream, opened: Instant) -> Duration {
    stream.set_read_timeout(Some(HANG_GUARD)).unwrap();
    let mut rest = Vec::new();
    stream.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty(), "{}", String::from_utf8_lossy(&rest));
    opened.elapsed()
}

/// One connection has sent nothing, and one has had a reply and sent
/// nothing since. Both are cut, and the connection that waits in the
/// backlog is served.
#[test]
fn a_connection_that_sends_no_head_is_cut() {
    let server = serve_limited(writer().app().clone(), TWO_CONNECTIONS);
    let opened = Instant::now();
    let mut silent = TcpStream::connect(server.address).unwrap();
    let mut kept_open = TcpStream::connect(server.address).unwrap();
    assert_eq!(livez_kept_open(&mut kept_open, server.address), 200);
    let mut third = TcpStream::connect(server.address).unwrap();
    third
        .write_all(livez(server.address, "close").as_bytes())
        .unwrap();
    third.set_read_timeout(Some(HANG_GUARD)).unwrap();
    assert_eq!(read_reply(&mut third).status, 200);
    let answered = opened.elapsed();
    let cut = [
        closed_after(&mut silent, opened),
        closed_after(&mut kept_open, opened),
    ];
    let limit = HEADER_TIMEOUT + CUT_MARGIN;
    assert!(
        answered >= HEADER_TIMEOUT && answered < limit,
        "{answered:?}"
    );
    assert!(cut[0] < limit && cut[1] < limit, "{cut:?}");
    server.stop();
}

/// How long a slow client waits to send its head: 1 s short of
/// [`HEADER_TIMEOUT`].
const IN_TIME: Duration = Duration::from_secs(4);

/// A connection that sends its head before the header timeout is served.
#[test]
fn a_connection_that_sends_a_head_in_time_is_served() {
    let server = serve_limited(writer().app().clone(), TWO_CONNECTIONS);
    let mut slow = TcpStream::connect(server.address).unwrap();
    slow.set_read_timeout(Some(HANG_GUARD)).unwrap();
    thread::sleep(IN_TIME);
    assert_eq!(livez_kept_open(&mut slow, server.address), 200);
    server.stop();
}

/// Kinds of trip in each hour. Each hour and kind is a row of its own,
/// so a day of them is an answer of some megabytes. That is far past
/// what the two sockets buffer, so a client that does not read it leaves
/// most of it in the server.
const KINDS: i64 = 4_000;

/// Every hour and kind of 2024-01-01.
const EVERY_ROW: &str = r#"{"query": "SELECT bucket, kind, zone_id, trips, fare FROM trips WHERE bucket >= $1 AND bucket < $2 GROUP BY bucket, kind, zone_id", "parameters": ["2024-01-01T00:00:00Z", "2024-01-02T00:00:00Z"]}"#;

/// A trip for each hour of 2024-01-01 and each of [`KINDS`] kinds, loaded.
fn many_rows() -> Writer<Fake> {
    let mut trips = Vec::new();
    for hour in 0..24 {
        for kind in 0..KINDS {
            trips.push(trip(hour, kind, 1, int(100)));
        }
    }
    let mut writer = writer_with(LIMITS, Fake { trips, ..fake() });
    writer.step().unwrap();
    writer
}

/// `POST /ask` of [`EVERY_ROW`], closing the connection after the reply.
fn ask_every_row(address: SocketAddr) -> String {
    format!(
        "POST /ask HTTP/1.1\r\nhost: {address}\r\nconnection: close\r\n\
         content-type: application/json\r\ncontent-length: {}\r\n\r\n{EVERY_ROW}",
        EVERY_ROW.len()
    )
}

/// Reads a reply's head and no more of its body than comes with it; its
/// status and its body's length.
fn head_only(stream: &mut TcpStream) -> (u16, usize) {
    let mut read = Vec::new();
    let mut buffer = [0; 1024];
    loop {
        let count = stream.read(&mut buffer).unwrap();
        assert!(count > 0, "closed in the head");
        read.extend_from_slice(&buffer[..count]);
        let text = String::from_utf8_lossy(&read);
        if let Some((head, _)) = text.split_once("\r\n\r\n") {
            return (head[STATUS_CODE].parse().unwrap(), content_length(head));
        }
    }
}

/// The limits with `bytes` for the answers that clients have not read.
fn answer_budget(bytes: usize) -> ServeLimits {
    ServeLimits {
        max_answer_bytes: bytes,
        ..SERVE_LIMITS
    }
}

/// A client that does not read its answer holds it in the server. With a
/// budget of one and a half answers, a second ask gets 503 while the
/// first is unread, and 200 once the first is read.
#[test]
fn an_ask_past_the_bytes_of_unread_answers_is_refused() {
    let writer = many_rows();
    // A first server tells about how long the answer is: its `as_of`
    // can change the length by a few bytes.
    let probe = serve_limited(writer.app().clone(), SERVE_LIMITS);
    let reply = send(probe.address, &ask_every_row(probe.address));
    assert_eq!(reply.status, 200, "{}", reply.body);
    let budget = reply.body.len() * 3 / 2;
    probe.stop();

    let server = serve_limited(writer.app().clone(), answer_budget(budget));
    let mut unread = TcpStream::connect(server.address).unwrap();
    unread.set_read_timeout(Some(HANG_GUARD)).unwrap();
    unread
        .write_all(ask_every_row(server.address).as_bytes())
        .unwrap();
    let (status, length) = head_only(&mut unread);
    assert_eq!(status, 200);

    let refused = send(server.address, &ask_every_row(server.address));
    assert_eq!(
        refused.status,
        503,
        "the server holds an unread answer of {length} bytes of a budget of \
         {budget}, and still answered another of {} bytes",
        refused.body.len()
    );
    // Its own answer's length can differ from the first's by a few bytes.
    let error = "busy: unread answers hold the memory that this answer of ";
    assert!(
        refused
            .body
            .starts_with(&format!("{{\"errors\":[\"{error}"))
            && refused
                .body
                .ends_with(" bytes needs; ask again shortly, or narrow the ask\"]}"),
        "{}",
        refused.body
    );

    // The server closes the first once it sent all of it, and its bytes
    // are free again.
    let mut rest = Vec::new();
    unread.read_to_end(&mut rest).unwrap();
    let answered = send(server.address, &ask_every_row(server.address));
    assert_eq!(answered.status, 200, "{}", answered.body);
    server.stop();
}
