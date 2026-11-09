//! The HTTP adapter over a real socket: a scripted source fills the app,
//! and requests go out as raw HTTP/1.1.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::thread::{self, JoinHandle};
use std::time::{Duration, UNIX_EPOCH};

use straum_app::{App, Change, Commit, Lsn, Source, SourceError, Step, Writer};
use straum_domain::{Datum, Dim, DimTable, Fact, Ident, Rollup, Spec};
use tokio::sync::oneshot;

const SPEC: &str = "
CREATE VIEW trips WITH (key = id) AS
SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket,
       t.kind,
       t.zone_id,
       zone.borough,
       count(*) AS trips,
       sum(t.fare) AS fare,
       avg(t.fare) AS fare_avg,
       max(t.fare) AS fare_max
FROM public.trips t
LEFT JOIN public.zones zone ON zone.id = t.zone_id
GROUP BY 1, 2, 3, 4;
";

/// 2024-01-01 00:00 UTC.
const JAN_1: i64 = 1_704_067_200;
const HOUR: i64 = 3_600;
const START: Lsn = Lsn(0x16_B374_D848);

/// Loads fixed rows at [`START`], then streams its commits.
struct Script {
    trips: Vec<Vec<Datum>>,
    zones: Vec<Vec<Datum>>,
    commits: VecDeque<Commit>,
}

impl Source for Script {
    fn open(&mut self, _: &Spec) -> Result<Lsn, SourceError> {
        Ok(START)
    }

    fn load_fact(&mut self, _: &Fact, rollup: &mut Rollup) -> Result<Lsn, SourceError> {
        // at, kind, zone_id, fare: fare is numeric(10, 2).
        rollup.set_scales(&[0, 0, 0, 2])?;
        for row in &self.trips {
            rollup.add(row)?;
        }
        Ok(START)
    }

    fn load_dim(&mut self, _: &Dim, table: &mut DimTable) -> Result<Lsn, SourceError> {
        for row in &self.zones {
            table.upsert(row)?;
        }
        Ok(START)
    }

    fn next_commit(&mut self) -> Result<Commit, SourceError> {
        self.commits
            .pop_front()
            .ok_or_else(|| "connection dropped".into())
    }
}

fn int(n: i64) -> Datum {
    Datum::Int(n)
}

fn text(s: &str) -> Datum {
    Datum::Text(s.into())
}

fn trip(hour: i64, kind: i64, zone: i64, fare: Datum) -> Vec<Datum> {
    vec![int(JAN_1 + hour * HOUR), int(kind), int(zone), fare]
}

/// Three trips on 2024-01-01 UTC, one at 23:00, which is the next day in
/// Oslo, and one in a zone with no row.
fn writer() -> Writer<Script> {
    let script = Script {
        trips: vec![
            trip(0, 1, 1, int(1_250)),
            trip(1, 1, 1, int(250)),
            trip(2, 2, 3, Datum::Null),
            trip(23, 2, 2, int(-5)),
        ],
        zones: vec![
            vec![int(1), text("Bronx")],
            vec![int(2), text("Queens \"Q\"")],
        ],
        commits: VecDeque::from([Commit {
            lsn: Lsn(0x17_0000_0000),
            time: UNIX_EPOCH + Duration::from_micros(1_704_153_600_123_456),
            changes: vec![Change::Insert {
                table: Ident::new("trips").unwrap(),
                row: trip(3, 1, 1, int(100)),
            }],
        }]),
    };
    let spec = straum_notation::parse(SPEC).unwrap();
    Writer::new(App::new(spec), script)
}

struct Server {
    addr: SocketAddr,
    stop: oneshot::Sender<()>,
    thread: JoinHandle<io::Result<()>>,
}

fn serve(app: App) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, stopped) = oneshot::channel::<()>();
    let thread = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        runtime.block_on(straum_http::serve(listener, app, async {
            stopped.await.ok();
        }))
    });
    Server { addr, stop, thread }
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

fn read_reply(stream: &mut TcpStream) -> Reply {
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    let (head, body) = reply.split_once("\r\n\r\n").unwrap();
    Reply {
        status: head[9..12].parse().unwrap(),
        head: head.to_owned(),
        body: body.to_owned(),
    }
}

