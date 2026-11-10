//! The straum binary end to end: `straum plan` sets up PostgreSQL 18 in Docker,
//! `straum run` loads and follows it as the role the plan made, and asks over
//! HTTP answer what the view body answers in PostgreSQL.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use testcontainers::core::{ExecCommand, IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};

const STRAUM: &str = env!("CARGO_BIN_EXE_straum");
const READY: &str = "database system is ready to accept connections";

const SPEC: &str = "
CREATE VIEW trips WITH (key = id) AS
SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket,
       t.kind,
       t.zone_id,
       zone.borough,
       count(*) AS trips,
       sum(t.fare) AS fare
FROM shop.trips t
LEFT JOIN shop.zones zone ON zone.id = t.zone_id
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
/// rows in straum's order.
const EXPECTED: &str = r#"
SELECT coalesce(string_agg(row, ',' ORDER BY kind COLLATE "C" NULLS FIRST,
                                             borough COLLATE "C" NULLS FIRST), '')
  FROM (SELECT kind, borough,
               json_build_object('kind', kind, 'zone.borough', borough,
                                 'trips', count(*), 'fare', sum(fare))::text AS row
          FROM shop.trips t
          LEFT JOIN shop.zones zone ON zone.id = t.zone_id
         GROUP BY 1, 2) v
"#;

const ASK: &str = concat!(
    r#"{"query": "SELECT kind, zone.borough, trips, fare FROM trips "#,
    r#"WHERE bucket >= $1 AND bucket < $2 GROUP BY kind, zone.borough", "#,
    r#""parameters": [0, "2100-01-01T00:00:00Z"]}"#
);

struct Postgres {
    container: Container<GenericImage>,
    port: u16,
}

/// CI sets `STRAUM_TEST_POSTGRES` to each major straum supports.
fn tag() -> String {
    std::env::var("STRAUM_TEST_POSTGRES").unwrap_or_else(|_| "18".to_owned())
}

fn postgres() -> Postgres {
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
    Postgres { container, port }
}

impl Postgres {
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
        self.psql(EXPECTED).replace(", ", ",").replace(" : ", ":")
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
}

fn run(args: &[&str], dsn: &str) -> Running {
    let mut child = straum(args)
        .env("STRAUM_DSN", dsn)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let log = Arc::new(Mutex::new(String::new()));
    let lines = BufReader::new(child.stderr.take().unwrap());
    let gathered = Arc::clone(&log);
    thread::spawn(move || {
        for line in lines.lines() {
            let mut log = gathered.lock().unwrap();
            log.push_str(&line.unwrap());
            log.push('\n');
        }
    });
    let mut running = Running {
        child,
        address: "0.0.0.0:0".parse().unwrap(),
        log,
    };
    running.wait_for("straum to listen", |straum| {
        straum.log().contains("listening on ")
    });
    let log = running.log();
    let at = log.find("listening on ").unwrap() + "listening on ".len();
    running.address = log[at..].lines().next().unwrap().parse().unwrap();
    running
}

impl Running {
    fn log(&self) -> String {
        self.log.lock().unwrap().clone()
    }

    fn wait_for(&mut self, what: &str, done: impl Fn(&mut Running) -> bool) {
        let start = Instant::now();
        while !done(self) {
            assert!(
                start.elapsed() < Duration::from_secs(60),
                "waited a minute for {what}; straum's log:\n{}",
                self.log()
            );
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "straum exited; its log:\n{}",
                self.log()
            );
            thread::sleep(Duration::from_millis(100));
        }
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

