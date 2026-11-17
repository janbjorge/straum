//! The Straum binary end to end, run as a child process. `straum plan`
//! sets up PostgreSQL in Docker, `straum run` loads and follows it as the
//! role the plan made, and asks over HTTP, `straum ask` and `straum shell`
//! answer what the view body answers in PostgreSQL. The tests without a
//! database check help, usage and the errors a person reads. A script's
//! view is checked too: `--json`, `STRAUM_URL` and exit 3 while not ready.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::ops::Range;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

// straum-pg's tests start PostgreSQL the same way; the file uses std alone.
#[path = "../../straum-pg/src/tests/compose.rs"]
#[expect(
    dead_code,
    reason = "these tests set no entrypoint, sysctl or shared memory"
)]
mod compose;

use compose::{Service, Stack};

const STRAUM: &str = env!("CARGO_BIN_EXE_straum");

const SPEC: &str = "
CREATE VIEW trips WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket,
       t.kind AS kind,
       t.zone_id AS zone_id,
       zone.borough AS borough,
       count(*) AS trips,
       sum(t.fare) AS fare
FROM shop.trips AS t
LEFT OUTER JOIN shop.zones AS zone ON zone.id = t.zone_id
GROUP BY 1, 2, 3, 4;
";

const TABLES: &str = "
CREATE SCHEMA shop;
CREATE TABLE shop.zones (id int PRIMARY KEY, borough text);
CREATE TABLE shop.trips (
    id bigint PRIMARY KEY,
    at timestamptz NOT NULL,
    kind text,
    zone_id int,
    fare numeric(8, 2)
);
INSERT INTO shop.zones VALUES (1, 'Bronx'), (2, 'Queens');
INSERT INTO shop.trips VALUES
    (1, '2024-01-01 00:10Z', 'cab', 1, 12.50),
    (2, '2024-01-01 01:10Z', 'cab', 2, 0.05),
    (3, '2024-01-01 02:10Z', 'van', 3, NULL),
    (4, '2024-01-01 03:10Z', NULL, 1, -2.25);
";

/// What the ask below answers, from the view body in PostgreSQL, as JSON
/// rows in Straum's order.
const EXPECTED: &str = r#"
SELECT coalesce(string_agg(row, ',' ORDER BY kind COLLATE "C" NULLS FIRST,
                                             borough COLLATE "C" NULLS FIRST), '')
  FROM (SELECT kind, borough,
               json_build_object('kind', kind, 'borough', borough,
                                 'trips', count(*), 'fare', sum(fare))::text AS row
          FROM shop.trips t
          LEFT JOIN shop.zones zone ON zone.id = t.zone_id
         GROUP BY 1, 2) v
"#;

const ASK: &str = concat!(
    r#"{"query": "SELECT kind, borough, trips, fare FROM trips "#,
    r#"WHERE bucket >= $1 AND bucket < $2 GROUP BY kind, borough", "#,
    r#""parameters": [0, "2100-01-01T00:00:00Z"]}"#
);

struct PostgresContainer {
    container: Stack,
}

/// CI sets `STRAUM_TEST_POSTGRES` to each major Straum supports.
fn tag() -> String {
    match std::env::var("STRAUM_TEST_POSTGRES") {
        Ok(tag) => tag,
        Err(_) => "18".to_owned(),
    }
}

fn postgres() -> PostgresContainer {
    let service = Service::new(&tag())
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .with_env_var("POSTGRES_HOST_AUTH_METHOD", "scram-sha-256")
        .with_cmd(&["-c", "wal_level=logical"]);
    PostgresContainer {
        container: Stack::start(&service),
    }
}

impl PostgresContainer {
    /// One statement at a time, as the superuser; the output unaligned.
    fn psql(&self, sql: &str) -> String {
        let result = self.container.exec(&[
            "psql",
            "postgres://postgres:postgres@127.0.0.1/postgres",
            "-At",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            sql,
        ]);
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(result.status.success(), "{sql}: {stderr}");
        String::from_utf8(result.stdout)
            .unwrap()
            .trim_end()
            .to_owned()
    }

    fn dsn(&self, user: &str) -> String {
        format!(
            "postgres://{user}:{user}@127.0.0.1:{}/postgres?sslmode=disable",
            self.container.port
        )
    }

