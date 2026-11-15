//! The HTTP adapter over a real socket: a scripted source fills the app,
//! and requests go out as raw HTTP/1.1.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::ops::Range;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, UNIX_EPOCH};

use jiff::{SignedDuration, Timestamp};
use straum_app::{
    App, Change, Commit, Limits, Lsn, Source, SourceError, Spill, SpillError, Spilling, Step, Wal,
    Writer,
};
use straum_domain::{Ask, ColumnType, Datum, Identifier, Sealed, SealedId, Spec, Store};
use straum_http::ReadAsk;
use tokio::sync::oneshot;

const SPEC: &str = "
CREATE VIEW trips WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket,
       t.kind AS kind,
       t.zone_id AS zone_id,
       zone.borough AS borough,
       count(*) AS trips,
       sum(t.fare) AS fare,
       avg(t.fare) AS fare_avg,
       max(t.fare) AS fare_max
FROM public.trips AS t
LEFT OUTER JOIN public.zones AS zone ON zone.id = t.zone_id
GROUP BY 1, 2, 3, 4;
";

/// 2024-01-01 00:00 UTC.
const JAN_1: i64 = 1_704_067_200;
const HOUR: i64 = 3_600;
const START: Lsn = Lsn(0x16_B374_D848);
/// The LSN of the one commit the fake streams.
const COMMIT: Lsn = Lsn(0x17_0000_0000);
const LIMITS: Limits = Limits {
    max_lag: Duration::from_secs(30),
    max_retained_wal: 4 << 30,
};

/// Loads fixed zones and trips at [`START`], then streams its commits.
struct Fake {
    trips: Vec<Vec<Datum>>,
    commits: VecDeque<Commit>,
    /// How long loading the fact takes.
    pause: Duration,
    wal: Wal,
}

impl Source for Fake {
    fn open(&mut self, _: &Spec) -> Result<Lsn, SourceError> {
        Ok(START)
    }

    /// Zones 1 and 2; a trip in zone 3 finds no row.
    fn load(&mut self, table: &Identifier, store: &mut Store) -> Result<(), SourceError> {
        if table.as_str() == "zones" {
            store.set_types("zones", &[ColumnType::Int, ColumnType::Text])?;
            return Ok(store.add_rows(
                "zones",
                &[int(1), text("Bronx"), int(2), text("Queens \"Q\"")],
            )?);
        }
        thread::sleep(self.pause);
        // at, kind, zone_id, fare.
        store.set_types(
            "trips",
            &[
                ColumnType::Time { zoned: true },
                ColumnType::Int,
                ColumnType::Int,
                ColumnType::Numeric { scale: 2 },
            ],
        )?;
        for row in &self.trips {
            store.add_row("trips", row)?;
        }
        Ok(())
    }

    fn next_commit(&mut self, _: Duration) -> Result<Option<Commit>, SourceError> {
        let commit = self.commits.pop_front();
        commit.map(Some).ok_or_else(|| "connection dropped".into())
    }

    fn wal(&mut self) -> Result<Wal, SourceError> {
        Ok(self.wal)
    }
}

fn int(n: i64) -> Datum {
    Datum::Int(n)
}

fn text(text: &str) -> Datum {
    Datum::Text(text.into())
}

fn trip(hour: i64, kind: i64, zone: i64, fare: Datum) -> Vec<Datum> {
    vec![int(JAN_1 + hour * HOUR), int(kind), int(zone), fare]
}

/// Four trips on 2024-01-01 UTC, one in a zone with no row, and a slot
/// that has confirmed all there is.
fn fake() -> Fake {
    Fake {
        trips: vec![
            trip(0, 1, 1, int(1_250)),
            trip(1, 1, 1, int(250)),
            trip(2, 2, 3, Datum::Null),
            trip(23, 2, 2, int(-5)),
        ],
        commits: VecDeque::from([Commit {
            lsn: COMMIT,
            committed: UNIX_EPOCH + Duration::from_micros(1_704_153_600_123_456),
            changes: vec![Change::Insert {
                table: Identifier::new("trips").unwrap(),
                row: trip(3, 1, 1, int(100)),
            }],
        }]),
        pause: Duration::ZERO,
        wal: Wal {
            written: START,
            confirmed: START,
            restart: START,
        },
    }
}

