//! The Straum binary end to end, run as a child process. `straum plan`
//! sets up PostgreSQL in Docker, `straum run` loads and follows it as the
//! role the plan made, and asks over HTTP, `straum ask` and `straum shell`
//! answer what the view body answers in PostgreSQL. The tests without a
//! database check help, usage and the errors a person reads. A script's
//! view is checked too: `--json`, `STRAUM_URL` and exit 3 while not ready.
//! This file holds PostgreSQL, the child process and the request helpers;
//! the tests are split by subject: `serve.rs` a Straum serving a database
//! through every command, `stops.rs` how it fails and stops, `plan.rs`
//! `straum plan`, and `usage.rs` help, usage and errors.

mod plan;
mod serve;
mod stops;
mod usage;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::ops::Range;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

// straum-pg's tests start PostgreSQL the same way; the file uses std alone.
#[path = "../../../straum-pg/src/tests/compose.rs"]
#[expect(
    dead_code,
    reason = "these tests set no entrypoint, sysctl or shared memory"
)]
mod compose;

use compose::{Service, Stack};

const STRAUM: &str = env!("CARGO_BIN_EXE_straum");

const SPEC: &str = "
CREATE VIEW trips AS
SELECT buckets(t.at, '1 hour') AS bucket,
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
        format!("\"rows\":[{rows}]}}\n")
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
    for variable in [
        "PGHOST",
        "PGHOSTADDR",
        "PGPORT",
        "PGUSER",
        "PGPASSWORD",
        "PGDATABASE",
        "PGSSLMODE",
        "PGSSLROOTCERT",
    ] {
        command.env_remove(variable);
    }
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

    /// The status and body, the body sent as JSON.
    fn call(&self, method: &str, path: &str, body: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(self.address).unwrap();
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nhost: {}\r\nconnection: close\r\n\
             content-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
            self.address,
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

/// Waits for the replication slots to be gone: a temporary one goes with
/// the connection that made it.
fn no_slot_left(pg: &PostgresContainer) {
    let start = Instant::now();
    while pg.psql("SELECT count(*) FROM pg_replication_slots") != "0" {
        assert!(start.elapsed() < Duration::from_secs(30), "the slot stayed");
        thread::sleep(Duration::from_millis(100));
    }
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