    fn expected_rows(&self) -> String {
        let rows = self.psql(EXPECTED).replace(", ", ",").replace(" : ", ":");
        format!("\"rows\":[{rows}]}}")
    }
}

/// A directory of the test's own, removed at the end.
struct Scratch {
    path: PathBuf,
}

static SCRATCH: AtomicUsize = AtomicUsize::new(0);

impl Scratch {
    fn new() -> Scratch {
        let number = SCRATCH.fetch_add(1, Ordering::Relaxed);
        let name = format!("straum-test-{}-{number}", std::process::id());
        let path = std::env::temp_dir().join(name);
        fs::create_dir_all(&path).unwrap();
        Scratch { path }
    }

    fn file(&self, name: &str, text: &str) -> String {
        let path = self.path.join(name);
        fs::write(&path, text).unwrap();
        path.to_str().unwrap().to_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.path).ok();
    }
}

fn straum(args: &[&str]) -> Command {
    let mut command = Command::new(STRAUM);
    command.args(args).env_remove("STRAUM_DSN");
    command
}

fn output(command: &mut Command) -> (Option<i32>, String, String) {
    let Output {
        status,
        stdout,
        stderr,
    } = command.output().unwrap();
    (
        status.code(),
        String::from_utf8(stdout).unwrap(),
        String::from_utf8(stderr).unwrap(),
    )
}

/// What a test waits for a running Straum to do.
enum Until {
    /// `/readyz` answers 200.
    Ready,
    /// The ask in `ASK` answers with these rows at the end.
    Answers(String),
}

/// Where the code sits in a status line such as `HTTP/1.1 200 OK`.
const STATUS_CODE: Range<usize> = 9..12;

/// `straum run`, its log gathered as it comes.
struct Running {
    child: Child,
    address: SocketAddr,
    log: Arc<Mutex<String>>,
    gatherer: Option<JoinHandle<()>>,
}

fn run(args: &[&str], dsn: &str) -> Running {
    let mut child = straum(args)
        .env("STRAUM_DSN", dsn)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let log = Arc::new(Mutex::new(String::new()));
    let lines = BufReader::new(child.stderr.take().unwrap());
    let shared_log = Arc::clone(&log);
    let gatherer = thread::spawn(move || {
        for line in lines.lines() {
            let mut log = shared_log.lock().unwrap();
            log.push_str(&line.unwrap());
            log.push('\n');
        }
    });
    let mut gatherer = Some(gatherer);
    let start = Instant::now();
    while !log.lock().unwrap().contains("listening on ") {
        if child.try_wait().unwrap().is_some() {
            // An exit may come before its log is read: the gatherer ends
            // with the pipe, so the log is whole once it joins.
            gatherer.take().unwrap().join().unwrap();
            let log = log.lock().unwrap();
            assert!(
                log.contains("listening on "),
                "Straum exited; its log:\n{log}"
            );
            break;
        }
        wait(&mut child, &log, "Straum to listen", start);
    }
    let address = {
        let log = log.lock().unwrap();
        let at = log.find("listening on ").unwrap() + "listening on ".len();
        log[at..].lines().next().unwrap().parse().unwrap()
    };
    Running {
        child,
        address,
        log,
        gatherer,
    }
}

fn wait(child: &mut Child, log: &Mutex<String>, what: &str, start: Instant) {
    assert!(
        start.elapsed() < Duration::from_secs(60),
        "waited a minute for {what}; Straum's log:\n{}",
        log.lock().unwrap()
    );
    assert!(
        child.try_wait().unwrap().is_none(),
        "Straum exited; its log:\n{}",
        log.lock().unwrap()
    );
    thread::sleep(Duration::from_millis(100));
}

impl Running {
    fn log(&self) -> String {
        self.log.lock().unwrap().clone()
    }

    fn wait_for(&mut self, what: &str, until: &Until) {
        let start = Instant::now();
        while !self.reached(until) {
            wait(&mut self.child, &self.log, what, start);
        }
    }

    fn reached(&self, until: &Until) -> bool {
        match until {
            Until::Ready => self.call("GET", "/readyz", "").0 == 200,
            Until::Answers(rows) => self.call("POST", "/ask", ASK).1.ends_with(rows),
        }
    }