fn writer_with(limits: Limits, fake: Fake) -> Writer<Fake> {
    let spec = straum_notation::parse(SPEC).unwrap();
    Writer::new(App::new(spec, limits, None), fake)
}

fn writer() -> Writer<Fake> {
    writer_with(LIMITS, fake())
}

struct Server {
    addr: SocketAddr,
    stop: oneshot::Sender<()>,
    thread: JoinHandle<io::Result<()>>,
}

fn serve(app: App) -> Server {
    serve_with(app, |query, parameters| {
        straum_notation::parse_ask(query, parameters)
            .map_err(|errors| errors.iter().map(ToString::to_string).collect())
    })
}

fn serve_with(app: App, read_ask: ReadAsk) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let thread = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(straum_http::serve(listener, app, read_ask, async {
            stopped.await.ok();
        }))
    });
    Server { addr, stop, thread }
}

/// Loads every table, then serves the app.
fn started(mut writer: Writer<Fake>) -> (Writer<Fake>, Server) {
    assert_eq!(writer.step().unwrap(), Step::Started(START));
    let server = serve(writer.app().clone());
    (writer, server)
}

impl Server {
    fn stop(self) {
        self.stop.send(()).unwrap();
        self.thread.join().unwrap().unwrap();
    }
}

struct Reply {
    status: u16,
    head: String,
    body: String,
}

/// Sends `request` whole and reads the reply until the server closes.
fn send(addr: SocketAddr, request: &str) -> Reply {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    read_reply(&mut stream)
}

/// Where the code sits in a status line such as `HTTP/1.1 200 OK`.
const STATUS_CODE: Range<usize> = 9..12;

fn read_reply(stream: &mut TcpStream) -> Reply {
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    let (head, body) = reply.split_once("\r\n\r\n").unwrap();
    Reply {
        status: head[STATUS_CODE].parse().unwrap(),
        head: head.to_owned(),
        body: body.to_owned(),
    }
}