fn call(addr: SocketAddr, method: &str, path: &str, body: &str) -> Reply {
    send(
        addr,
        &format!(
            "{method} {path} HTTP/1.1\r\nhost: straum\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        ),
    )
}

const BY_DAY: &str = r#"{
    "fact": "trips",
    "measures": ["trips", "fare", "fare_avg", "fare_max"],
    "by": ["kind", "zone.borough"],
    "time": "day",
    "zone": "Europe/Oslo",
    "from": "2024-01-01T00:00:00+01:00",
    "to": "2024-01-03T00:00:00Z"
}"#;

#[test]
fn answers_asks_as_json() {
    let mut writer = writer();
    assert_eq!(writer.step().unwrap(), Step::Started(START));
    let server = serve(writer.app().clone());

    let reply = call(server.addr, "POST", "/ask", BY_DAY);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(
        reply.head.contains("content-type: application/json"),
        "{}",
        reply.head
    );
    let rows = concat!(
        r#"["2024-01-01T00:00:00+01:00",1,"Bronx",2,15.00,{"sum":15.00,"count":2},12.50],"#,
        r#"["2024-01-01T00:00:00+01:00",2,null,1,null,null,null],"#,
        r#"["2024-01-02T00:00:00+01:00",2,"Queens \"Q\"",1,-0.05,{"sum":-0.05,"count":1},-0.05]"#,
    );
    let columns = r#"["day","kind","zone.borough","trips","fare","fare_avg","fare_max"]"#;
    assert_eq!(
        reply.body,
        format!(
            r#"{{"lsn":"16/B374D848","commit_time":null,"columns":{columns},"rows":[{rows}]}}"#
        )
    );

    // The same range in Unix seconds.
    let seconds = BY_DAY
        .replace(r#""2024-01-01T00:00:00+01:00""#, "1704063600")
        .replace(r#""2024-01-03T00:00:00Z""#, "1704240000");
    assert_eq!(call(server.addr, "POST", "/ask", &seconds).body, reply.body);

    // After a commit, answers carry its LSN and time.
    assert_eq!(writer.step().unwrap(), Step::Applied(Lsn(0x17_0000_0000)));
    let ask = r#"{"fact": "trips", "measures": ["trips", "fare"], "from": 0, "to": 1704240000}"#;
    assert_eq!(
        call(server.addr, "POST", "/ask", ask).body,
        concat!(
            r#"{"lsn":"17/0","commit_time":"2024-01-02T00:00:00.123456Z","#,
            r#""columns":["trips","fare"],"rows":[[5,15.95]]}"#
        )
    );
    server.stop();
}

#[test]
fn asks_straum_cant_answer() {
    let mut writer = writer();
    let server = serve(writer.app().clone());
    let ask = r#"{"fact": "trips", "measures": ["trips"], "from": 0, "to": 3600}"#;
    let reply = call(server.addr, "POST", "/ask", ask);
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (503, r#"{"errors":["not ready"]}"#)
    );

    writer.step().unwrap();
    let cases = [
        (
            r#"{"fact": "trips", "measures": ["nope"], "by": ["zone.nope"], "from": 0, "to": 1}"#,
            concat!(
                r#"{"errors":["unknown measure nope","#,
                r#""zone.nope is not a by column, ref or ref attribute","#,
                r#""range end 1 is not on a 3600s bucket edge"]}"#
            ),
        ),
        (
            r#"{"fact": "trips", "measures": ["trips"], "from": 0, "to": 3600, "limit": 1}"#,
            r#"{"errors":["unknown field \"limit\""]}"#,
        ),
        (
            "{\"fact\": \"trips\", \"fact\": \"trips\"}",
            r#"{"errors":["key \"fact\" given twice at line 1 column 24"]}"#,
        ),
    ];
    for (ask, errors) in cases {
        let reply = call(server.addr, "POST", "/ask", ask);
        assert_eq!((reply.status, reply.body.as_str()), (400, errors), "{ask}");
    }

    let big = format!("{{\"fact\": \"{}\"}}", "a".repeat(1 << 20));
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
        ("PUT", "/metrics", "GET"),
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
fn readiness_says_why_not() {
    let mut writer = writer();
    let server = serve(writer.app().clone());
    let reply = call(server.addr, "GET", "/readyz", "");
    assert_eq!(
        (reply.status, reply.body.as_str()),
        (503, "not ready: waiting for zones, trips\n")
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
fn metrics() {
    let mut writer = writer();
    let server = serve(writer.app().clone());
    let ask = r#"{"fact": "trips", "measures": ["trips"], "from": 0, "to": 3600}"#;
    call(server.addr, "POST", "/ask", ask);
    let before = call(server.addr, "GET", "/metrics", "");
    assert_eq!(before.status, 200);
    assert!(
        before
            .head
            .contains("content-type: text/plain; version=0.0.4")
    );
    for line in [
        "straum_ready 0",
        "straum_asks_total{outcome=\"not_ready\"} 1",
        "straum_cells{fact=\"trips\"} 0",
        "straum_table_ready{table=\"zones\"} 0",
        "straum_ask_seconds_count 0",
    ] {
        assert!(
            before.body.lines().any(|l| l == line),
            "{line} in\n{}",
            before.body
        );
    }
    assert!(
        !before.body.lines().any(|l| l.starts_with("straum_lsn ")),
        "{}",
        before.body
    );

    writer.step().unwrap();
    writer.step().unwrap();
    call(server.addr, "POST", "/ask", ask);
    call(server.addr, "POST", "/ask", "{}");
    let after = call(server.addr, "GET", "/metrics", "").body;
    let expected = [
        "# HELP straum_ready 1 when every table is loaded and the stream has passed each load.",
        "# TYPE straum_ready gauge",
        "straum_ready 1",
        "straum_lsn 98784247808",
        "straum_commit_time_seconds 1704153600.123456",
        "straum_starts_total 1",
        "straum_commits_total 1",
        "straum_reloads_total 0",
        "straum_failures_total 0",
        "straum_cells{fact=\"trips\"} 5",
        "straum_rows{dim=\"zones\"} 2",
        "straum_table_ready{table=\"zones\"} 1",
        "straum_table_ready{table=\"trips\"} 1",
        "straum_asks_total{outcome=\"answered\"} 1",
        "straum_asks_total{outcome=\"not_ready\"} 1",
        "straum_asks_total{outcome=\"invalid\"} 1",
        "straum_asks_total{outcome=\"failed\"} 0",
        "straum_ask_seconds_bucket{le=\"+Inf\"} 1",
        "straum_ask_seconds_count 1",
    ];
    for line in expected {
        assert!(after.lines().any(|l| l == line), "{line} in\n{after}");
    }
    // One cumulative bucket per bound, ending at the count.
    let buckets: Vec<u64> = (after.lines())
        .filter(|l| l.starts_with("straum_ask_seconds_bucket"))
        .map(|l| l.rsplit(' ').next().unwrap().parse().unwrap())
        .collect();
    assert_eq!(buckets.len(), 10);
    assert!(buckets.is_sorted() && buckets[9] == 1, "{buckets:?}");
    // Each family is one group: its help and type, then its samples.
    let mut families = Vec::new();
    for line in after.lines() {
        let name = match line.strip_prefix("# HELP ") {
            Some(help) => help.split(' ').next().unwrap(),
            None if line.starts_with('#') => continue,
            None => line.split(['{', ' ']).next().unwrap(),
        };
        let family = ["_bucket", "_sum", "_count"]
            .iter()
            .find_map(|s| name.strip_suffix(s))
            .unwrap_or(name);
        if families.last() != Some(&family) {
            assert!(!families.contains(&family), "{family} split in\n{after}");
            families.push(family);
        }
    }
    assert_eq!(families.len(), 12, "{families:?}");
    server.stop();
}

#[test]
fn shutdown_finishes_requests_in_flight() {
    let mut writer = writer();
    writer.step().unwrap();
    let server = serve(writer.app().clone());
    let ask = r#"{"fact": "trips", "measures": ["trips"], "from": 0, "to": 3600}"#;

    // An idle keep-alive connection doesn't hold the shutdown up, and a
    // request whose body is still coming finishes.
    let idle = TcpStream::connect(server.addr).unwrap();
    let mut busy = TcpStream::connect(server.addr).unwrap();
    let head = format!(
        "POST /ask HTTP/1.1\r\nhost: straum\r\ncontent-length: {}\r\n\r\n",
        ask.len()
    );
    busy.write_all(head.as_bytes()).unwrap();
    busy.write_all(&ask.as_bytes()[..10]).unwrap();
    thread::sleep(Duration::from_millis(200));
    let addr = server.addr;
    let stopping = thread::spawn(move || server.stop());
    thread::sleep(Duration::from_millis(200));
    busy.write_all(&ask.as_bytes()[10..]).unwrap();
    let reply = read_reply(&mut busy);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(reply.head.contains("connection: close"), "{}", reply.head);
    stopping.join().unwrap();
    drop(idle);
    assert!(TcpStream::connect(addr).is_err(), "still listening");
}
