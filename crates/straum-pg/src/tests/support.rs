//! What every PostgreSQL test shares: the containers PostgreSQL runs in,
//! `Db`, a SQL connection to one test's schema, `Rng`, the seeded random
//! numbers workloads draw from, and a fake server that plays back scripted
//! bytes of the PostgreSQL wire protocol (the messages a client and
//! server exchange) for the error paths a real server never takes.

use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::path::{Path, PathBuf};
use std::process;
use std::thread;
use std::time::Duration;

use straum_app::{App, Limits, Lsn, Step, Writer};
use straum_domain::{
    Ask, AskKey, Comparison, Condition, Datum, Identifier, Literal, MeasureCondition, Spec, Value,
    decimal,
};
use testcontainers::core::{ExecCommand, IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, ContainerRequest, GenericImage, ImageExt};

use crate::dsn::Dsn;
use crate::error::Error;
use crate::source::{Postgres, Replication, parse_lsn};
use crate::wal::RelationFile;
use crate::wal::test_support::Rng as SplitMix;
use crate::wire::{Connection, ConnectionKind};

/// The password of the `postgres` role in every container.
pub(super) const PASSWORD: &str = "straum";
/// Long enough for anything a test waits on to come.
pub(super) const LONG: Duration = Duration::from_secs(10);
/// The `--max-retained-wal` the tests run under: 4 GiB, more than any test
/// writes.
pub(super) const MAX_RETAINED_WAL_BYTES: u64 = 4 << 30;
/// The limits every test's app runs under.
pub(super) const LIMITS: Limits = Limits {
    max_lag: Duration::from_secs(30),
    max_retained_wal: MAX_RETAINED_WAL_BYTES,
};
/// Rows a transaction may change before Straum starts over; no test but
/// the one for it comes near.
pub(super) const ROW_LIMIT: usize = 10_000;
/// The rows a physical stream may hold in these tests.
pub(super) const PAGE_MEMORY: usize = 256 << 20;
/// The physical stream, with the tests' page memory.
pub(super) const PHYSICAL: Replication = Replication::Physical {
    max_page_memory: PAGE_MEMORY,
};
/// What PostgreSQL logs once it takes connections.
const READY: &str = "database system is ready to accept connections";
/// 2100-01-01, on every bucket edge.
pub(super) const END: i64 = 4_102_444_800;
/// 2024-01-15, where test rows start.
pub(super) const START: i64 = 1_705_276_800;
/// From 5 hours before `START` to 29 after: the day bucket `START` begins
/// and the hours at either end.
pub(super) const WINDOW_FROM: i64 = START - 5 * 3_600;
/// The end of the window [`WINDOW_FROM`] starts.
pub(super) const WINDOW_TO: i64 = START + 29 * 3_600;

/// The boroughs zones lie in. The last of each list holds what `COPY`
/// escapes, so loads read escapes back.
pub(super) const BOROUGHS: [&str; 4] = ["Manhattan", "Queens", "Bronx", "Bø\tro\\ugh\n"];
/// The kinds of trip.
pub(super) const KINDS: [&str; 4] = ["cab", "pool", "van", "e\\bike\r\x08\x0b\x0c"];
/// The riders a trip names.
const RIDERS: [&str; 4] = ["ann", "bo", "cy", "di"];

/// Logical decoding and a short `wal_sender_timeout`.
const FLAGS: [&str; 4] = ["-c", "wal_level=logical", "-c", "wal_sender_timeout=4s"];

/// The first major where `wal_level = replica` is enough; `19rc2` until 19
/// ships.
pub(super) const NINETEEN: &str = "19rc2";

/// Makes a CA and a certificate it signs for 127.0.0.1, then starts
/// PostgreSQL with ssl on. Both are valid from 2024, whatever the clocks.
const WITH_TLS: &str = r#"
set -e
cd /tmp
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -subj /CN=straum-test-ca -not_before 20240101000000Z -days 36500 \
    -keyout ca.key -out ca.pem
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj /CN=straum \
    -keyout server.key -out server.csr
printf 'subjectAltName=IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' > server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -extfile server.ext \
    -not_before 20240101000000Z -days 36500 -out server.pem
chown postgres server.key server.pem
chmod 600 server.key
exec docker-entrypoint.sh postgres -c ssl=on \
    -c ssl_cert_file=/tmp/server.pem -c ssl_key_file=/tmp/server.key "$@"