    fn wait_for_rows(&mut self, pg: &PostgresContainer, what: &str) {
        self.wait_for(what, &Until::Answers(pg.expected_rows()));
    }

    /// The status and body.
    fn call(&self, method: &str, path: &str, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(self.address).unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nhost: straum\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        let (head, body) = reply.split_once("\r\n\r\n").unwrap();
        (head[STATUS_CODE].parse().unwrap(), body.to_owned())
    }

    /// Sends `signal`, such as `-TERM`, and goes on.
    fn signal(&self, signal: &str) {
        let pid = self.child.id().to_string();
        let sent = Command::new("kill").args([signal, &pid]).status().unwrap();
        assert!(sent.success());
    }

    /// Whether Straum is still running.
    fn running(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    /// Sends the signal and waits for Straum to exit.
    fn stop(self, signal: &str) -> (Option<i32>, String) {
        self.signal(signal);
        self.exited()
    }

    /// Waits a minute at most for Straum to exit on its own, and returns
    /// its exit code and whole log.
    fn exited(mut self) -> (Option<i32>, String) {
        let start = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "Straum never exited; its log:\n{}",
                self.log()
            );
            thread::sleep(Duration::from_millis(100));
        };
        // The gatherer ends with the pipe, so the log is whole once it joins.
        if let Some(gatherer) = self.gatherer.take() {
            gatherer.join().unwrap();
        }
        (status.code(), self.log())
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// Runs the statements of `straum plan` one at a time, since ALTER SYSTEM
/// can't run in a transaction, but those that start with `hold`, which it
/// returns. The role `straum` gets its password from the DSN the plan
/// reads, the one `straum run` connects with after.
fn set_up(pg: &PostgresContainer, spec: &str, hold: Option<&str>) -> Vec<String> {
    let mut plan = straum(&["plan", spec]);
    plan.env("STRAUM_DSN", pg.dsn("straum"));
    let (code, plan, _) = output(&mut plan);
    assert_eq!(code, Some(0));
    let mut held_back = Vec::new();
    for statement in plan.lines() {
        if statement.starts_with("--") {
            continue;
        }
        let is_held = match hold {
            Some(hold) => statement.starts_with(hold),
            None => false,
        };
        if is_held {
            held_back.push(statement.to_owned());
        } else {
            pg.psql(statement);
        }
    }
    held_back
}

#[test]
fn serves_asks_from_postgres() {
    let pg = postgres();
    pg.psql(TABLES);
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    assert_eq!(set_up(&pg, &spec, None), Vec::<String>::new());
    // From a file, as a mounted secret would give it.
    let dsn = scratch.file("dsn", &format!("{}\n", pg.dsn("straum")));
    let mut straum = run(
        &["run", &spec, "--listen", "127.0.0.1:0", "--dsn-file", &dsn],
        "not a DSN",
    );
    straum.wait_for("ready", &Until::Ready);

    let url = format!("http://{}", straum.address);
    status_says_ready(&url);
    let views = views_lists_the_view_and_the_dim(&url);
    ask_prints_a_table_or_says_why_not(&url);
    scripts_read_json_from_straum_url(&url);
    shell_answers_a_line_at_a_time(&url, &views);
    http_answers_in_json(&pg, &straum);
    follows_changes(&pg, &mut straum);
    stops_on_sigterm(&pg, straum);
}

// What `straum status` says, as from inside the pod.
fn status_says_ready(url: &str) {
    let (code, status, _) = output(Command::new(STRAUM).args(["status", "--url", url]));
    assert_eq!(code, Some(0), "{status}");
    assert!(
        status.starts_with("ready:        yes\napplied:      "),
        "{status}"
    );
    assert!(status.contains("\nWAL retained: "), "{status}");
    assert!(!status.contains("failure"), "{status}");
}

fn views_lists_the_view_and_the_dim(url: &str) -> String {
    let (code, views, _) = output(Command::new(STRAUM).args(["views", "--url", url]));
    assert_eq!(code, Some(0), "{views}");
    assert_eq!(
        views,
        "\
view trips
  buckets:  3600s
  holds:    4 cells, 2024-01-01T00:00:00Z to 2024-01-01T04:00:00Z
  keys:     bucket timestamptz, kind text, zone_id integer, borough text
  measures: trips, fare

dim shop.zones
  holds:    2 rows
  asked as: trips.borough
"
    );

    views
}

fn ask_prints_a_table_or_says_why_not(url: &str) {
    // The range `straum views` gave, as `straum ask` takes it.
    let query = "SELECT kind, borough, trips, fare FROM trips \
                 WHERE bucket >= $1 AND bucket < $2 GROUP BY kind, borough";
    let range = ["2024-01-01T00:00:00Z", "2024-01-01T04:00:00Z"];
    let (code, table, _) =
        output(Command::new(STRAUM).args(["ask", "--url", url, query, range[0], range[1]]));
    assert_eq!(code, Some(0), "{table}");
    let rows = "\
kind | borough | trips | fare
-----+---------+-------+------
null | Bronx   |     1 | -2.25
cab  | Bronx   |     1 | 12.50
cab  | Queens  |     1 |  0.05
van  | null    |     1 | null
4 rows, as of 20";
    assert!(table.starts_with(rows), "{table}");
    let wrong = "SELECT trips FROM trips WHERE kind = $1 AND bucket >= $2 AND bucket < $3";
    let (code, _, stderr) =
        output(Command::new(STRAUM).args(["ask", "--url", url, wrong, "7", "0", "3600"]));
    assert_eq!(code, Some(1), "{stderr}");
    assert_eq!(
        stderr,
        format!(
            "straum: {url} answered \"HTTP/1.1 400 Bad Request\"\n\
             straum: kind is text, so it can't be the integer 7\n"
        )
    );
}

/// `straum <args>` finding Straum through `STRAUM_URL` alone.
fn with_url_variable(url: &str, args: &[&str]) -> (Option<i32>, String, String) {
    output(Command::new(STRAUM).args(args).env("STRAUM_URL", url))
}

fn json(text: &str) -> serde_json::Value {
    match serde_json::from_str(text) {
        Ok(value) => value,
        Err(error) => panic!("{error}: {text}"),
    }
}

// What a script reads: `--json`, with Straum found through STRAUM_URL.
fn scripts_read_json_from_straum_url(url: &str) {
    let (code, status, _) = with_url_variable(url, &["status", "--json"]);
    assert_eq!(code, Some(0), "{status}");
    let status = json(&status);
    assert_eq!(status["ready"], true, "{status}");
    assert!(status["why_not"].is_null(), "{status}");
    assert!(status["lag_ms"].is_u64(), "{status}");

    let (code, views, _) = with_url_variable(url, &["views", "--json"]);
    assert_eq!(code, Some(0), "{views}");
    assert_eq!(json(&views)["views"][0]["name"], "trips", "{views}");

    let query = "SELECT kind, borough, trips FROM trips \
                 WHERE bucket >= $1 AND bucket < $2 GROUP BY kind, borough";
    let range = ["2024-01-01T00:00:00Z", "2024-01-01T04:00:00Z"];
    let (code, answer, _) = with_url_variable(url, &["ask", "--json", query, range[0], range[1]]);
    assert_eq!(code, Some(0), "{answer}");
    let answer = json(&answer);
    assert!(answer["as_of"].is_string(), "{answer}");
    assert_eq!(answer["rows"].as_array().unwrap().len(), 4, "{answer}");

    // Refused: nothing on stdout, the errors as JSON on stderr, exit 1.
    let (code, stdout, stderr) =
        with_url_variable(url, &["ask", "--json", "SELECT nope FROM trips"]);
    assert_eq!((code, stdout.as_str()), (Some(1), ""), "{stderr}");
    assert!(stderr.contains(r#"{"errors":["#), "{stderr}");
}

// The same from `straum shell`, a line each, on a pipe.
fn shell_answers_a_line_at_a_time(url: &str, views: &str) {
    let mut shell = Command::new(STRAUM)
        .args(["shell", "--url", url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let lines = "\\d\n\\rows trips\n\
        SELECT trips FROM trips WHERE bucket >= $1 AND bucket < $2; 0 2024-01-01T04:00:00Z\n\
        \\rows nope\n\\q\n\\d\n";
    (shell.stdin.take().unwrap())
        .write_all(lines.as_bytes())
        .unwrap();
    let shell = shell.wait_with_output().unwrap();
    let shelled = String::from_utf8(shell.stdout).unwrap();
    // A line that fails goes to stderr and the others still answer; the
    // exit says one failed.
    assert_eq!(shell.status.code(), Some(1), "{shelled}");
    assert_eq!(
        String::from_utf8(shell.stderr).unwrap(),
        format!(
            "straum: {url} answered \"HTTP/1.1 404 Not Found\"\n\
             no view nope; GET /views lists them\n\
             straum: lines that failed: 1\n"
        )
    );
    let rows = "\
bucket               | kind | zone_id | borough | trips | fare
---------------------+------+---------+---------+-------+------
2024-01-01T00:00:00Z | cab  |       1 | Bronx   |     1 | 12.50
2024-01-01T01:00:00Z | cab  |       2 | Queens  |     1 |  0.05
2024-01-01T02:00:00Z | van  |       3 | null    |     1 | null
2024-01-01T03:00:00Z | null |       1 | Bronx   |     1 | -2.25
4 rows, as of 20";
    let (described, rest) = shelled.split_at(views.len());
    assert_eq!(described, views);
    assert!(rest.starts_with(rows), "{rest}");
    let (_, rest) = rest.split_once("Z\n").unwrap();
    assert!(
        rest.starts_with("trips\n-----\n    4\n1 row, as of 20"),
        "{rest}"
    );
    assert_eq!(rest.split_once("Z\n").unwrap().1, "");
}

fn http_answers_in_json(pg: &PostgresContainer, straum: &Running) {
    let (status, body) = straum.call("POST", "/ask", ASK);
    assert_eq!(status, 200, "{body}");
    assert!(body.starts_with(r#"{"as_of":"20"#), "{body}");
    assert!(!body.contains("lsn"), "{body}");
    let rows = pg.expected_rows();
    assert!(body.ends_with(&rows), "{body}\nis not\n{rows}");
    let cab = r#"{"kind":"cab","borough":"Bronx","trips":1,"fare":12.50}"#;
    assert!(body.contains(cab), "{body}");
}

fn follows_changes(pg: &PostgresContainer, straum: &mut Running) {
    pg.psql("INSERT INTO shop.trips VALUES (5, '2024-01-02 00:00Z', 'cab', 2, 1.25)");
    pg.psql("DELETE FROM shop.trips WHERE id = 3");
    pg.psql("UPDATE shop.trips SET fare = 10 WHERE id = 1");
    straum.wait_for_rows(pg, "the changes");

    // A zone renamed shows in the next answer: the dim is held and
    // followed, so nothing loads again.
    pg.psql("UPDATE shop.zones SET borough = 'Brooklyn' WHERE id = 2");
    straum.wait_for_rows(pg, "the dim change");
    pg.psql("UPDATE shop.trips SET fare = 20 WHERE id = 2");
    straum.wait_for_rows(pg, "the trip change");
    assert!(!straum.log().contains("no longer fits"), "{}", straum.log());
}

fn stops_on_sigterm(pg: &PostgresContainer, straum: Running) {
    let (code, log) = straum.stop("-TERM");
    assert_eq!(code, Some(0), "{log}");
    for line in [
        "straum: opened a stream at ",
        "straum: loaded view trips in ",
        "straum: loaded every table; streaming from ",
    ] {
        assert!(log.contains(line), "{log}");
    }
    assert!(
        log.ends_with("straum: stopping: finishing the asks in flight\nstraum: stopped\n"),
        "{log}"
    );
    no_slot_left(pg);
}

/// Not ready is exit 3 for every command that asks Straum, so a script
/// can wait on it; `straum status` still prints why.
fn not_ready_exits_with_3(url: &str) {
    let (code, status, stderr) = output(&mut straum(&["status", "--url", url]));
    assert_eq!(code, Some(3), "{status}{stderr}");
    assert!(status.starts_with("ready:        no, "), "{status}");
    assert_eq!(stderr, "straum: not ready\n");
    let (code, _, stderr) = output(&mut straum(&["views", "--url", url]));
    assert_eq!(code, Some(3), "{stderr}");
    let query = "SELECT trips FROM trips WHERE bucket >= $1 AND bucket < $2";
    let (code, _, stderr) = output(&mut straum(&["ask", "--url", url, query, "0", "3600"]));
    assert_eq!(code, Some(3), "{stderr}");
}

/// Waits for the replication slots to be gone: a temporary one goes with
/// the connection that made it.
fn no_slot_left(pg: &PostgresContainer) {
    let start = Instant::now();
    while pg.psql("SELECT count(*) FROM pg_replication_slots") != "0" {
        assert!(start.elapsed() < Duration::from_secs(30), "the slot stayed");
        thread::sleep(Duration::from_millis(100));
    }
}

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
    let args = ["run", &spec, "--listen", "127.0.0.1:0"];
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
    let straum = run(&["run", &spec, "--listen", "127.0.0.1:0"], &dsn);
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

#[test]
fn plan_prints_the_setup_sql() {
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    let parsed = straum_notation::parse(SPEC).unwrap();
    let (code, stdout, stderr) = output(&mut straum(&["plan", &spec]));
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    assert_eq!(
        stdout,
        straum_pg::plan(
            &parsed,
            "straum",
            "straum",
            None,
            straum_pg::Replication::Logical
        )
    );
    let mut custom = straum(&["plan", "--publication", "p", &spec, "--role", "r"]);
    let (_, stdout, _) = output(&mut custom);
    assert_eq!(
        stdout,
        straum_pg::plan(&parsed, "p", "r", None, straum_pg::Replication::Logical)
    );

    // With a DSN the role is its user, with its password as a verifier;
    // --role still names the role. The password has a hyphen, which no
    // base64 verifier can contain, so the check can't trip on chance.
    let dsn = scratch.file("dsn", "postgres://reader:plain-pw@db/app\n");
    let mut with_dsn = straum(&["plan", &spec, "--dsn-file", &dsn]);
    let (code, stdout, stderr) = output(&mut with_dsn);
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    let role = "CREATE ROLE \"reader\" LOGIN REPLICATION PASSWORD 'SCRAM-SHA-256$4096:";
    assert!(stdout.contains(role), "{stdout}");
    assert!(
        !stdout.contains("plain-pw") && !stdout.contains("\\password"),
        "{stdout}"
    );
    let mut named = straum(&["plan", &spec, "--role", "r"]);
    named.env("STRAUM_DSN", "postgres://reader@db/app");
    let (_, stdout, _) = output(&mut named);
    assert!(
        stdout.contains("CREATE ROLE \"r\" LOGIN REPLICATION;\n"),
        "{stdout}"
    );
    // A line break in the role would end a comment line and start a
    // statement: refused, from the flag or the DSN's user.
    let broken = "r\nDROP TABLE t; --";
    let (code, stdout, stderr) = output(&mut straum(&["plan", &spec, "--role", broken]));
    assert_eq!((code, stdout.as_str()), (Some(1), ""));
    assert_eq!(
        stderr,
        "straum: the role \"r\\nDROP TABLE t; --\" holds a control character\n"
    );
    let mut from_dsn = straum(&["plan", &spec]);
    from_dsn.env("STRAUM_DSN", "postgres://r%0ADROP@db/app");
    assert_eq!(output(&mut from_dsn).0, Some(1));
    let mut bad = straum(&["plan", &spec]);
    bad.env("STRAUM_DSN", "mysql://reader@db/app");
    let (code, _, stderr) = output(&mut bad);
    assert_eq!(code, Some(1));
    assert_eq!(
        stderr,
        "straum: DSN must start with postgres:// or postgresql://\n"
    );
}

#[test]
fn prints_help_and_version() {
    let (code, stdout, _) = output(&mut straum(&["help"]));
    assert_eq!(code, Some(0));
    assert!(stdout.contains("usage: straum <command>"), "{stdout}");
    let (code, stdout, _) = output(&mut straum(&["version"]));
    assert_eq!(code, Some(0));
    assert_eq!(stdout, format!("straum {}\n", env!("CARGO_PKG_VERSION")));
    for (command, usage) in [
        ("views", "usage: straum views [flags]"),
        ("ask", "usage: straum ask [flags] QUERY [PARAMETER...]"),
        ("shell", "usage: straum shell [flags]"),
    ] {
        let (code, stdout, _) = output(&mut straum(&[command, "--help"]));
        assert_eq!(code, Some(0));
        assert!(stdout.starts_with(usage), "{stdout}");
    }
    let (code, stdout, _) = output(&mut straum(&["plan", "--help"]));
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("usage: straum plan FILE"), "{stdout}");
}

#[test]
fn rejects_bad_usage() {
    let (code, _, stderr) = output(&mut straum(&[]));
    assert_eq!(code, Some(2));
    assert!(stderr.contains("usage: straum <command>"), "{stderr}");
    let (code, _, stderr) = output(&mut straum(&["run", "--port", "1"]));
    assert_eq!(code, Some(2));
    assert_eq!(
        stderr,
        "straum: straum run takes no \"--port\"\ntry: straum run --help\n"
    );
    let (code, _, stderr) = output(&mut straum(&["ask"]));
    assert_eq!(code, Some(2));
    assert_eq!(
        stderr,
        "straum: straum ask needs a query\ntry: straum ask --help\n"
    );
    let (code, _, stderr) = output(&mut straum(&["serve"]));
    assert_eq!(code, Some(2));
    assert_eq!(
        stderr,
        "straum: unknown command \"serve\"\ntry: straum help\n"
    );
}

#[test]
fn says_what_is_wrong() {
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    let bad = scratch.file("bad.sql", "CREATE VIEW v AS SELECT 1;\n");
    let missing = scratch.path.join("missing").to_str().unwrap().to_owned();
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = taken.local_addr().unwrap().to_string();
    let not_found = format!("straum: reading {missing}: No such file or directory (os error 2)\n");

    let cases: &[(&[&str], Option<&str>, String)] = &[
        (&["plan", &missing], None, not_found.clone()),
        (&["plan", &bad], None, format!("straum: {bad}: line 1: ")),
        (
            &["run", &spec],
            None,
            "straum: set STRAUM_DSN or pass --dsn-file\n".to_owned(),
        ),
        (
            &["run", &spec],
            Some("mysql://straum@db/straum"),
            "straum: DSN must start with postgres:// or postgresql://\n".to_owned(),
        ),
        (
            &["run", &spec, "--dsn-file", &missing],
            Some("postgres://straum@db/straum"),
            not_found,
        ),
        (
            &["run", &spec, "--listen", "nowhere"],
            Some("postgres://straum@db/straum"),
            "straum: listening on nowhere: invalid socket address\n".to_owned(),
        ),
        (
            &["run", &spec, "--listen", &taken],
            Some("postgres://straum:secret@db/straum"),
            format!("straum: listening on {taken}: Address already in use"),
        ),
    ];
    for (args, dsn, error) in cases {
        let mut command = straum(args);
        if let Some(dsn) = dsn {
            command.env("STRAUM_DSN", dsn);
        }
        let (code, stdout, stderr) = output(&mut command);
        assert_eq!(code, Some(1), "{args:?}: {stderr}");
        assert_eq!(stdout, "");
        assert!(
            stderr.starts_with(error.as_str()),
            "{args:?}: {stderr:?} is not {error:?}"
        );
        assert!(!stderr.contains("secret"), "{stderr}");
    }
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
            &["run", &spec, "--listen", "127.0.0.1:0"],
            &pg.dsn("straum"),
        );
        straum.wait_for("ready", &Until::Ready);
        let mut held = TcpStream::connect(straum.address).unwrap();
        // A head never ended: hyper waits 30 s for the rest.
        held.write_all(b"POST /ask HTTP/1.1\r\nhost: straum\r\n")
            .unwrap();
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
    for args in [&["help"][..], &["version"], &["plan", &spec]] {
        let (reader, writer) = io::pipe().unwrap();
        drop(reader);
        let status = straum(args).stdout(writer).status().unwrap();
        assert_eq!(status.code(), Some(0), "{args:?}");
    }
    let (reader, writer) = io::pipe().unwrap();
    drop(reader);
    let mut child = straum(&["run", &spec, "--listen", "127.0.0.1:0"])
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