fn request(addr: SocketAddr, method: &str, path: &str, accept: Option<&str>, body: &str) -> Reply {
    let accept = accept.map_or(String::new(), |accept| format!("accept: {accept}\r\n"));
    send(
        addr,
        &format!(
            "{method} {path} HTTP/1.1\r\nhost: straum\r\nconnection: close\r\n{accept}content-length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
}

fn call(addr: SocketAddr, method: &str, path: &str, body: &str) -> Reply {
    request(addr, method, path, None, body)
}

/// An ask from a person, who reads plain text.
fn call_as_text(addr: SocketAddr, body: &str) -> Reply {
    request(addr, "POST", "/ask", Some("text/plain"), body)
}

/// An ask as straum-http reads it: a query and its parameters.
fn ask(query: &str, parameters: &str) -> String {
    format!(r#"{{"query": "{query}", "parameters": {parameters}}}"#)
}

/// Every trip in the first hour.
fn first_hour() -> String {
    ask(
        "SELECT trips FROM trips WHERE bucket >= $1 AND bucket < $2",
        "[0, 3600]",
    )
}

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
    let reply = call(server.addr, "POST", "/ask", &ask(BY_BUCKET, range));
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
    let reply = call(server.addr, "POST", "/ask", &ask(BY_BUCKET, range));
    let seconds = ask(BY_BUCKET, "[1704067200, 1704240000]");
    let again = call(server.addr, "POST", "/ask", &seconds);
    assert_eq!(rows_of(&again.body), rows_of(&reply.body));
    let later = ask(BY_BUCKET, "[1704070800, 1704240000]");
    let fewer = call(server.addr, "POST", "/ask", &later);
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
    let reply = call(server.addr, "POST", "/ask", &totals);
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
    let reply = call_as_text(server.addr, &ask(BY_BUCKET, range));
    assert_eq!(reply.status, 200, "{}", reply.body);
    for header in [
        "content-type: text/plain; charset=utf-8",
        "straum-lsn: 16/B374D848",
    ] {
        assert!(reply.head.contains(header), "{}", reply.head);
    }
    let phases = server_timing(&reply.head).unwrap();
    let names: Vec<&str> = phases.iter().map(|phase| phase.name.as_str()).collect();
    assert_eq!(names, ["parse", "answer", "text", "total"]);
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
    let reply = call_as_text(server.addr, &first_hour());
    assert!(
        reply.body.starts_with("trips\n-----\n    0\n1 row, as of "),
        "{}",
        reply.body
    );
    let reply = request(server.addr, "POST", "/ask", Some("*/*"), &first_hour());
    assert_eq!(rows_of(&reply.body), r#"[{"trips":0}]"#);
    server.stop();
}

/// The content type of a 200 reply.
fn content_type(addr: SocketAddr, method: &str, path: &str, accept: Option<&str>) -> String {
    let body = if method == "POST" {
        first_hour()
    } else {
        String::new()
    };
    let reply = request(addr, method, path, accept, &body);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let content_type = reply
        .head
        .lines()
        .find_map(|line| line.strip_prefix("content-type: "));
    content_type.unwrap().to_owned()
}

#[test]
fn accept_picks_the_format_and_each_route_its_default() {
    let (_writer, server) = started(writer());
    let json = "application/json";
    let text = "text/plain; charset=utf-8";
    for (accept, ask, views) in [
        (Some("application/json"), json, json),
        (Some("text/plain"), text, text),
        (Some("*/*"), json, text),
        (None, json, text),
    ] {
        let got = content_type(server.addr, "POST", "/ask", accept);
        assert_eq!(got, ask, "/ask {accept:?}");
        let got = content_type(server.addr, "GET", "/views", accept);
        assert_eq!(got, views, "/views {accept:?}");
    }
    server.stop();
}

#[test]
fn errors_are_lines_to_those_who_accept_text() {
    let mut writer = writer();
    let server = serve(writer.app().clone());
    let reply = call_as_text(server.addr, &first_hour());
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
        let reply = call_as_text(server.addr, &body);
        assert_eq!((reply.status, reply.body.as_str()), (400, errors), "{body}");
        assert!(
            reply.head.contains("content-type: text/plain"),
            "{}",
            reply.head
        );
    }
    server.stop();
}

fn views_as_json(addr: SocketAddr) -> Reply {
    request(addr, "GET", "/views", Some("application/json"), "")
}

#[test]
fn views_wait_for_every_table() {
    let mut writer = writer();
    let server = serve(writer.app().clone());
    let reply = call(server.addr, "GET", "/views", "");
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (503, "not ready: loading\n")
    );

    writer.step().unwrap();
    writer.step().unwrap();
    writer.step().unwrap_err();
    let reply = call(server.addr, "GET", "/views", "");
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (503, "not ready: connection dropped\n")
    );
    let reply = views_as_json(server.addr);
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (503, r#"{"errors":["not ready: connection dropped"]}"#)
    );
    server.stop();
}

#[test]
fn views_say_what_straum_holds() {
    let (_writer, server) = started(writer());
    let reply = call(server.addr, "GET", "/views", "");
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.body,
        "\
view trips
  buckets:  3600s
  holds:    4 cells, 2024-01-01T00:00:00Z to 2024-01-02T00:00:00Z
  keys:     bucket timestamptz, kind integer, zone_id integer, borough text
  measures: trips, fare, fare_avg, fare_max

dim public.zones
  holds:    2 rows
"
    );
    server.stop();
}

#[test]
fn views_as_json_name_each_views_keys_and_measures() {
    let (_writer, server) = started(writer());
    let reply = views_as_json(server.addr);
    assert!(
        reply.head.contains("content-type: application/json"),
        "{}",
        reply.head
    );
    assert_eq!(
        reply.body,
        concat!(
            r#"{"views":[{"name":"trips","#,
            r#""from":"2024-01-01T00:00:00Z","to":"2024-01-02T00:00:00Z","#,
            r#""keys":[{"name":"bucket","type":"timestamptz"},{"name":"kind","type":"integer"},"#,
            r#"{"name":"zone_id","type":"integer"},{"name":"borough","type":"text"}],"#,
            r#""measures":["trips","fare","fare_avg","fare_max"]}]}"#
        )
    );
    server.stop();
}