"#;

/// PostgreSQL, removed when dropped, with its CA file if it has TLS.
pub(crate) struct PostgresContainer {
    pub(crate) container: Container<GenericImage>,
    pub(crate) dsn: Dsn,
    pub(super) _ca: Option<TempFile>,
}

impl PostgresContainer {
    /// What `pg_waldump` prints of the server's WAL directory, read in
    /// `directory`, with `arguments` after it.
    pub(super) fn waldump(&self, directory: &str, arguments: &[String]) -> String {
        let mut command = vec![
            "pg_waldump".to_owned(),
            "-p".to_owned(),
            format!("{directory}/pg_wal"),
        ];
        command.extend_from_slice(arguments);
        let mut waldump = self.container.exec(ExecCommand::new(command)).unwrap();
        String::from_utf8(waldump.stdout_to_vec().unwrap()).unwrap()
    }
}

/// A file in the temp directory, removed when dropped.
pub(super) struct TempFile(pub(super) PathBuf);

impl TempFile {
    /// Writes `contents` to a file named after `name` and this process.
    pub(super) fn new(name: &str, contents: &[u8]) -> TempFile {
        let path = std::env::temp_dir().join(format!("straum-{}-{name}", process::id()));
        fs::write(&path, contents).unwrap();
        TempFile(path)
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// The PostgreSQL major to test. CI sets `STRAUM_TEST_POSTGRES` to each
/// major Straum supports.
pub(crate) fn tag() -> String {
    let Ok(tag) = std::env::var("STRAUM_TEST_POSTGRES") else {
        return "18".to_owned();
    };
    tag
}

/// PostgreSQL with logical decoding.
pub(super) fn postgres() -> PostgresContainer {
    plain(image(&tag()).with_cmd(FLAGS))
}

/// PostgreSQL `tag` at `wal_level = replica`, which logs no logical
/// decoding.
pub(super) fn postgres_at_replica(tag: &str) -> PostgresContainer {
    plain(image(tag).with_cmd(["-c", "wal_level=replica"]))
}

/// PostgreSQL as `request` starts it, without TLS.
pub(crate) fn plain(request: ContainerRequest<GenericImage>) -> PostgresContainer {
    let Running { container, port } = start(request);
    let dsn = format!("postgres://postgres:{PASSWORD}@127.0.0.1:{port}/postgres?sslmode=disable");
    PostgresContainer {
        container,
        dsn: Dsn::parse(&dsn).unwrap(),
        _ca: None,
    }
}

/// As [`postgres`], with ssl on. The DSN trusts the test's CA alone.
pub(super) fn postgres_tls() -> PostgresContainer {
    let [setting, wal_level, next_setting, timeout] = FLAGS.map(str::to_owned);
    let arguments = [
        "-c".into(),
        WITH_TLS.into(),
        "sh".into(),
        setting,
        wal_level,
        next_setting,
        timeout,
    ];
    let Running { container, port } =
        start(image(&tag()).with_entrypoint("sh").with_cmd(arguments));
    let mut printed = container
        .exec(ExecCommand::new(["cat", "/tmp/ca.pem"]))
        .unwrap();
    let ca = TempFile::new(&format!("ca-{port}.pem"), &printed.stdout_to_vec().unwrap());
    let dsn = format!(
        "postgres://postgres:{PASSWORD}@127.0.0.1:{port}/postgres?sslrootcert={}",
        ca.0.display()
    );
    PostgresContainer {
        container,
        dsn: Dsn::parse(&dsn).unwrap(),
        _ca: Some(ca),
    }
}

/// The postgres image at `tag`, ready once PostgreSQL takes connections.
pub(crate) fn image(tag: &str) -> GenericImage {
    GenericImage::new("postgres", tag)
        .with_exposed_port(5432.tcp())
        // The init server says it on stdout, the real one on stderr.
        .with_wait_for(WaitFor::message_on_stdout(READY))
        .with_wait_for(WaitFor::message_on_stderr(READY))
}

/// Loads January's taxi trips into the PostgreSQL in `container`:
/// `bench/load_taxi_trips.py` writes the SQL and `psql` in the container
/// runs it.
pub(crate) fn load_taxi_month(root: &Path, container: &Container<GenericImage>) {
    let mut loader = process::Command::new(root.join("bench/load_taxi_trips.py"))
        .arg("--data")
        .arg(root.join("bench/data"))
        .stdout(process::Stdio::piped())
        .spawn()
        .expect("bench/load_taxi_trips.py runs with uv");
    let psql = process::Command::new("docker")
        .args(["exec", "-i", container.id(), "psql", "-U", "postgres", "-q"])
        .stdin(loader.stdout.take().unwrap())
        .status()
        .unwrap();
    assert!(psql.success(), "psql: {psql}");
    let exit = loader.wait().unwrap();
    assert!(exit.success(), "bench/load_taxi_trips.py: {exit}");
}

/// A container PostgreSQL runs in, and its port on the host.
pub(super) struct Running {
    pub(super) container: Container<GenericImage>,
    pub(super) port: u16,
}

/// Runs `request` and waits until PostgreSQL takes connections.
pub(super) fn start(request: ContainerRequest<GenericImage>) -> Running {
    let container = request
        .with_env_var("POSTGRES_PASSWORD", PASSWORD)
        .with_env_var("POSTGRES_HOST_AUTH_METHOD", "scram-sha-256")
        .start()
        .expect("Docker runs postgres");
    // Docker may publish the port a moment after the container starts.
    for _ in 0..50 {
        if let Ok(port) = container.get_host_port_ipv4(5432) {
            return Running { container, port };
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("Docker publishes port 5432");
}

/// A plain SQL connection, plus the test's schema and publication, both
/// named `name`.
pub(super) struct Db {
    pub(super) connection: Connection,
    pub(super) name: String,
    pub(super) dsn: Dsn,
}

impl Db {
    /// Connects to `pg` and creates schema `name` holding `tables`.
    pub(super) fn new(pg: &PostgresContainer, name: &str, tables: &str) -> Db {
        let mut db = Db {
            connection: Connection::connect(&pg.dsn, ConnectionKind::Sql).unwrap(),
            name: name.to_owned(),
            dsn: pg.dsn.clone(),
        };
        db.run(&format!("CREATE SCHEMA $S; {tables}"));
        db
    }

    /// A second connection to the same schema.
    pub(super) fn another_connection(&self) -> Db {
        Db {
            connection: Connection::connect(&self.dsn, ConnectionKind::Sql).unwrap(),
            name: self.name.clone(),
            dsn: self.dsn.clone(),
        }
    }

    /// `text` with `$S` replaced by the test's schema.
    pub(super) fn sql(&self, text: &str) -> String {
        text.replace("$S", &self.name)
    }

    /// Runs `text` in the test's schema; panics naming the SQL on an error.
    pub(super) fn run(&mut self, text: &str) {
        let sql = self.sql(text);
        if let Err(error) = self.connection.execute(&sql) {
            panic!("{error}: {sql}");
        }
    }

    /// The rows `text` returns, each field as PostgreSQL prints it.
    pub(super) fn rows(&mut self, text: &str) -> Vec<Vec<Option<String>>> {
        let sql = self.sql(text);
        match self.connection.rows(&sql) {
            Ok(rows) => rows,
            Err(error) => panic!("{error}: {sql}"),
        }
    }

    /// Where PostgreSQL inserts WAL now.
    pub(super) fn insert_lsn(&mut self) -> Lsn {
        let rows = self.rows("SELECT pg_current_wal_insert_lsn()");
        parse_lsn(rows[0][0].as_deref().unwrap()).unwrap()
    }

    /// The spec `text` over the test's schema.
    pub(super) fn spec(&self, text: &str) -> Spec {
        straum_notation::parse(&self.sql(text)).unwrap()
    }

    /// A source that streams pgoutput from the test's publication.
    pub(super) fn pg(&self) -> Postgres {
        Postgres::new(
            self.dsn.clone(),
            &self.name,
            ROW_LIMIT,
            MAX_RETAINED_WAL_BYTES,
            Replication::Logical,
        )
    }

    /// A writer for `spec` over [`Db::pg`].
    pub(super) fn writer(&self, spec: &str) -> Writer<Postgres> {
        Writer::new(App::new(self.spec(spec), LIMITS), self.pg())
    }

    /// A writer that reads the physical WAL (ADR-0008).
    pub(super) fn physical_writer(&self, spec: &str) -> Writer<Postgres> {
        let source = Postgres::new(
            self.dsn.clone(),
            &self.name,
            ROW_LIMIT,
            MAX_RETAINED_WAL_BYTES,
            PHYSICAL,
        );
        Writer::new(App::new(self.spec(spec), LIMITS), source)
    }
}

/// `name` as an identifier.
pub(super) fn id(name: &str) -> Identifier {
    Identifier::new(name).unwrap()
}

/// An ask for `measures` of view `fact` by the keys `by`, over every
/// bucket.
pub(super) fn ask(fact: &str, measures: &[&str], by: &[&str]) -> Ask {
    Ask {
        view: id(fact),
        measures: measures.iter().copied().map(id).collect(),
        keys: by.iter().copied().map(ask_key).collect(),
        filter: Vec::new(),
        having: Vec::new(),
        from: 0,
        to: END,
    }
}

/// `ask` over the window from [`WINDOW_FROM`] to [`WINDOW_TO`].
pub(super) fn in_window(ask: Ask) -> Ask {
    Ask {
        from: WINDOW_FROM,
        to: WINDOW_TO,
        ..ask
    }
}

/// `ask` over the rows whose `key` is `value`.
pub(super) fn where_is(ask: Ask, key: &str, value: Literal) -> Ask {
    Ask {
        filter: vec![Condition {
            key: id(key),
            values: vec![value],
        }],
        ..ask
    }
}

/// `ask` kept by `conditions`.
pub(super) fn having(ask: Ask, conditions: Vec<MeasureCondition>) -> Ask {
    Ask {
        having: conditions,
        ..ask
    }
}

/// `measure comparison value`, as HAVING writes it.
pub(super) fn compared(measure: &str, comparison: Comparison, value: Literal) -> MeasureCondition {
    MeasureCondition {
        measure: id(measure),
        comparison,
        value,
    }
}

/// The key named `key`: `bucket` is the time key.
fn ask_key(key: &str) -> AskKey {
    match key {
        "bucket" => AskKey::Bucket,
        key => AskKey::Key(id(key)),
    }
}

/// A value as PostgreSQL prints it; `scale` for numeric measures.
pub(super) fn text(value: &Value, scale: u32) -> Option<String> {
    match *value {
        Value::Null => None,
        Value::Int(number) => Some(decimal(number, scale)),
        Value::Bool(flag) => Some(boolean(flag)),
        // As `round(avg(c), 6)`: half away from zero.
        Value::Avg { sum, count } => {
            let numerator = sum * 10_i128.pow(6 - scale) * 2;
            let denominator = i128::from(count) * 2;
            let half = numerator.signum() * i128::from(count);
            Some(decimal((numerator + half) / denominator, 6))
        }
    }
}

/// A key as PostgreSQL prints it; `scale` for numeric keys.
pub(super) fn datum(key: &Datum, scale: u32) -> Option<String> {
    match key {
        Datum::Null => None,
        Datum::Int(number) => Some(decimal(i128::from(*number), scale)),
        Datum::Text(text) => Some(text.to_string()),
        Datum::Bool(flag) => Some(boolean(*flag)),
    }
}

/// A `bool` as PostgreSQL prints it.
pub(super) fn boolean(flag: bool) -> String {
    if flag { "t" } else { "f" }.to_owned()
}

/// The `SELECT` of a view in `spec`.
pub(super) fn body(spec: &str, view: &str) -> String {
    let start = spec.find(&format!("CREATE VIEW {view} ")).unwrap();
    let rest = &spec[start..];
    let select = rest.find("SELECT").unwrap();
    let end = rest.find(';').unwrap();
    rest[select..end].to_owned()
}

/// Applies the next commit; panics on anything else.
pub(super) fn applied(writer: &mut Writer<Postgres>) {
    match writer.step() {
        Ok(Step::Applied(_)) => {}
        other => panic!("expected a commit, got {other:?}"),
    }
}

/// Applies the next commit, or starts over when it doesn't fit a table.
pub(super) fn applied_or_started(writer: &mut Writer<Postgres>) {
    match writer.step() {
        Ok(Step::Applied(_)) => {}
        Err(straum_app::Error::Unfit(_)) => {
            assert!(matches!(writer.step(), Ok(Step::Started(_))));
        }
        other => panic!("expected a commit, got {other:?}"),
    }
}

/// Seeded random numbers by splitmix64, and the hours from `START` that
/// trips fall in.
pub(super) struct Rng {
    numbers: SplitMix,
    hours: u64,
}

impl Rng {
    /// Numbers from `seed`, with times over three days.
    pub(super) fn new(seed: u64) -> Rng {
        Rng::within_hours(seed, 72)
    }

    /// Numbers from `seed`, with times within `hours` of `START`.
    pub(super) fn within_hours(seed: u64, hours: u64) -> Rng {
        Rng {
            numbers: SplitMix(seed),
            hours,
        }
    }

    /// A number from 0 up to, not including, `bound`.
    pub(super) fn below(&mut self, bound: u64) -> u64 {
        self.numbers.below(bound)
    }

    /// A second within `hours` of `START`.
    pub(super) fn time(&mut self) -> i64 {
        START + i64::try_from(self.below(self.hours * 3_600)).unwrap()
    }

    /// True `percent` times in a hundred.
    pub(super) fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    /// An index into something `length` long.
    pub(super) fn index(&mut self, length: usize) -> usize {
        self.numbers.below_usize(length)
    }

    /// One of `choices`.
    // `'a`: the choice is one of the caller's strings, not a copy.
    pub(super) fn pick<'a>(&mut self, choices: &[&'a str]) -> &'a str {
        choices[self.index(choices.len())]
    }

    /// A SQL literal, or `NULL` one time in ten.
    pub(super) fn or_null(&mut self, literal: String) -> String {
        if self.chance(10) {
            "NULL".into()
        } else {
            literal
        }
    }

    /// A `numeric(4, 1)` literal, or `NULL`.
    pub(super) fn toll(&mut self) -> String {
        let toll = format!("{}.{}", self.below(20), self.below(10));
        self.or_null(toll)
    }

    /// A task row over a `timestamp` column: the literal is the UTC wall
    /// clock of `at`, whatever zone the session has. Task groups 3 and 4
    /// have no row, so their category is null.
    pub(super) fn task(&mut self, id: u64) -> String {
        let at = self.time();
        let organization = 1 + self.below(3);
        let taskgroup = (1 + self.below(4)).to_string();
        let completed = if self.chance(50) {
            format!("to_timestamp({}) AT TIME ZONE 'UTC'", at + 600)
        } else {
            "NULL".into()
        };
        let picks = self.below(50).to_string();
        format!(
            "INSERT INTO $S.tasks VALUES ({id}, {organization}, {}, \
             to_timestamp({at}) AT TIME ZONE 'UTC', {completed}, {});",
            self.or_null(taskgroup),
            self.or_null(picks)
        )
    }

    /// An order of the shop: customer 8 and sku 5 have no row.
    pub(super) fn order(&mut self, id: u64) -> String {
        let at = self.time();
        let customer = (1 + self.below(8)).to_string();
        let sku = (1 + self.below(5)).to_string();
        let qty = self.below(10).to_string();
        let status = format!("'{}'", self.pick(&["paid", "shipped", "new", "cancelled"]));
        let gift = self.pick(&["true", "false"]).to_owned();
        format!(
            "INSERT INTO $S.orders VALUES ({id}, to_timestamp({at}), {}, {}, {}, {}, {})",
            self.or_null(customer),
            self.or_null(sku),
            self.or_null(qty),
            self.or_null(status),
            self.or_null(gift)
        )
    }

    /// An insert of trip `id`, each column past the time maybe null.
    pub(super) fn trip(&mut self, id: u64) -> String {
        let at = self.time();
        let kind = format!("'{}'", self.pick(&KINDS));
        let zone = (1 + self.below(6)).to_string();
        let fare = format!("{}.{:02}", self.below(80), self.below(100));
        let rider = format!("'{}'", self.pick(&RIDERS));
        let tip = self.below(20).to_string();
        format!(
            "INSERT INTO $S.trips VALUES ({id}, to_timestamp({at}), {}, {}, {}, {}, {});",
            self.or_null(kind),
            self.or_null(zone),
            self.or_null(fare),
            self.or_null(rider),
            self.or_null(tip)
        )
    }
}

/// A message as PostgreSQL frames it: tag, length counting itself, body.
pub(super) fn message(tag: u8, body: &[u8]) -> Vec<u8> {
    let length = i32::try_from(body.len() + 4).unwrap();
    let mut framed = vec![tag];
    framed.extend(length.to_be_bytes());
    framed.extend(body);
    framed
}

/// `AuthenticationOk`, then `ReadyForQuery`.
pub(super) fn signed_in() -> Vec<u8> {
    let mut messages = message(b'R', &0_i32.to_be_bytes());
    messages.extend(message(b'Z', b"I"));
    messages
}

/// `CopyData` holding `body`.
pub(super) fn copy_data(body: &[u8]) -> Vec<u8> {
    message(b'd', body)
}

/// A primary keepalive at WAL position `end`, asking for a reply or not.
pub(super) fn keepalive(end: u64, reply: Reply) -> Vec<u8> {
    let mut body = vec![b'k'];
    body.extend(end.to_be_bytes());
    body.extend(0_i64.to_be_bytes());
    body.push(u8::from(reply == Reply::Asked));
    copy_data(&body)
}

/// Whether a keepalive asks the client to answer at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Reply {
    Asked,
    NotAsked,
}

