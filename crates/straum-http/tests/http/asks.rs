//! `POST /ask` end to end: answers as JSON and as a table, asks not sent
//! as JSON, the errors of the body, the query and the app,
//! `Server-Timing`, and a `ReadAsk` other than Straum SQL. Each test starts a server over the fake in `fake.rs`.

use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::num::NonZero;
use std::thread;
use std::time::{Duration, Instant};

use straum_app::Step;
use straum_domain::{Ask, Identifier, Literal};
use straum_http::QueryError;

use crate::fake::{COMMIT, HOUR, JAN_1, loaded, writer};
use crate::{
    Headers, Reply, ask, call, call_as_text, first_hour, header, read_reply, request, request_with,
    rows_of, send, serve, serve_with, started,
};

const BY_BUCKET: &str = "SELECT bucket, kind, borough, trips, fare, fare_avg, fare_max \
    FROM trips WHERE bucket >= $1 AND bucket < $2 GROUP BY bucket, kind, borough";

/// One trip's group as JSON, keyed by the names `BY_BUCKET` selects.
fn json_row(bucket: &str, kind: i64, borough: &str, fare: &str, avg: &str) -> String {
    format!(
        r#"{{"bucket":"{bucket}","kind":{kind},"borough":{borough},"trips":1,"fare":{fare},"fare_avg":{avg},"fare_max":{fare}}}"#
    )
}