#[test]
fn a_view_shows_its_rows() {
    let mut writer = writer();
    let server = serve(writer.app().clone());
    let reply = call(server.addr, "GET", "/views/trips", "");
    assert_eq!((reply.status, reply.body.as_str()), (503, "not ready\n"));
    let reply = call(server.addr, "GET", "/views/zones", "");
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (404, "no view zones; GET /views lists them\n")
    );
    let reply = call(server.addr, "POST", "/views/trips", "");
    assert_eq!((reply.status, reply.body.as_str()), (405, "use GET\n"));

    writer.step().unwrap();
    let reply = call(server.addr, "GET", "/views/trips", "");
    assert_eq!(reply.status, 200, "{}", reply.body);
    let (table, as_of) = reply.body.split_at(reply.body.find("4 rows").unwrap());
    assert_eq!(
        table,
        "\
bucket               | kind | zone_id | borough    | trips | fare  | fare_avg | fare_max
---------------------+------+---------+------------+-------+-------+----------+---------
2024-01-01T00:00:00Z |    1 |       1 | Bronx      |     1 | 12.50 |     12.5 |    12.50
2024-01-01T01:00:00Z |    1 |       1 | Bronx      |     1 |  2.50 |      2.5 |     2.50
2024-01-01T02:00:00Z |    2 |       3 | null       |     1 | null  | null     | null
2024-01-01T23:00:00Z |    2 |       2 | Queens \"Q\" |     1 | -0.05 |    -0.05 |    -0.05
"
    );
    assert!(as_of.starts_with("4 rows, as of 2"), "{as_of}");
    server.stop();
}

/// The rows `GET /views/NAME` shows at most.
const SHOWN_ROWS: i64 = 1_000;

/// More trips than a view shows, each in a cell of its own.
#[test]
fn a_view_shows_its_first_thousand_rows() {
    let fake = Fake {
        trips: (0..=SHOWN_ROWS)
            .map(|kind| trip(0, kind, 1, int(100)))
            .collect(),
        ..fake()
    };
    let (_writer, server) = started(writer_with(LIMITS, fake));
    let reply = call(server.addr, "GET", "/views/trips", "");
    // The names, the rule, the rows, then the count.
    let lines = usize::try_from(SHOWN_ROWS + 3).unwrap();
    assert_eq!(reply.body.lines().count(), lines, "{}", reply.body);
    let count = reply.body.lines().last().unwrap();
    assert!(count.starts_with("1000 of 1001 rows, as of "), "{count}");
    server.stop();
}

/// Takes every bucket and gives none back.
struct Lost;

impl Spill for Lost {
    fn put(&self, _: &Sealed) -> Result<(), SpillError> {
        Ok(())
    }

    fn get(&self, id: &SealedId) -> Result<Sealed, SpillError> {
        Err(format!("lost {id}").into())
    }

    fn remove(&self, _: &SealedId) -> Result<(), SpillError> {
        Ok(())
    }

    fn clear(&self) -> Result<(), SpillError> {
        Ok(())
    }
}

#[test]
fn a_sealed_bucket_that_cannot_be_read_is_a_server_error() {
    let spec = straum_notation::parse(SPEC).unwrap();
    let spilling = Spilling {
        spill: Arc::new(Lost),
        seal_after: Duration::ZERO,
    };
    let (_writer, server) = started(Writer::new(App::new(spec, LIMITS, Some(spilling)), fake()));
    let range = r#"["2024-01-01T00:00:00Z", "2024-01-01T01:00:00Z"]"#;
    let reply = call(server.addr, "POST", "/ask", &ask(BY_BUCKET, range));
    let error = r#"{"errors":["reading a sealed bucket failed: lost trips bucket of 3600s at 1704067200"]}"#;
    assert_eq!((reply.status, reply.body.as_str()), (500, error));
    server.stop();
}