/// `ErrorResponse` with SQLSTATE `code` and message `text`.
pub(super) fn error_response(code: &str, text: &str) -> Vec<u8> {
    message(b'E', format!("SERROR\0C{code}\0M{text}\0\0").as_bytes())
}

/// Bytes a fake server writes once `pause` has passed.
pub(super) struct Part {
    pause: Duration,
    bytes: Vec<u8>,
}

/// `bytes`, written at once.
pub(super) fn now(bytes: Vec<u8>) -> Part {
    later(Duration::ZERO, bytes)
}

/// `bytes`, written after `pause`.
pub(super) fn later(pause: Duration, bytes: Vec<u8>) -> Part {
    Part { pause, bytes }
}

/// A fake server's address, and the thread that returns what the client
/// sent it.
pub(super) struct FakeServer {
    pub(super) dsn: Dsn,
    pub(super) received: thread::JoinHandle<Vec<u8>>,
}

/// A fake for PostgreSQL: writes each part after its pause, then stops
/// writing, reads until the client hangs up and returns what it sent.
pub(super) fn scripted(parts: Vec<Part>) -> FakeServer {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let received = thread::spawn(move || serve(&listener, parts));
    let dsn = format!("postgres://straum@127.0.0.1:{port}/straum?sslmode=disable");
    FakeServer {
        dsn: Dsn::parse(&dsn).unwrap(),
        received,
    }
}