#[test]
fn answers_asks_as_json() {
    let (_writer, server) = started(writer());

    let range = r#"["2024-01-01T01:00:00+01:00", "2024-01-03T00:00:00Z"]"#;
    let reply = call(server.address, "POST", "/ask", &ask(BY_BUCKET, range));
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(
        reply.head.contains("content-type: application/json"),
        "{}",
        reply.head
    );
    assert!(
        reply.head.contains("straum-lsn: 16/B374D848"),
        "{}",
        reply.head
    );
    let rows = [
        json_row("2024-01-01T00:00:00Z", 1, r#""Bronx""#, "12.50", "12.5"),
        json_row("2024-01-01T01:00:00Z", 1, r#""Bronx""#, "2.50", "2.5"),
        json_row("2024-01-01T02:00:00Z", 2, "null", "null", "null"),
        json_row(
            "2024-01-01T23:00:00Z",
            2,
            r#""Queens \"Q\"""#,
            "-0.05",
            "-0.05",
        ),
    ];
    assert_eq!(rows_of(&reply.body), format!("[{}]", rows.join(",")));
    server.stop();
}

#[test]
fn a_range_in_unix_seconds_answers_as_one_in_rfc_3339() {
    let (_writer, server) = started(writer());
    let range = r#"["2024-01-01T00:00:00Z", "2024-01-03T00:00:00Z"]"#;
    let reply = call(server.address, "POST", "/ask", &ask(BY_BUCKET, range));
    let seconds = ask(BY_BUCKET, "[1704067200, 1704240000]");
    let again = call(server.address, "POST", "/ask", &seconds);
    assert_eq!(rows_of(&again.body), rows_of(&reply.body));
    let later = ask(BY_BUCKET, "[1704070800, 1704240000]");
    let fewer = call(server.address, "POST", "/ask", &later);
    assert_ne!(rows_of(&fewer.body), rows_of(&reply.body));
    server.stop();
}

#[test]
fn answers_after_a_commit_carry_its_lsn() {
    let (mut writer, server) = started(writer());
    assert_eq!(writer.step().unwrap(), Step::Applied(COMMIT));
    let totals = ask(
        "SELECT trips, fare, fare_avg FROM trips WHERE bucket >= $1 AND bucket < $2",
        "[0, 1704240000]",
    );
    let reply = call(server.address, "POST", "/ask", &totals);
    assert!(reply.head.contains("straum-lsn: 17/0"), "{}", reply.head);
    // 15.95 over the 4 trips with a fare, to four places past its scale.
    assert_eq!(
        rows_of(&reply.body),
        r#"[{"trips":5,"fare":15.95,"fare_avg":3.9875}]"#
    );
    server.stop();
}

#[test]
fn answers_asks_as_a_table_to_those_who_accept_text() {
    let (_writer, server) = started(writer());

    let range = r#"["2024-01-01T00:00:00Z", "2024-01-03T00:00:00Z"]"#;
    let reply = call_as_text(server.address, &ask(BY_BUCKET, range));
    assert_eq!(reply.status, 200, "{}", reply.body);
    for header in [
        "content-type: text/plain; charset=utf-8",
        "straum-lsn: 16/B374D848",
    ] {
        assert!(reply.head.contains(header), "{}", reply.head);
    }
    let phases = server_timing(&reply.head).unwrap();
    assert_eq!(phase_names(&phases), ["parse", "answer", "text", "total"]);
    let (table, as_of) = reply.body.split_at(reply.body.find("4 rows").unwrap());
    assert_eq!(
        table,
        "\
bucket               | kind | borough    | trips | fare  | fare_avg | fare_max
---------------------+------+------------+-------+-------+----------+---------
2024-01-01T00:00:00Z |    1 | Bronx      |     1 | 12.50 |     12.5 |    12.50
2024-01-01T01:00:00Z |    1 | Bronx      |     1 |  2.50 |      2.5 |     2.50
2024-01-01T02:00:00Z |    2 | null       |     1 | null  | null     | null
2024-01-01T23:00:00Z |    2 | Queens \"Q\" |     1 | -0.05 |    -0.05 |    -0.05
"
    );
    assert!(
        as_of.starts_with("4 rows, as of 2") && as_of.ends_with("Z\n"),
        "{as_of}"
    );

    server.stop();
}

#[test]
fn a_table_of_one_row_says_so_and_any_other_accept_gets_json() {
    let (_writer, server) = started(writer());
    let reply = call_as_text(server.address, &first_hour());
    assert!(
        reply.body.starts_with("trips\n-----\n    0\n1 row, as of "),
        "{}",
        reply.body
    );
    let reply = request(server.address, "POST", "/ask", Some("*/*"), &first_hour());
    assert_eq!(rows_of(&reply.body), r#"[{"trips":0}]"#);
    server.stop();
}

/// The content type of a 200 reply.
fn content_type(address: SocketAddr, method: &str, path: &str, accept: Option<&str>) -> String {
    let body = if method == "POST" {
        first_hour()
    } else {
        String::new()
    };
    let reply = request(address, method, path, accept, &body);
    assert_eq!(reply.status, 200, "{}", reply.body);
    header(&reply.head, "content-type").unwrap()
}

#[test]
fn accept_picks_the_format_and_each_route_its_default() {
    let (_writer, server) = started(writer());
    let json = "application/json";
    let text = "text/plain; charset=utf-8";
    for (accept, ask, views) in [
        (Some("application/json"), json, json),
        (Some("text/plain"), text, text),
        (Some("application/json; charset=utf-8"), json, json),
        (Some("Application/JSON"), json, json),
        (Some("text/html, TEXT/plain;q=0.9, */*;q=0.1"), text, text),
        (Some("text/html"), json, text),
        (Some("*/*"), json, text),
        (None, json, text),
    ] {
        let got = content_type(server.address, "POST", "/ask", accept);
        assert_eq!(got, ask, "/ask {accept:?}");
        let got = content_type(server.address, "GET", "/views", accept);
        assert_eq!(got, views, "/views {accept:?}");
        // /status is for people first, as /views is.
        let got = content_type(server.address, "GET", "/status", accept);
        assert_eq!(got, views, "/status {accept:?}");
    }
    server.stop();
}

#[test]
fn errors_are_lines_to_those_who_accept_text() {
    let mut writer = writer();
    let server = serve(writer.app().clone());
    let reply = call_as_text(server.address, &first_hour());
    assert_eq!((reply.status, reply.body.as_str()), (503, "not ready\n"));

    writer.step().unwrap();
    let twice = ask(
        "SELECT kind, kind, trips, trips FROM trips \
         WHERE bucket >= $1 AND bucket < $2 GROUP BY kind",
        "[0, 3600]",
    );
    let cases = [
        (twice, "kind asked twice\ntrips asked twice\n"),
        (
            "{".to_owned(),
            "EOF while parsing an object at line 1 column 1\n",
        ),
    ];
    for (body, errors) in cases {
        let reply = call_as_text(server.address, &body);
        assert_eq!((reply.status, reply.body.as_str()), (400, errors), "{body}");
        assert!(
            reply.head.contains("content-type: text/plain"),
            "{}",
            reply.head
        );
    }
    server.stop();
}

/// An ask without WHERE reads every bucket held, as does one whose bounds
/// lie past both ends of them; a bound still cuts its side.
#[test]
fn an_ask_without_bounds_reads_every_bucket_held() {
    let (_writer, server) = started(writer());
    let bounded = ask(BY_BUCKET, "[0, 4102444800]");
    let every = call(server.address, "POST", "/ask", &bounded);
    let open = BY_BUCKET.replace("WHERE bucket >= $1 AND bucket < $2 ", "");
    let reply = call(server.address, "POST", "/ask", &ask(&open, "[]"));
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(rows_of(&reply.body), rows_of(&every.body));
    let later = BY_BUCKET.replace(" AND bucket < $2", "");
    let fewer = call(server.address, "POST", "/ask", &ask(&later, "[1704070800]"));
    assert_eq!(fewer.status, 200, "{}", fewer.body);
    assert_ne!(rows_of(&fewer.body), rows_of(&every.body));
    assert!(rows_of(&fewer.body).starts_with(r#"[{"bucket":"2024-01-01T01:00:00Z""#));
    server.stop();
}

/// Asks the view can't answer, each with the errors its reply holds.
fn unanswerable() -> [(String, &'static str); 6] {
    [
        (
            ask(
                "SELECT zone_nope, nope FROM trips \
                 WHERE bucket >= $1 AND bucket < $2 GROUP BY zone_nope",
                "[0, 1]",
            ),
            concat!(
                r#"{"errors":["unknown measure nope; the view's measures are "#,
                r#"trips, fare, fare_avg, fare_max","#,
                r#""zone_nope is not a key of the view; the view's keys are "#,
                r#"bucket, kind, zone_id, borough","#,
                r#""bound 1 is not on a 3600s bucket edge"]}"#
            ),
        ),
        (
            ask(
                "SELECT trips FROM trips WHERE kind = 1 AND bucket >= $1 AND bucket < $2",
                "[0, 3600]",
            ),
            concat!(
                r#"{"errors":["line 1: value, use a parameter such as $1, `1` is not in "#,
                r#"Straum SQL, the PostgreSQL subset made only for Straum"]}"#
            ),
        ),
        (
            ask(
                "SELECT kind, kind, trips, trips FROM trips \
                 WHERE bucket >= $1 AND bucket < $2 GROUP BY kind",
                "[0, 3600]",
            ),
            r#"{"errors":["kind asked twice","trips asked twice"]}"#,
        ),
        (
            ask(
                "SELECT trips FROM trips WHERE kind = $1 AND borough = $2 \
                 AND bucket >= $3 AND bucket < $4",
                r#"["1", 2, 0, 3600]"#,
            ),
            concat!(
                r#"{"errors":["kind is an integer, so it can't be the text \"1\"","#,
                r#""borough is text, so it can't be the integer 2"]}"#
            ),
        ),
        (
            first_hour().replace('}', r#", "limit": 1}"#),
            r#"{"errors":["unknown field \"limit\""]}"#,
        ),
        (
            r#"{"query": "SELECT", "query": "SELECT"}"#.to_owned(),
            r#"{"errors":["key \"query\" given twice at line 1 column 27"]}"#,
        ),
    ]
}

#[test]
fn asks_straum_cant_answer() {
    let mut writer = writer();
    let server = serve(writer.app().clone());
    let reply = call(server.address, "POST", "/ask", &first_hour());
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (503, r#"{"errors":["not ready"]}"#)
    );

    writer.step().unwrap();
    for (body, errors) in unanswerable() {
        let reply = call(server.address, "POST", "/ask", &body);
        assert_eq!((reply.status, reply.body.as_str()), (400, errors), "{body}");
    }
    server.stop();
}

/// A body that stops coming is answered 408 once it is 10 s late, and
/// the connection closes; one that comes whole in time is read.
#[test]
fn a_stalled_body_times_out() {
    let server = serve(writer().app().clone());
    let mut stream = TcpStream::connect(server.address).unwrap();
    let head = "POST /ask HTTP/1.1\r\nhost: straum\r\ncontent-type: application/json\r\n\
        content-length: 100\r\n\r\n";
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(br#"{"query":"#).unwrap();
    let clock = Instant::now();
    let reply = read_reply(&mut stream);
    let waited = clock.elapsed();
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (
            408,
            r#"{"errors":["the ask's body did not come whole within 10 s"]}"#
        )
    );
    assert!(
        waited >= Duration::from_secs(9) && waited < Duration::from_secs(20),
        "{waited:?}"
    );
    server.stop();
}

#[test]
fn ask_bodies_too_big_or_broken_are_refused() {
    let server = serve(writer().app().clone());
    let big = format!("{{\"query\": \"{}\"}}", "a".repeat(1 << 20));
    let reply = call(server.address, "POST", "/ask", &big);
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (413, r#"{"errors":["an ask is at most 1048576 bytes"]}"#)
    );

    // A chunked body that breaks off.
    let reply = send(
        server.address,
        "POST /ask HTTP/1.1\r\nhost: straum\r\ncontent-type: application/json\r\n\
         transfer-encoding: chunked\r\n\r\nzz\r\n",
    );
    assert_eq!(reply.status, 400);
    assert!(
        reply.body.starts_with(r#"{"errors":["reading the ask: "#),
        "{}",
        reply.body
    );
    server.stop();
}

/// An ask sent without `Content-Type`, or with `content_type`, and read
/// back in `accept`.
fn ask_sent_as(address: SocketAddr, content_type: Option<&str>, accept: Option<&str>) -> Reply {
    let headers = Headers {
        accept,
        content_type,
    };
    request_with(address, "POST", "/ask", &headers, &first_hour())
}

/// A browser sends a form or plain text to any site without asking it
/// first, so an ask in anything but JSON is refused before it is read.
#[test]
fn asks_not_sent_as_json_are_refused() {
    let (_writer, server) = started(writer());
    let refused = r#"{"errors":["an ask is JSON, sent with Content-Type: application/json"]}"#;
    for content_type in [
        None,
        Some("text/plain"),
        Some("application/x-www-form-urlencoded"),
        Some("multipart/form-data; boundary=x"),
        Some("application/jsonx"),
    ] {
        let reply = ask_sent_as(server.address, content_type, None);
        assert_eq!(
            (reply.status, reply.body.as_str()),
            (415, refused),
            "{content_type:?}"
        );
    }
    let reply = ask_sent_as(server.address, Some("text/plain"), Some("text/plain"));
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (
            415,
            "an ask is JSON, sent with Content-Type: application/json\n"
        )
    );
    for content_type in ["application/json", "Application/JSON; charset=utf-8"] {
        let reply = ask_sent_as(server.address, Some(content_type), None);
        assert_eq!(reply.status, 200, "{content_type}: {}", reply.body);
        assert_eq!(rows_of(&reply.body), r#"[{"trips":0}]"#);
    }
    server.stop();
}

/// One phase of `Server-Timing`.
#[derive(Debug, PartialEq)]
struct TimingPhase {
    name: String,
    took_ms: f64,
}

/// The phases of `Server-Timing`, if the reply has one.
fn server_timing(head: &str) -> Option<Vec<TimingPhase>> {
    let value = header(head, "server-timing")?;
    let mut phases = Vec::new();
    for phase in value.split(", ") {
        let (name, took_ms) = phase.split_once(";dur=").unwrap();
        phases.push(TimingPhase {
            name: name.to_owned(),
            took_ms: took_ms.parse().unwrap(),
        });
    }
    Some(phases)
}

fn phase_names(phases: &[TimingPhase]) -> Vec<&str> {
    let mut names = Vec::with_capacity(phases.len());
    for phase in phases {
        names.push(phase.name.as_str());
    }
    names
}

#[test]
fn says_where_the_time_went() {
    let mut writer = writer();
    let server = serve(writer.app().clone());
    let reply = call(server.address, "POST", "/ask", &first_hour());
    assert_eq!(reply.status, 503);
    assert_eq!(server_timing(&reply.head), None, "{}", reply.head);

    writer.step().unwrap();
    let reply = call(server.address, "POST", "/ask", &first_hour());
    assert_eq!(reply.status, 200, "{}", reply.body);
    let phases = server_timing(&reply.head).expect("a Server-Timing header");
    assert_eq!(phase_names(&phases), ["parse", "answer", "json", "total"]);
    let total = phases[3].took_ms;
    for TimingPhase { name, took_ms } in &phases {
        assert!(
            (0.0..=total).contains(took_ms),
            "{name} took {took_ms} ms of {total}"
        );
    }

    let reply = call(server.address, "POST", "/ask", &ask("SELECT", "[]"));
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert_eq!(server_timing(&reply.head), None, "{}", reply.head);
    server.stop();
}

fn trips_from(from: i64) -> Ask {
    Ask {
        view: Identifier::new("trips").unwrap(),
        measures: vec![Identifier::new("trips").unwrap()],
        keys: Vec::new(),
        filter: Vec::new(),
        having: Vec::new(),
        from: Some(from),
        to: Some(JAN_1 + 24 * HOUR),
    }
}

/// A query no reader in Straum reads, for readers that ignore it.
const ANY_QUERY: &str = r#"{"query": "not an ask in any syntax"}"#;

#[expect(clippy::unnecessary_wraps, reason = "a ReadAsk returns a Result")]
fn read_every_trip(_: &str, _: &[Literal]) -> Result<Ask, Vec<QueryError>> {
    Ok(trips_from(JAN_1))
}

#[expect(clippy::unnecessary_wraps, reason = "a ReadAsk returns a Result")]
fn read_past_jiff(_: &str, _: &[Literal]) -> Result<Ask, Vec<QueryError>> {
    Ok(trips_from(i64::MIN))
}

/// Reads every ask as all trips, taking a second as a long ask does.
#[expect(clippy::unnecessary_wraps, reason = "a ReadAsk returns a Result")]
fn read_slowly(_: &str, _: &[Literal]) -> Result<Ask, Vec<QueryError>> {
    thread::sleep(Duration::from_secs(1));
    Ok(trips_from(JAN_1))
}

/// Asks read off the async threads: with one being read on each core,
/// `/livez` still answers at once.
#[test]
fn reading_asks_leaves_the_health_routes_free() {
    let writer = loaded();
    let server = serve_with(writer.app().clone(), read_slowly);
    let address = server.address;
    let cores = thread::available_parallelism().map_or(4, NonZero::get);
    let mut asks = Vec::new();
    for _ in 0..cores {
        asks.push(thread::spawn(move || {
            call(address, "POST", "/ask", ANY_QUERY)
        }));
    }
    thread::sleep(Duration::from_millis(200));
    let clock = Instant::now();
    assert_eq!(call(address, "GET", "/livez", "").status, 200);
    let waited = clock.elapsed();
    assert!(waited < Duration::from_millis(500), "{waited:?}");
    for ask in asks {
        assert_eq!(ask.join().unwrap().status, 200);
    }
    server.stop();
}

fn query_error(line: Option<u64>, message: &str) -> QueryError {
    QueryError {
        line,
        message: message.to_owned(),
    }
}

fn refuse_twice(_: &str, _: &[Literal]) -> Result<Ask, Vec<QueryError>> {
    Err(vec![query_error(None, "no"), query_error(Some(2), "never")])
}

fn refuse_with_quotes(_: &str, _: &[Literal]) -> Result<Ask, Vec<QueryError>> {
    let message = "a \"quote\", a \\ and\na line in Zürich";
    Err(vec![query_error(None, message)])
}

#[test]
fn reads_asks_with_the_reader_it_is_given() {
    let writer = loaded();
    let server = serve_with(writer.app().clone(), read_every_trip);
    let reply = call(server.address, "POST", "/ask", ANY_QUERY);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(rows_of(&reply.body), r#"[{"trips":4}]"#);
    server.stop();
}

#[test]
fn a_readers_errors_are_the_replys() {
    let writer = loaded();
    let server = serve_with(writer.app().clone(), refuse_twice);
    let reply = call(server.address, "POST", "/ask", ANY_QUERY);
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (400, r#"{"errors":["no","line 2: never"]}"#)
    );
    server.stop();

    let server = serve_with(writer.app().clone(), refuse_with_quotes);
    let reply = call(server.address, "POST", "/ask", ANY_QUERY);
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (
            400,
            r#"{"errors":["a \"quote\", a \\ and\na line in Zürich"]}"#
        )
    );
    server.stop();
}

/// A reader that lets through a range jiff can't print gets a 400, not a
/// panic while the answer is written.
#[test]
fn a_range_past_jiff_from_a_reader_is_refused() {
    let writer = loaded();
    let server = serve_with(writer.app().clone(), read_past_jiff);
    let reply = call(server.address, "POST", "/ask", ANY_QUERY);
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert!(
        reply
            .body
            .starts_with(r#"{"errors":["range start -9223372036854775808 is not between"#),
        "{}",
        reply.body
    );
    server.stop();
}