    /// Sends the signal and waits for straum to exit.
    fn stop(mut self, signal: &str) -> (Option<i32>, String) {
        let pid = self.child.id().to_string();
        let sent = Command::new("kill").args([signal, &pid]).status().unwrap();
        assert!(sent.success());
        let status = self.child.wait().unwrap();
        // The log thread ends with the pipe.
        thread::sleep(Duration::from_millis(100));
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
/// returns. Gives the role `straum` a password.
fn set_up(pg: &Postgres, spec: &str, hold: &str) -> Vec<String> {
    let (code, plan, _) = output(&mut straum(&["plan", "--spec", spec]));
    assert_eq!(code, Some(0));
    let mut held = Vec::new();
    for statement in plan.lines().filter(|l| !l.starts_with("--")) {
        if statement.starts_with(hold) {
            held.push(statement.to_owned());
        } else {
            pg.psql(statement);
        }
    }
    pg.psql("ALTER ROLE straum PASSWORD 'straum'");
    held
}

#[test]
fn serves_asks_from_postgres() {
    let pg = postgres();
    pg.psql(TABLES);
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    assert_eq!(set_up(&pg, &spec, "nothing"), Vec::<String>::new());
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
    let counts = "\nstarts:       1\nfailures:     0\nlast failure: none\n";
    assert!(status.ends_with(counts), "{status}");

    let (status, body) = straum.call("POST", "/ask", ASK);
    assert_eq!(status, 200, "{body}");
    assert!(body.starts_with(r#"{"as_of":"20"#), "{body}");
    assert!(!body.contains("lsn"), "{body}");
    let rows = format!("\"rows\":[{}]}}", pg.expected_rows());
    assert!(body.ends_with(&rows), "{body}\nis not\n{rows}");
    let cab = r#"{"kind":"cab","zone.borough":"Bronx","trips":1,"fare":12.50}"#;
    assert!(body.contains(cab), "{body}");

    pg.psql("INSERT INTO shop.trips VALUES (5, '2024-01-02 00:00Z', 'cab', 2, 1.25)");
    pg.psql("UPDATE shop.zones SET borough = 'Brooklyn' WHERE id = 2");
    pg.psql("DELETE FROM shop.trips WHERE id = 3");
    pg.psql("UPDATE shop.trips SET fare = 10 WHERE id = 1");
    let rows = format!("\"rows\":[{}]}}", pg.expected_rows());
    straum.wait_for("the changes", |straum| {
        straum.call("POST", "/ask", ASK).1.ends_with(&rows)
    });

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
    // The temporary slot went with straum's connection.
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
    let publication = set_up(&pg, &spec, "CREATE PUBLICATION");
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
    assert_eq!(stdout, straum_pg::plan(&parsed, "straum", "straum"));
    let mut custom = straum(&["plan", "--publication", "p", "--spec", &spec, "--role", "r"]);
    let (_, stdout, _) = output(&mut custom);
    assert_eq!(stdout, straum_pg::plan(&parsed, "p", "r"));
}

#[test]
fn says_what_is_wrong() {
    let scratch = Scratch::new();
    let spec = scratch.file("spec.sql", SPEC);
    let bad = scratch.file("bad.sql", "CREATE VIEW v AS SELECT 1;\n");
    let missing = scratch.0.join("missing").to_str().unwrap().to_owned();
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let taken = taken.local_addr().unwrap().to_string();

    let (code, stdout, _) = output(&mut straum(&["help"]));
    assert_eq!(code, Some(0));
    assert!(
        stdout.starts_with("usage:\n  straum run --spec FILE"),
        "{stdout}"
    );

    let (code, _, stderr) = output(&mut straum(&[]));
    assert_eq!(code, Some(2));
    assert!(
        stderr.starts_with("straum: no command\n\nusage:\n"),
        "{stderr}"
    );
    let (code, _, stderr) = output(&mut straum(&["run", "--port", "1"]));
    assert_eq!(code, Some(2));
    assert!(
        stderr.starts_with("straum: straum run takes no \"--port\"\n"),
        "{stderr}"
    );

    let cases: [(Vec<&str>, Option<&str>, String); 7] = [
        (
            vec!["plan", "--spec", &missing],
            None,
            format!("straum: reading {missing}: No such file or directory (os error 2)\n"),
        ),
        (
            vec!["plan", "--spec", &bad],
            None,
            format!("straum: {bad}: line 1: "),
        ),
        (
            vec!["run", "--spec", &spec],
            None,
            "straum: set STRAUM_DSN or pass --dsn-file\n".to_owned(),
        ),
        (
            vec!["run", "--spec", &spec],
            Some("mysql://straum@db/straum"),
            "straum: DSN must start with postgres:// or postgresql://\n".to_owned(),
        ),
        (
            vec!["run", "--spec", &spec, "--dsn-file", &missing],
            Some("postgres://straum@db/straum"),
            format!("straum: reading {missing}: No such file or directory (os error 2)\n"),
        ),
        (
            vec!["run", "--spec", &spec, "--listen", "nowhere"],
            Some("postgres://straum@db/straum"),
            "straum: listening on nowhere: invalid socket address\n".to_owned(),
        ),
        (
            vec!["run", "--spec", &spec, "--listen", &taken],
            Some("postgres://straum:secret@db/straum"),
            format!("straum: listening on {taken}: Address already in use"),
        ),
    ];
    for (args, dsn, error) in cases {
        let mut command = straum(&args);
        if let Some(dsn) = dsn {
            command.env("STRAUM_DSN", dsn);
        }
        let (code, stdout, stderr) = output(&mut command);
        assert_eq!(code, Some(1), "{args:?}: {stderr}");
        assert_eq!(stdout, "");
        assert!(
            stderr.starts_with(&error),
            "{args:?}: {stderr:?} is not {error:?}"
        );
        assert!(!stderr.contains("secret"), "{stderr}");
    }
}