/// Plays `parts` to the first client of `listener` and returns what it
/// sent.
fn serve(listener: &TcpListener, parts: Vec<Part>) -> Vec<u8> {
    let (mut socket, _) = listener.accept().unwrap();
    for Part { pause, bytes } in parts {
        thread::sleep(pause);
        // The client may hang up first.
        if socket.write_all(&bytes).is_err() {
            break;
        }
    }
    // The client sees the end of the stream once it has read the parts.
    let _ = socket.shutdown(Shutdown::Write);
    let mut received = Vec::new();
    let _ = socket.read_to_end(&mut received);
    received
}

/// A connection of `kind` to a fake server that plays `parts`.
pub(super) fn connect(parts: Vec<Part>, kind: ConnectionKind) -> Result<Connection, Error> {
    let server = scripted(parts);
    Connection::connect(&server.dsn, kind)
}

/// `AuthenticationSASL` offering `mechanisms`.
pub(super) fn sasl(mechanisms: &str) -> Vec<u8> {
    let mut body = 10_i32.to_be_bytes().to_vec();
    body.extend(mechanisms.as_bytes());
    message(b'R', &body)
}

/// What comes after `key` in `line`.
// `'a`: the text returned lies inside `line`, not a copy.
pub(super) fn after<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let (_, rest) = line.split_once(key)?;
    Some(rest.trim_start())
}

/// A table's file in the WAL's terms, and its length in pages.
pub(super) struct TableFile {
    pub(super) relation: RelationFile,
    pub(super) pages: u32,
}

/// The file of `$S.table` and its length in pages.
pub(super) fn relation_file(db: &mut Db, table: &str) -> TableFile {
    let rows = db.rows(&format!(
        "SELECT CASE reltablespace WHEN 0 THEN 1663 ELSE reltablespace END,
                (SELECT oid FROM pg_database WHERE datname = current_database()),
                pg_relation_filenode(oid), pg_relation_size(oid) / 8192
         FROM pg_class WHERE oid = '$S.{table}'::regclass"
    ));
    let mut numbers: Vec<u32> = Vec::new();
    for field in &rows[0] {
        numbers.push(field.as_deref().unwrap().parse().unwrap());
    }
    let relation = RelationFile {
        tablespace: numbers[0],
        database: numbers[1],
        relation: numbers[2],
    };
    TableFile {
        relation,
        pages: numbers[3],
    }
}
