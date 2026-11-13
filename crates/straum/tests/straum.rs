//! The Straum binary end to end: `straum plan` sets up PostgreSQL 18 in Docker,
//! `straum run` loads and follows it as the role the plan made, and asks over
//! HTTP answer what the view body answers in PostgreSQL. With `--feed` the
//! same rows come from a feeder, and no PostgreSQL runs.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use straum_domain::{ColumnType, Datum};
use straum_feed::{Feeder, Request};
use testcontainers::core::{ExecCommand, IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};

const STRAUM: &str = env!("CARGO_BIN_EXE_straum");
const READY: &str = "database system is ready to accept connections";

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
    container: Container<GenericImage>,
    port: u16,
}

/// CI sets `STRAUM_TEST_POSTGRES` to each major Straum supports.
fn tag() -> String {
    std::env::var("STRAUM_TEST_POSTGRES").unwrap_or_else(|_| "18".to_owned())
}

fn postgres() -> PostgresContainer {
    let container = GenericImage::new("postgres", tag().as_str())
        .with_exposed_port(5432.tcp())
        // The init server says it on stdout, the real one on stderr.
        .with_wait_for(WaitFor::message_on_stdout(READY))
        .with_wait_for(WaitFor::message_on_stderr(READY))
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .with_env_var("POSTGRES_HOST_AUTH_METHOD", "scram-sha-256")
        .with_cmd(["-c", "wal_level=logical"])
        .start()
        .expect("Docker runs postgres");
    // Docker may publish the port a moment after the container starts.
    let port = (0..50)
        .find_map(|_| {
            let port = container.get_host_port_ipv4(5432).ok();
            if port.is_none() {
                thread::sleep(Duration::from_millis(100));
            }
            port
        })
        .expect("Docker publishes port 5432");
    PostgresContainer { container, port }
}

impl PostgresContainer {
    /// One statement at a time, as the superuser; the output unaligned.
    fn psql(&self, sql: &str) -> String {
        let mut result = self
            .container
            .exec(ExecCommand::new([
                "psql",
                "postgres://postgres:postgres@127.0.0.1/postgres",
                "-At",
                "-v",
                "ON_ERROR_STOP=1",
                "-c",
                sql,
            ]))
            .unwrap();
        let stdout = String::from_utf8(result.stdout_to_vec().unwrap()).unwrap();
        let mut stderr = String::new();
        result.stderr().read_to_string(&mut stderr).unwrap();
        assert_eq!(result.exit_code().unwrap(), Some(0), "{sql}: {stderr}");
        stdout.trim_end().to_owned()
    }

    fn dsn(&self, user: &str) -> String {
        format!(
            "postgres://{user}:{user}@127.0.0.1:{}/postgres?sslmode=disable",
            self.port
        )
    }

    fn expected_rows(&self) -> String {
        let rows = self.psql(EXPECTED).replace(", ", ",").replace(" : ", ":");
        format!("\"rows\":[{rows}]}}")
    }
}

/// A directory of the test's own, removed at the end.
struct Scratch(PathBuf);

static SCRATCH: AtomicUsize = AtomicUsize::new(0);