#[test]
fn no_reply_may_be_cached() {
    let (_writer, server) = started(writer());
    let answer = call(server.addr, "POST", "/ask", &first_hour());
    let error = call(server.addr, "POST", "/ask", "{");
    let status = call(server.addr, "GET", "/status", "");
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
fn asks_straum_cant_answer() {
    let mut writer = writer();
    let server = serve(writer.app().clone());
    let reply = call(server.addr, "POST", "/ask", &first_hour());
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (503, r#"{"errors":["not ready"]}"#)
    );

    writer.step().unwrap();
    let cases = [
        (
            ask(
                "SELECT zone_nope, nope FROM trips \
                 WHERE bucket >= $1 AND bucket < $2 GROUP BY zone_nope",
                "[0, 1]",
            ),
            concat!(
                r#"{"errors":["unknown measure nope","#,
                r#""zone_nope is not a key of the view","#,
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
    ];
    for (body, errors) in cases {
        let reply = call(server.addr, "POST", "/ask", &body);
        assert_eq!((reply.status, reply.body.as_str()), (400, errors), "{body}");
    }
    server.stop();
}

#[test]
fn ask_bodies_too_big_or_broken_are_refused() {
    let server = serve(writer().app().clone());
    let big = format!("{{\"query\": \"{}\"}}", "a".repeat(1 << 20));
    let reply = call(server.addr, "POST", "/ask", &big);
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (413, r#"{"errors":["an ask is at most 1048576 bytes"]}"#)
    );

    // A chunked body that breaks off.
    let reply = send(
        server.addr,
        "POST /ask HTTP/1.1\r\nhost: straum\r\ntransfer-encoding: chunked\r\n\r\nzz\r\n",
    );
    assert_eq!(reply.status, 400);
    assert!(
        reply.body.starts_with(r#"{"errors":["reading the ask: "#),
        "{}",
        reply.body
    );
    server.stop();
}

#[derive(Debug, PartialEq)]
struct TimingPhase {
    name: String,
    took_ms: f64,
}

/// The phases of `Server-Timing`, if the reply has one.
fn server_timing(head: &str) -> Option<Vec<TimingPhase>> {
    let value = head
        .lines()
        .find_map(|line| line.strip_prefix("server-timing: "))?;
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

#[test]
fn says_where_the_time_went() {
    let mut writer = writer();
    let server = serve(writer.app().clone());
    let reply = call(server.addr, "POST", "/ask", &first_hour());
    assert_eq!(reply.status, 503);
    assert_eq!(server_timing(&reply.head), None, "{}", reply.head);

    writer.step().unwrap();
    let reply = call(server.addr, "POST", "/ask", &first_hour());
    assert_eq!(reply.status, 200, "{}", reply.body);
    let phases = server_timing(&reply.head).expect("a Server-Timing header");
    let names: Vec<&str> = phases.iter().map(|phase| phase.name.as_str()).collect();
    assert_eq!(names, ["parse", "answer", "json", "total"]);
    let total = phases[3].took_ms;
    for TimingPhase { name, took_ms } in &phases {
        assert!(
            (0.0..=total).contains(took_ms),
            "{name} took {took_ms} ms of {total}"
        );
    }

    let reply = call(server.addr, "POST", "/ask", &ask("SELECT", "[]"));
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert_eq!(server_timing(&reply.head), None, "{}", reply.head);
    server.stop();
}

#[test]
fn routes() {
    let server = serve(writer().app().clone());
    let reply = call(server.addr, "GET", "/livez", "");
    assert_eq!((reply.status, reply.body.as_str()), (200, "live\n"));
    let reply = call(server.addr, "GET", "/nope", "");
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
        let reply = call(server.addr, method, path, "");
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
    let reply = call(server.addr, "GET", "/status", "");
    assert_eq!(reply.status, 200);
    assert_eq!(
        reply.body,
        "\
ready:        no, loading
applied:      nothing yet
as of:        unknown until every table is loaded
lag:          unknown until every table is loaded
WAL retained: unknown until every table is loaded
starts:       0
failures:     0
last failure: none
sealed:       no spill
"
    );
    server.stop();
}

#[test]
fn status_says_what_the_stream_applied() {
    let (mut writer, server) = started(writer());
    writer.step().unwrap();
    let reply = call(server.addr, "GET", "/status", "");
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
    assert_eq!(lines[5..7], ["starts:       1", "failures:     0"]);
    server.stop();
}

#[test]
fn readiness_says_why_not() {
    let mut writer = writer();
    let server = serve(writer.app().clone());
    let reply = call(server.addr, "GET", "/readyz", "");
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (503, "not ready: loading\n")
    );
    writer.step().unwrap();
    let reply = call(server.addr, "GET", "/readyz", "");
    assert_eq!((reply.status, reply.body.as_str()), (200, "ready\n"));
    writer.step().unwrap();
    writer.step().unwrap_err();
    let reply = call(server.addr, "GET", "/readyz", "");
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (503, "not ready: connection dropped\n")
    );
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
    let reply = call(server.addr, "GET", "/readyz", "");
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (503, "not ready: 1 s behind PostgreSQL\n")
    );
    // Asks still answer.
    assert_eq!(call(server.addr, "POST", "/ask", &first_hour()).status, 200);
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
    let idle = TcpStream::connect(server.addr).unwrap();
    let mut busy = TcpStream::connect(server.addr).unwrap();
    let head = format!(
        "POST /ask HTTP/1.1\r\nhost: straum\r\ncontent-length: {}\r\n\r\n",
        ask.len()
    );
    busy.write_all(head.as_bytes()).unwrap();
    let (sent, rest) = ask.as_bytes().split_at(SENT_BEFORE_SHUTDOWN);
    busy.write_all(sent).unwrap();
    thread::sleep(SETTLE);
    let addr = server.addr;
    let stopping = thread::spawn(move || server.stop());
    thread::sleep(SETTLE);
    busy.write_all(rest).unwrap();
    let reply = read_reply(&mut busy);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.head.contains("connection: close"), "{}", reply.head);
    stopping.join().unwrap();
    drop(idle);
    assert!(TcpStream::connect(addr).is_err(), "still listening");
}

fn trips_from(from: i64) -> Ask {
    Ask {
        view: Identifier::new("trips").unwrap(),
        measures: vec![Identifier::new("trips").unwrap()],
        keys: Vec::new(),
        filter: Vec::new(),
        from,
        to: JAN_1 + 24 * HOUR,
    }
}

/// A query no reader in Straum reads, for readers that ignore it.
const ANY_QUERY: &str = r#"{"query": "not an ask in any syntax"}"#;

fn loaded() -> Writer<Fake> {
    let mut writer = writer();
    writer.step().unwrap();
    writer
}

#[test]
fn reads_asks_with_the_reader_it_is_given() {
    let writer = loaded();
    let server = serve_with(writer.app().clone(), |_, _| Ok(trips_from(JAN_1)));
    let reply = call(server.addr, "POST", "/ask", ANY_QUERY);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(rows_of(&reply.body), r#"[{"trips":4}]"#);
    server.stop();
}

#[test]
fn a_readers_errors_are_the_replys() {
    let writer = loaded();
    let server = serve_with(writer.app().clone(), |_, _| {
        Err(vec!["no".to_owned(), "never".to_owned()])
    });
    let reply = call(server.addr, "POST", "/ask", ANY_QUERY);
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (400, r#"{"errors":["no","never"]}"#)
    );
    server.stop();

    let server = serve_with(writer.app().clone(), |_, _| {
        Err(vec!["a \"quote\", a \\ and\na line in Zürich".to_owned()])
    });
    let reply = call(server.addr, "POST", "/ask", ANY_QUERY);
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
    let server = serve_with(writer.app().clone(), |_, _| Ok(trips_from(i64::MIN)));
    let reply = call(server.addr, "POST", "/ask", ANY_QUERY);
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

/// An answer's rows, once its `as_of` is checked to be a time from the last
/// minute.
fn rows_of(body: &str) -> &str {
    let Some(fields) = body.strip_prefix(r#"{"as_of":""#) else {
        panic!("not an answer: {body}");
    };
    let Some(fields) = fields.strip_suffix('}') else {
        panic!("not an answer: {body}");
    };
    let Some((as_of, rows)) = fields.split_once(r#"","rows":"#) else {
        panic!("not an answer: {body}");
    };
    let as_of: Timestamp = as_of.parse().unwrap();
    let age = Timestamp::now().duration_since(as_of);
    assert!(
        (SignedDuration::ZERO..SignedDuration::from_secs(60)).contains(&age),
        "{as_of}"
    );
    rows
}