impl Scratch {
    fn new() -> Scratch {
        let n = SCRATCH.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("straum-test-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Scratch(path)
    }

    fn file(&self, name: &str, text: &str) -> String {
        let path = self.0.join(name);
        fs::write(&path, text).unwrap();
        path.to_str().unwrap().to_owned()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).ok();
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

/// `straum run`, its log gathered as it comes.
struct Running {
    child: Child,
    address: SocketAddr,
    log: Arc<Mutex<String>>,
    gatherer: Option<JoinHandle<()>>,
}

fn run(args: &[&str], dsn: &str) -> Running {
    started(straum(args).env("STRAUM_DSN", dsn))
}

/// Starts `straum run` and waits until it listens.
fn started(command: &mut Command) -> Running {
    let mut child = command.stderr(Stdio::piped()).spawn().unwrap();
    let log = Arc::new(Mutex::new(String::new()));
    let lines = BufReader::new(child.stderr.take().unwrap());
    let gathered = Arc::clone(&log);
    let gatherer = thread::spawn(move || {
        for line in lines.lines() {
            let mut log = gathered.lock().unwrap();
            log.push_str(&line.unwrap());
            log.push('\n');
        }
    });
    let start = Instant::now();
    while !log.lock().unwrap().contains("listening on ") {
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
        gatherer: Some(gatherer),
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

    fn wait_for(&mut self, what: &str, done: impl Fn(&Running) -> bool) {
        let start = Instant::now();
        while !done(self) {
            wait(&mut self.child, &self.log, what, start);
        }
    }

    fn wait_for_rows(&mut self, pg: &PostgresContainer, what: &str) {
        let rows = pg.expected_rows();
        self.wait_for(what, |straum| {
            straum.call("POST", "/ask", ASK).1.ends_with(&rows)
        });
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
        (head[9..12].parse().unwrap(), body.to_owned())
    }

    /// Sends the signal and waits for Straum to exit.
    fn stop(mut self, signal: &str) -> (Option<i32>, String) {
        let pid = self.child.id().to_string();
        let sent = Command::new("kill").args([signal, &pid]).status().unwrap();
        assert!(sent.success());
        let status = self.child.wait().unwrap();
        // The gatherer ends with the pipe, so the log is whole once it joins.
        self.gatherer.take().unwrap().join().unwrap();
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
    let mut plan = straum(&["plan", "--spec", spec]);
    plan.env("STRAUM_DSN", pg.dsn("straum"));
    let (code, plan, _) = output(&mut plan);
    assert_eq!(code, Some(0));
    let mut held = Vec::new();
    for statement in plan.lines().filter(|l| !l.starts_with("--")) {
        if hold.is_some_and(|hold| statement.starts_with(hold)) {
            held.push(statement.to_owned());
        } else {
            pg.psql(statement);
        }
    }
    held
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
        &[
            "run",
            "--spec",
            &spec,
            "--listen",
            "127.0.0.1:0",
            "--dsn-file",
            &dsn,
        ],
        "not a DSN",
    );
    straum.wait_for("ready", |straum| straum.call("GET", "/readyz", "").0 == 200);

    // What `straum status` says, as from inside the pod.
    let url = format!("http://{}", straum.address);
    let (code, status, _) = output(Command::new(STRAUM).args(["status", "--url", &url]));
    assert_eq!(code, Some(0), "{status}");
    assert!(
        status.starts_with("ready:        yes\napplied:      "),
        "{status}"
    );
    let counts = "\nstarts:       1\nfailures:     0\nlast failure: none\nsealed:       no spill\n";
    assert!(status.ends_with(counts), "{status}");

    let (code, views, _) = output(Command::new(STRAUM).args(["views", "--url", &url]));
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
"
    );

    // The range `straum views` gave, as `straum ask` takes it.
    let query = "SELECT kind, borough, trips, fare FROM trips \
                 WHERE bucket >= $1 AND bucket < $2 GROUP BY kind, borough";
    let range = ["2024-01-01T00:00:00Z", "2024-01-01T04:00:00Z"];
    let (code, table, _) =
        output(Command::new(STRAUM).args(["ask", "--url", &url, query, range[0], range[1]]));
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
        output(Command::new(STRAUM).args(["ask", "--url", &url, wrong, "7", "0", "3600"]));
    assert_eq!(code, Some(1), "{stderr}");
    assert_eq!(
        stderr,
        format!(
            "straum: {url} answered \"HTTP/1.1 400 Bad Request\"\n\
             kind is text, so it can't be the integer 7\n"
        )
    );

    // The same from `straum shell`, a line each, on a pipe.
    let mut shell = Command::new(STRAUM)
        .args(["shell", "--url", &url])
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

    let (status, body) = straum.call("POST", "/ask", ASK);
    assert_eq!(status, 200, "{body}");
    assert!(body.starts_with(r#"{"as_of":"20"#), "{body}");
    assert!(!body.contains("lsn"), "{body}");
    let rows = pg.expected_rows();
    assert!(body.ends_with(&rows), "{body}\nis not\n{rows}");
    let cab = r#"{"kind":"cab","borough":"Bronx","trips":1,"fare":12.50}"#;
    assert!(body.contains(cab), "{body}");

    pg.psql("INSERT INTO shop.trips VALUES (5, '2024-01-02 00:00Z', 'cab', 2, 1.25)");
    pg.psql("DELETE FROM shop.trips WHERE id = 3");
    pg.psql("UPDATE shop.trips SET fare = 10 WHERE id = 1");
    straum.wait_for_rows(&pg, "the changes");

    // A zone renamed shows in the next answer: the dim is held and
    // followed, so nothing starts over.
    pg.psql("UPDATE shop.zones SET borough = 'Brooklyn' WHERE id = 2");
    straum.wait_for_rows(&pg, "the dim change");
    pg.psql("UPDATE shop.trips SET fare = 20 WHERE id = 2");
    straum.wait_for_rows(&pg, "the trip change");
    assert!(!straum.log().contains("no longer fits"), "{}", straum.log());

    let (code, log) = straum.stop("-TERM");
    assert_eq!(code, Some(0), "{log}");
    assert!(
        log.contains("straum: loaded every table; streaming from "),
        "{log}"
    );
    assert!(
        log.ends_with("straum: stopping: finishing the asks in flight\nstraum: stopped\n"),
        "{log}"
    );
    // The temporary slot went with Straum's connection.
    let start = Instant::now();
    while pg.psql("SELECT count(*) FROM pg_replication_slots") != "0" {
        assert!(start.elapsed() < Duration::from_secs(30), "the slot stayed");
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn waits_for_the_setup() {
    let pg = postgres();
    pg.psql(TABLES);
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    let publication = set_up(&pg, &spec, Some("CREATE PUBLICATION"));
    let mut straum = run(
        &["run", "--spec", &spec, "--listen", "127.0.0.1:0"],
        &pg.dsn("straum"),
    );
    // Each try waits twice as long as the one before.
    straum.wait_for("two failed starts", |straum| {
        straum.log().contains("starting over in 2 s")
    });
    assert!(straum.log().contains("starting over in 1 s"));
    let (status, body) = straum.call("GET", "/readyz", "");
    assert_eq!(status, 503);
    assert!(body.starts_with("not ready: "), "{body}");
    assert!(body.contains("publication"), "{body}");
    let (status, body) = straum.call("POST", "/ask", ASK);
    assert_eq!(
        (status, body.as_str()),
        (503, r#"{"errors":["not ready"]}"#)
    );

    // Once the setup is there, the next try starts.
    pg.psql(&publication[0]);
    straum.wait_for("ready", |straum| straum.call("GET", "/readyz", "").0 == 200);
    let (status, body) = straum.call("POST", "/ask", ASK);
    assert_eq!(status, 200, "{body}");
    let (code, log) = straum.stop("-INT");
    assert_eq!(code, Some(0), "{log}");
}

#[test]
fn plan_prints_the_setup_sql() {
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    let parsed = straum_notation::parse(SPEC).unwrap();
    let (code, stdout, stderr) = output(&mut straum(&["plan", "--spec", &spec]));
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    assert_eq!(stdout, straum_pg::plan(&parsed, "straum", "straum", None));
    let mut custom = straum(&["plan", "--publication", "p", "--spec", &spec, "--role", "r"]);
    let (_, stdout, _) = output(&mut custom);
    assert_eq!(stdout, straum_pg::plan(&parsed, "p", "r", None));

    // With a DSN the role is its user, with its password as a verifier;
    // --role still names the role. The password has a hyphen, which no
    // base64 verifier can contain, so the check can't trip on chance.
    let dsn = scratch.file("dsn", "postgres://reader:plain-pw@db/app\n");
    let mut with_dsn = straum(&["plan", "--spec", &spec, "--dsn-file", &dsn]);
    let (code, stdout, stderr) = output(&mut with_dsn);
    assert_eq!((code, stderr.as_str()), (Some(0), ""));
    let role = "CREATE ROLE \"reader\" LOGIN REPLICATION PASSWORD 'SCRAM-SHA-256$4096:";
    assert!(stdout.contains(role), "{stdout}");
    assert!(
        !stdout.contains("plain-pw") && !stdout.contains("\\password"),
        "{stdout}"
    );
    let mut named = straum(&["plan", "--spec", &spec, "--role", "r"]);
    named.env("STRAUM_DSN", "postgres://reader@db/app");
    let (_, stdout, _) = output(&mut named);
    assert!(
        stdout.contains("CREATE ROLE \"r\" LOGIN REPLICATION;\n"),
        "{stdout}"
    );
    let mut bad = straum(&["plan", "--spec", &spec]);
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
    assert!(
        stdout.starts_with("usage: straum plan --spec FILE"),
        "{stdout}"
    );
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
    let missing = scratch.0.join("missing").to_str().unwrap().to_owned();
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = taken.local_addr().unwrap().to_string();
    let not_found = format!("straum: reading {missing}: No such file or directory (os error 2)\n");

    let cases: &[(&[&str], Option<&str>, String)] = &[
        (&["plan", "--spec", &missing], None, not_found.clone()),
        (
            &["plan", "--spec", &bad],
            None,
            format!("straum: {bad}: line 1: "),
        ),
        (
            &["run", "--spec", &spec],
            None,
            "straum: set STRAUM_DSN or pass --dsn-file\n".to_owned(),
        ),
        (
            &["run", "--spec", &spec],
            Some("mysql://straum@db/straum"),
            "straum: DSN must start with postgres:// or postgresql://\n".to_owned(),
        ),
        (
            &["run", "--spec", &spec, "--dsn-file", &missing],
            Some("postgres://straum@db/straum"),
            not_found,
        ),
        (
            &["run", "--spec", &spec, "--listen", "nowhere"],
            Some("postgres://straum@db/straum"),
            "straum: listening on nowhere: invalid socket address\n".to_owned(),
        ),
        (
            &["run", "--spec", &spec, "--listen", &taken],
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

/// The rows of `TABLES`, as a feeder holds them: each table's columns
/// with their types, and its rows.
type Held = (
    &'static str,
    Vec<(&'static str, ColumnType)>,
    Vec<Vec<Datum>>,
);

fn shop() -> Vec<Held> {
    let text = |text: &str| Datum::Text(text.into());
    let (int, null) = (Datum::Int, Datum::Null);
    // 2024-01-01 00:10 UTC, and fares in cents as a numeric(8, 2) reads.
    let at = |hour: i64| int(1_704_067_800 + hour * 3_600);
    vec![
        (
            "zones",
            vec![("id", ColumnType::Int), ("borough", ColumnType::Text)],
            vec![vec![int(1), text("Bronx")], vec![int(2), text("Queens")]],
        ),
        (
            "trips",
            vec![
                ("id", ColumnType::Int),
                ("at", ColumnType::Time { zoned: true }),
                ("kind", ColumnType::Text),
                ("zone_id", ColumnType::Int),
                ("fare", ColumnType::Numeric { scale: 2 }),
            ],
            vec![
                vec![int(1), at(0), text("cab"), int(1), int(1_250)],
                vec![int(2), at(1), text("cab"), int(2), int(5)],
                vec![int(3), at(2), text("van"), int(3), null.clone()],
                vec![int(4), at(3), null, int(1), int(-225)],
            ],
        ),
    ]
}

/// Connects to the feed and answers every table asked for from `held`.
fn feed(socket: &str, held: &[Held]) -> Result<(), straum_feed::Error> {
    let mut feeder = Feeder::connect(socket)?;
    loop {
        let (table, columns) = match feeder.next_request()? {
            Request::Done => return Ok(()),
            Request::Load { table, columns } => (table, columns),
        };
        let Some((_, typed, rows)) = held.iter().find(|held| held.0 == table.name.as_str()) else {
            return feeder.fail(&format!("no table {}.{}", table.schema, table.name));
        };
        let at: Vec<usize> = (columns.iter())
            .map(|column| typed.iter().position(|c| c.0 == column.as_str()).unwrap())
            .collect();
        feeder.types(&at.iter().map(|&at| typed[at].1).collect::<Vec<_>>())?;
        for row in rows {
            feeder.rows(&at.iter().map(|&at| row[at].clone()).collect::<Vec<_>>())?;
        }
        feeder.end()?;
    }
}

/// What `ASK` answers for the rows of `TABLES`.
const FED_ROWS: &str = concat!(
    r#""rows":[{"kind":null,"borough":"Bronx","trips":1,"fare":-2.25},"#,
    r#"{"kind":"cab","borough":"Bronx","trips":1,"fare":12.50},"#,
    r#"{"kind":"cab","borough":"Queens","trips":1,"fare":0.05},"#,
    r#"{"kind":"van","borough":null,"trips":1,"fare":null}]}"#
);

#[test]
fn serves_asks_from_a_feed() {
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    let socket = scratch.0.join("feed").to_str().unwrap().to_owned();
    let args = [
        "run",
        "--spec",
        &spec,
        "--listen",
        "127.0.0.1:0",
        "--feed",
        &socket,
    ];
    // A DSN in the environment is PostgreSQL's, and a feed needs none.
    let mut straum = started(straum(&args).env("STRAUM_DSN", "postgres://nobody@127.0.0.1:1/x"));
    assert!(
        straum
            .log()
            .contains(&format!("waiting for a feeder on {socket}"))
    );
    assert_eq!(
        straum.call("GET", "/readyz", ""),
        (503, "not ready: loading\n".to_owned())
    );
    assert_eq!(straum.call("POST", "/ask", ASK).0, 503);

    // A feeder without the trips: Straum says why it is not ready, and
    // waits for the next one.
    feed(&socket, &shop()[..1]).unwrap();
    straum.wait_for("the failure", |straum| {
        straum.call("GET", "/readyz", "").1 == "not ready: the feeder: no table shop.trips\n"
    });

    feed(&socket, &shop()).unwrap();
    straum.wait_for("the load", |straum| {
        straum.call("GET", "/readyz", "").0 == 200
    });
    let (code, body) = straum.call("POST", "/ask", ASK);
    assert_eq!(code, 200);
    assert!(body.ends_with(FED_ROWS), "{body}");
    assert!(straum.log().contains("loaded every table from the feed"));

    let url = format!("http://{}", straum.address);
    let (code, status, _) = output(&mut self::straum(&["status", "--url", &url]));
    assert_eq!(code, Some(0));
    assert!(
        status.starts_with("ready:        yes\napplied:      0/0"),
        "{status}"
    );
    assert!(
        status.contains("\nstarts:       1\nfailures:     1\n"),
        "{status}"
    );
    let (code, views, _) = output(&mut self::straum(&["views", "--url", &url]));
    assert_eq!(code, Some(0));
    assert!(views.contains("holds:    4 cells"), "{views}");
    assert!(
        views.contains("dim shop.zones\n  holds:    2 rows"),
        "{views}"
    );

    let (code, log) = straum.stop("-TERM");
    assert_eq!(code, Some(0));
    assert!(log.ends_with("straum: stopped\n"), "{log}");

    // The socket the first left behind is no one's, so the next takes it.
    let straum = started(&mut self::straum(&args));
    feed(&socket, &shop()).unwrap();
    assert_eq!(straum.call("GET", "/livez", "").0, 200);
}

#[test]
fn a_feed_that_cannot_listen_stops_straum() {
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    let socket = scratch.0.join("no/such/directory/feed");
    let socket = socket.to_str().unwrap();
    let mut run = straum(&[
        "run",
        "--spec",
        &spec,
        "--listen",
        "127.0.0.1:0",
        "--feed",
        socket,
    ]);
    let (code, _, stderr) = output(&mut run);
    assert_eq!(code, Some(1));
    assert!(
        stderr.starts_with(&format!("straum: listening on {socket}: ")),
        "{stderr}"
    );

    let mut run = straum(&["run", "--spec", &spec, "--feed", socket, "--max-lag", "5"]);
    let (code, _, stderr) = output(&mut run);
    assert_eq!(code, Some(2));
    assert_eq!(
        stderr,
        "straum: --feed takes PostgreSQL's place, so --max-lag does not apply\n\
         try: straum run --help\n"
    );
}
