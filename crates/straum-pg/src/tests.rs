//! straum-pg against PostgreSQL 18 in Docker, one container per test. Every
//! answer is checked against the view body of the spec, run in PostgreSQL.

use std::fs;
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::path::{Path, PathBuf};
use std::process;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use straum_app::{AnswerError, App, Limits, Lsn, Source, Step, Writer};
use straum_domain::{Answer, Ask, AskKey, Datum, Ident, KeyPath, Spec, Store, TimeKey, Value};
use testcontainers::core::{AccessMode, ExecCommand, IntoContainerPort, Mount, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};

use crate::catalog;
use crate::dsn::Dsn;
use crate::error::Error;
use crate::source::{Pg, read_wal};
use crate::stream::{Decoder, Stream};
use crate::wire::Conn;

const PASSWORD: &str = "straum";
/// Long enough for anything a test waits on to come.
const LONG: Duration = Duration::from_secs(10);
const LIMITS: Limits = Limits {
    max_lag: Duration::from_secs(30),
    max_retained_wal: 4 << 30,
};
const READY: &str = "database system is ready to accept connections";
/// 2100-01-01, on every bucket edge.
const END: i64 = 4_102_444_800;
/// 2024-01-15, where test rows start.
const START: i64 = 1_705_276_800;

/// The spec the taxi-shaped tests follow. `$S` is the test's schema.
const SPEC: &str = "
CREATE VIEW trips WITH (key = id) AS
SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket,
       f.kind,
       f.zone_id,
       pickup.borough,
       count(*) AS trips,
       sum(f.fare) AS fare,
       max(f.fare) AS fare_max,
       avg(f.fare) AS fare_avg,
       count(DISTINCT f.rider) AS riders,
       sum(f.tip) AS tips,
       count(*) FILTER (WHERE pickup.borough = 'Manhattan') AS manhattan,
       count(*) FILTER (WHERE f.zone_id IN (1, 2)) AS near
FROM $S.trips f
LEFT JOIN $S.zones pickup ON pickup.id = f.zone_id
GROUP BY 1, 2, 3, 4;

CREATE VIEW logins WITH (key = id) AS
SELECT date_bin('1 day', l.at, timestamptz 'epoch') AS bucket,
       l.zone_id,
       pickup.toll,
       count(*) AS logins
FROM $S.logins l
LEFT JOIN $S.zones pickup ON pickup.id = l.zone_id
GROUP BY 1, 2, 3;
";

const TABLES: &str = "
CREATE TABLE $S.zones (id int PRIMARY KEY, borough text, toll numeric(4, 1));
CREATE TABLE $S.trips (
    id bigint PRIMARY KEY,
    at timestamptz,
    kind text,
    zone_id int,
    fare numeric(10, 2),
    rider text,
    tip int
);
ALTER TABLE $S.trips REPLICA IDENTITY FULL;
CREATE TABLE $S.logins (id bigint PRIMARY KEY, at timestamptz, zone_id int);
ALTER TABLE $S.logins REPLICA IDENTITY FULL;
CREATE PUBLICATION $S FOR TABLE $S.zones, $S.trips, $S.logins;
";

const BOROUGHS: [&str; 3] = ["Manhattan", "Queens", "Bronx"];
const KINDS: [&str; 3] = ["cab", "pool", "van"];
const RIDERS: [&str; 4] = ["ann", "bo", "cy", "di"];

/// Logical decoding and a short `wal_sender_timeout`.
const FLAGS: [&str; 4] = ["-c", "wal_level=logical", "-c", "wal_sender_timeout=4s"];

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

/// PostgreSQL 18 with [`FLAGS`], removed when dropped, with its CA file if
/// it has TLS.
struct Postgres {
    _container: Container<GenericImage>,
    dsn: Dsn,
    _ca: Option<TempFile>,
}

/// A file in the temp directory, removed when dropped.
struct TempFile(PathBuf);

impl TempFile {
    fn new(name: &str, contents: &[u8]) -> TempFile {
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

fn postgres() -> Postgres {
    let (container, port) = start(
        GenericImage::new("postgres", "18"),
        FLAGS.map(str::to_owned),
    );
    let dsn = format!("postgres://postgres:{PASSWORD}@127.0.0.1:{port}/postgres?sslmode=disable");
    Postgres {
        _container: container,
        dsn: Dsn::parse(&dsn).unwrap(),
        _ca: None,
    }
}

/// As [`postgres`], with ssl on. The DSN trusts the test's CA alone.
fn postgres_tls() -> Postgres {
    let [c, wal_level, c2, timeout] = FLAGS.map(str::to_owned);
    let cmd = [
        "-c".into(),
        WITH_TLS.into(),
        "sh".into(),
        c,
        wal_level,
        c2,
        timeout,
    ];
    let image = GenericImage::new("postgres", "18").with_entrypoint("sh");
    let (container, port) = start(image, cmd);
    let mut cat = container
        .exec(ExecCommand::new(["cat", "/tmp/ca.pem"]))
        .unwrap();
    let ca = TempFile::new(&format!("ca-{port}.pem"), &cat.stdout_to_vec().unwrap());
    let dsn = format!(
        "postgres://postgres:{PASSWORD}@127.0.0.1:{port}/postgres?sslrootcert={}",
        ca.0.display()
    );
    Postgres {
        _container: container,
        dsn: Dsn::parse(&dsn).unwrap(),
        _ca: Some(ca),
    }
}

/// Runs `image` with `cmd` and waits until PostgreSQL takes connections;
/// returns the container and its port on the host.
fn start<const N: usize>(image: GenericImage, cmd: [String; N]) -> (Container<GenericImage>, u16) {
    let container = image
        .with_exposed_port(5432.tcp())
        // The init server says it on stdout, the real one on stderr.
        .with_wait_for(WaitFor::message_on_stdout(READY))
        .with_wait_for(WaitFor::message_on_stderr(READY))
        .with_env_var("POSTGRES_PASSWORD", PASSWORD)
        .with_env_var("POSTGRES_HOST_AUTH_METHOD", "scram-sha-256")
        .with_cmd(cmd)
        .start()
        .expect("Docker runs postgres:18");
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
    (container, port)
}

/// A plain SQL connection, plus the test's schema and publication, both
/// named `name`.
struct Db {
    conn: Conn,
    name: String,
    dsn: Dsn,
}

impl Db {
    fn new(pg: &Postgres, name: &str, tables: &str) -> Db {
        let mut db = Db {
            conn: Conn::connect(&pg.dsn, false).unwrap(),
            name: name.to_owned(),
            dsn: pg.dsn.clone(),
        };
        db.run(&format!("CREATE SCHEMA $S; {tables}"));
        db
    }

    fn sql(&self, text: &str) -> String {
        text.replace("$S", &self.name)
    }

    fn run(&mut self, text: &str) {
        let sql = self.sql(text);
        self.conn
            .execute(&sql)
            .unwrap_or_else(|e| panic!("{e}: {sql}"));
    }

    fn rows(&mut self, text: &str) -> Vec<Vec<Option<String>>> {
        let sql = self.sql(text);
        self.conn
            .rows(&sql)
            .unwrap_or_else(|e| panic!("{e}: {sql}"))
    }

    fn spec(&self, text: &str) -> Spec {
        straum_notation::parse(&self.sql(text)).unwrap()
    }

    fn pg(&self) -> Pg {
        Pg::new(self.dsn.clone(), &self.name)
    }

    fn writer(&self) -> Writer<Pg> {
        Writer::new(App::new(self.spec(SPEC), LIMITS), self.pg())
    }
}

fn id(s: &str) -> Ident {
    Ident::new(s).unwrap()
}

fn ask(fact: &str, measures: &[&str], by: &[&str]) -> Ask {
    Ask {
        fact: id(fact),
        measures: measures.iter().map(|m| id(m)).collect(),
        by: by
            .iter()
            .map(|k| match *k {
                "bucket" => AskKey::Time(TimeKey::Bucket),
                k => AskKey::Key(match k.split_once('.') {
                    Some((head, attr)) => KeyPath {
                        head: id(head),
                        attr: Some(id(attr)),
                    },
                    None => KeyPath {
                        head: id(k),
                        attr: None,
                    },
                }),
            })
            .collect(),
        filter: Vec::new(),
        from: 0,
        to: END,
    }
}

/// A value as PostgreSQL prints it; `scale` for numeric measures.
fn text(value: &Value, scale: u32) -> Option<String> {
    match *value {
        Value::Null => None,
        Value::Int(n) => Some(decimal(n, scale)),
        // As `round(avg(c), 6)`: half away from zero.
        Value::Avg { sum, count } => {
            let numerator = sum * 10_i128.pow(6 - scale) * 2;
            let denominator = i128::from(count) * 2;
            let half = numerator.signum() * i128::from(count);
            Some(decimal((numerator + half) / denominator, 6))
        }
    }
}

fn decimal(n: i128, scale: u32) -> String {
    if scale == 0 {
        return n.to_string();
    }
    let unit = 10_i128.pow(scale);
    let sign = if n < 0 { "-" } else { "" };
    let (whole, fraction) = (n.abs() / unit, n.abs() % unit);
    format!("{sign}{whole}.{fraction:0width$}", width = scale as usize)
}

/// A key as PostgreSQL prints it; `scale` for numeric keys.
fn datum(d: &Datum, scale: u32) -> Option<String> {
    match d {
        Datum::Null => None,
        Datum::Int(n) => Some(decimal(i128::from(*n), scale)),
        Datum::Text(s) => Some(s.to_string()),
        Datum::Bool(b) => Some(if *b { "t" } else { "f" }.to_owned()),
    }
}

/// The `SELECT` of a view in the spec.
fn body(view: &str) -> String {
    let start = SPEC.find(&format!("CREATE VIEW {view} ")).unwrap();
    let rest = &SPEC[start..];
    let select = rest.find("SELECT").unwrap();
    let end = rest.find(';').unwrap();
    rest[select..end].to_owned()
}

/// Asks straum what each view body answers in PostgreSQL: trips by bucket,
/// kind, zone and borough; logins by day, zone and toll. `fare_scale`
/// follows `fare`'s type. `None` when straum isn't ready.
fn compare(db: &mut Db, app: &App, fare_scale: u32) -> Option<()> {
    let trips = (
        format!(
            "SELECT extract(epoch FROM bucket)::int8, kind, zone_id, borough, trips, fare,
                    fare_max, round(fare_avg, 6), riders, tips, manhattan, near
               FROM ({}) v",
            body("trips")
        ),
        ask(
            "trips",
            &[
                "trips",
                "fare",
                "fare_max",
                "fare_avg",
                "riders",
                "tips",
                "manhattan",
                "near",
            ],
            &["bucket", "kind", "pickup", "pickup.borough"],
        ),
        vec![
            0, 0, 0, 0, 0, fare_scale, fare_scale, fare_scale, 0, 0, 0, 0,
        ],
    );
    let logins = (
        format!(
            "SELECT extract(epoch FROM bucket)::int8, zone_id, toll, logins FROM ({}) v",
            body("logins")
        ),
        ask("logins", &["logins"], &["bucket", "pickup", "pickup.toll"]),
        vec![0, 0, 1, 0],
    );
    for (sql, ask, scales) in [trips, logins] {
        let answered = match app.ask(&ask) {
            Ok(answered) => answered,
            Err(AnswerError::NotReady) => return None,
            Err(e) => panic!("{e}"),
        };
        assert_eq!(answered.answer.scales, scales, "{}", ask.fact);
        let mut got = printed(&answered.answer, &scales);
        let mut expected = db.rows(&sql);
        got.sort();
        expected.sort();
        assert_eq!(got, expected, "{}", ask.fact);
    }
    Some(())
}

/// An answer's rows as PostgreSQL prints them, at `scales`.
fn printed(answer: &Answer, scales: &[u32]) -> Vec<Vec<Option<String>>> {
    (answer.rows.iter())
        .map(|(key, values)| {
            let (key_scales, value_scales) = scales.split_at(key.len());
            (key.iter().zip(key_scales).map(|(k, s)| datum(k, *s)))
                .chain(values.iter().zip(value_scales).map(|(v, s)| text(v, *s)))
                .collect()
        })
        .collect()
}

fn applied(writer: &mut Writer<Pg>) {
    match writer.step() {
        Ok(Step::Applied(_)) => {}
        other => panic!("expected a commit, got {other:?}"),
    }
}

/// Applies the next commit, or starts over when it doesn't fit a table.
fn applied_or_started(writer: &mut Writer<Pg>) {
    match writer.step() {
        Ok(Step::Applied(_)) => {}
        Err(straum_app::Error::Unfit(_)) => {
            assert!(matches!(writer.step(), Ok(Step::Started(_))));
        }
        other => panic!("expected a commit, got {other:?}"),
    }
}

/// splitmix64, and the hours from `START` that trips fall in.
struct Rng {
    state: u64,
    hours: u64,
}

impl Rng {
    fn new(seed: u64) -> Rng {
        Rng {
            state: seed,
            hours: 72,
        }
    }

    fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len() as u64) as usize]
    }

    /// A SQL literal, or `NULL` one time in ten.
    fn or_null(&mut self, literal: String) -> String {
        if self.chance(10) {
            "NULL".into()
        } else {
            literal
        }
    }

    /// A `numeric(4, 1)` literal, or `NULL`.
    fn toll(&mut self) -> String {
        let toll = format!("{}.{}", self.below(20), self.below(10));
        self.or_null(toll)
    }

    fn trip(&mut self, id: u64) -> String {
        let at = START + self.below(self.hours * 3_600) as i64;
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

#[test]
fn follows_the_database() {
    let pg = postgres();
    let mut db = Db::new(&pg, "follows", TABLES);
    // Six hours of trips, so a cell holds several load groups.
    let mut rng = Rng {
        hours: 6,
        ..Rng::new(7)
    };
    let mut trips: Vec<u64> = Vec::new();
    let mut next = 1;
    for zone in 1..=4 {
        db.run(&format!(
            "INSERT INTO $S.zones VALUES ({zone}, '{}', {})",
            rng.pick(&BOROUGHS),
            rng.toll()
        ));
    }
    for _ in 0..400 {
        db.run(&rng.trip(next));
        trips.push(next);
        next += 1;
    }

    let mut writer = db.writer();
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    compare(&mut db, writer.app(), 2).expect("ready after the load");

    let mut compared = 0;
    for round in 0..120 {
        // Every transaction inserts a trip, so each is one commit.
        let mut sql = vec!["BEGIN;".to_owned(), rng.trip(next)];
        trips.push(next);
        next += 1;
        for _ in 0..rng.below(4) {
            let pick = trips[rng.below(trips.len() as u64) as usize];
            let statement = match rng.below(10) {
                0 | 1 => format!(
                    "UPDATE $S.trips SET fare = fare + 1, kind = '{}' WHERE id = {pick};",
                    rng.pick(&KINDS)
                ),
                2 => format!("UPDATE $S.trips SET at = at + interval '5 hours' WHERE id = {pick};"),
                3 => {
                    trips.retain(|t| *t != pick);
                    format!("DELETE FROM $S.trips WHERE id = {pick};")
                }
                4 => format!(
                    "INSERT INTO $S.zones VALUES ({}, '{}', {}) ON CONFLICT (id)
                     DO UPDATE SET borough = excluded.borough, toll = excluded.toll;",
                    1 + rng.below(6),
                    rng.pick(&BOROUGHS),
                    rng.toll()
                ),
                5 => format!("DELETE FROM $S.zones WHERE id = {};", 1 + rng.below(6)),
                6 => format!(
                    "UPDATE $S.zones SET id = {} WHERE id = {}
                       AND NOT EXISTS (SELECT FROM $S.zones WHERE id = {0});",
                    1 + rng.below(6),
                    1 + rng.below(6)
                ),
                7 => format!(
                    "INSERT INTO $S.logins VALUES ({next}, to_timestamp({}), {});",
                    START + rng.below(3 * 86_400) as i64,
                    1 + rng.below(6)
                ),
                _ => continue,
            };
            next += 1;
            sql.push(statement);
        }
        // Every login moves zone at once; truncates empty a table in place.
        match round {
            30 | 70 => sql.push("UPDATE $S.logins SET zone_id = zone_id + 1;".into()),
            50 => sql.push("TRUNCATE $S.logins;".into()),
            90 => {
                sql.push("TRUNCATE $S.trips;".into());
                trips.clear();
                sql.push(rng.trip(next));
                trips.push(next);
                next += 1;
            }
            _ => {}
        }
        sql.push("COMMIT;".into());
        db.run(&sql.join("\n"));
        applied_or_started(&mut writer);
        if compare(&mut db, writer.app(), 2).is_some() {
            compared += 1;
        }
    }
    assert!(compared > 100, "compared only {compared} times");
}

#[test]
fn the_snapshot_and_the_stream_split_exactly() {
    let pg = postgres();
    let mut db = Db::new(&pg, "split", TABLES);
    for round in 0..3 {
        db.run("TRUNCATE $S.trips");
        let written = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let inserter = {
            let (dsn, name) = (db.dsn.clone(), db.name.clone());
            let (written, done) = (Arc::clone(&written), Arc::clone(&done));
            thread::spawn(move || {
                let mut conn = Conn::connect(&dsn, false).unwrap();
                for i in 0..2_000 {
                    let sql = format!(
                        "INSERT INTO {name}.trips VALUES ({i}, to_timestamp({}), 'cab', 1, 1.00, 'ann', 2)",
                        START + i
                    );
                    conn.execute(&sql).unwrap();
                    written.fetch_add(1, Ordering::Release);
                }
                done.store(true, Ordering::Release);
            })
        };
        while written.load(Ordering::Acquire) < 50 {
            thread::sleep(Duration::from_millis(1));
        }
        let mut writer = db.writer();
        assert!(matches!(writer.step(), Ok(Step::Started(_))));
        assert!(
            !done.load(Ordering::Acquire),
            "round {round}: the writer finished before the load"
        );
        let loaded = writer.app().ask(&ask("trips", &["trips"], &[])).unwrap();
        assert_ne!(loaded.answer.rows[0].1[0], Value::Int(0), "round {round}");
        inserter.join().unwrap();
        db.run("INSERT INTO $S.trips VALUES (-1, now(), 'marker', 1, 1.00, 'ann', 2)");
        let marker = ask("trips", &["trips"], &["kind"]);
        let mut streamed = 0;
        loop {
            applied(&mut writer);
            streamed += 1;
            let answered = writer.app().ask(&marker).unwrap();
            if answered
                .answer
                .rows
                .iter()
                .any(|(k, _)| k[0] == Datum::Text("marker".into()))
            {
                break;
            }
        }
        assert!(streamed > 1, "round {round}: nothing was streamed");
        // The marker's commit time, by the server's clock, is now.
        let time = writer.app().ask(&marker).unwrap().time.unwrap();
        let apart = match SystemTime::now().duration_since(time) {
            Ok(behind) => behind,
            Err(ahead) => ahead.duration(),
        };
        assert!(apart < Duration::from_secs(60), "round {round}: {apart:?}");
        compare(&mut db, writer.app(), 2).expect("ready");
    }
}

#[test]
fn an_unfit_change_starts_over() {
    let pg = postgres();
    let mut db = Db::new(&pg, "unfit", TABLES);
    db.run("INSERT INTO $S.logins VALUES (1, to_timestamp(1705276800), 1)");
    let mut writer = db.writer();
    writer.step().unwrap();
    compare(&mut db, writer.app(), 2).expect("ready");

    // A fact that stops sending old rows starts over, and the setup then
    // says what to fix.
    db.run("ALTER TABLE $S.logins REPLICA IDENTITY DEFAULT");
    db.run("UPDATE $S.logins SET zone_id = 2");
    let err = writer.step().unwrap_err().to_string();
    assert_eq!(err, "a change no longer fits logins");
    assert_eq!(
        writer.app().ask(&ask("trips", &["trips"], &[])),
        Err(AnswerError::NotReady)
    );
    let err = writer.step().unwrap_err().to_string();
    assert!(
        err.contains("fact logins: straum needs the whole old row of every update and delete"),
        "{err}"
    );
    db.run("ALTER TABLE $S.logins REPLICA IDENTITY FULL");
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    compare(&mut db, writer.app(), 2).expect("ready after the new load");
    db.run("INSERT INTO $S.logins VALUES (2, to_timestamp(1705276800), 3)");
    applied(&mut writer);
    compare(&mut db, writer.app(), 2).expect("ready");
}

#[test]
fn follows_the_database_over_tls() {
    let pg = postgres_tls();
    let mut db = Db::new(&pg, "tls", TABLES);
    let mut rng = Rng::new(11);
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5), (2, 'Bronx', NULL)");
    for id in 0..50 {
        db.run(&rng.trip(id));
    }
    db.run("INSERT INTO $S.logins VALUES (1, to_timestamp(1705276800), 1)");
    let mut writer = db.writer();
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    compare(&mut db, writer.app(), 2).expect("ready after the load");
    for id in 50..60 {
        db.run(&rng.trip(id));
        applied(&mut writer);
    }
    // Past wal_sender_timeout (4 s): reads time out and keepalives are
    // answered, over TLS.
    thread::sleep(Duration::from_secs(6));
    // An altered table starts over, on a new TLS connection. The new
    // Relation comes with the next change to the table.
    db.run("ALTER TABLE $S.logins ADD COLUMN note text");
    db.run("INSERT INTO $S.logins VALUES (2, to_timestamp(1705276800), 1)");
    applied_or_started(&mut writer);
    for id in 60..70 {
        db.run(&rng.trip(id));
        applied(&mut writer);
    }
    compare(&mut db, writer.app(), 2).expect("ready");
    let tls = db.rows(
        "SELECT DISTINCT a.backend_type, s.ssl FROM pg_stat_activity a
         JOIN pg_stat_ssl s USING (pid) WHERE a.application_name = 'straum' ORDER BY 1",
    );
    let row = |kind: &str| vec![Some(kind.to_owned()), Some("t".to_owned())];
    assert_eq!(tls, [row("client backend"), row("walsender")]);
}

#[test]
fn tls_trusts_only_its_roots() {
    let pg = postgres_tls();
    // Display leaves the password out.
    let dsn = (pg.dsn.to_string()).replace("postgres@", &format!("postgres:{PASSWORD}@"));
    let ssl = |dsn: &str| {
        let mut conn = Conn::connect(&Dsn::parse(dsn).unwrap(), false)?;
        let rows = conn.rows("SELECT ssl FROM pg_stat_ssl WHERE pid = pg_backend_pid()")?;
        Ok::<_, Error>(rows[0][0].clone().unwrap())
    };
    assert_eq!(ssl(&dsn).unwrap(), "t");
    let (plain, _) = dsn.split_once('?').unwrap();
    assert_eq!(ssl(&format!("{plain}?sslmode=disable")).unwrap(), "f");

    // The roots built in don't know the test's CA.
    let err = ssl(&format!("{plain}?sslmode=require")).unwrap_err();
    assert_eq!(
        err.to_string(),
        "TLS to PostgreSQL: invalid peer certificate: UnknownIssuer"
    );
    // The certificate names 127.0.0.1, not localhost; verify-ca checks the
    // name too.
    let localhost = dsn.replace("127.0.0.1", "localhost") + "&sslmode=verify-ca";
    let err = ssl(&localhost).unwrap_err();
    assert_eq!(
        err.to_string(),
        "TLS to PostgreSQL: invalid peer certificate: certificate not valid for name \"localhost\"; \
         certificate is only valid for IpAddress(127.0.0.1)"
    );
}

#[test]
fn tls_start_errors() {
    let missing = std::env::temp_dir().join("straum-no-such-ca.pem");
    let empty = TempFile::new("empty.pem", b"");
    let junk = TempFile::new(
        "junk.pem",
        b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
    );
    let open = TempFile::new("open.pem", b"-----BEGIN CERTIFICATE-----\nAAAA\n");
    let path = |file: &Path| file.display().to_string();
    let cases: Vec<(Vec<u8>, Option<String>, String)> = vec![
        (
            b"N".to_vec(),
            None,
            "TLS to PostgreSQL: PostgreSQL has ssl off; turn it on, \
             or use sslmode=disable on a trusted network"
                .into(),
        ),
        (
            b"E".to_vec(),
            None,
            "PostgreSQL sent 0x45 as its answer to SSLRequest".into(),
        ),
        (
            Vec::new(),
            None,
            "connection to PostgreSQL: failed to fill whole buffer".into(),
        ),
        (
            b"Snot a TLS record".to_vec(),
            None,
            "TLS to PostgreSQL: received corrupt message of type InvalidContentType".into(),
        ),
        (
            b"S".to_vec(),
            Some(path(&missing)),
            format!(
                "TLS to PostgreSQL: reading {}: I/O error: No such file or directory (os error 2)",
                path(&missing)
            ),
        ),
        (
            b"S".to_vec(),
            Some(path(&empty.0)),
            format!(
                "TLS to PostgreSQL: {} holds no CA certificate",
                path(&empty.0)
            ),
        ),
        (
            b"S".to_vec(),
            Some(path(&junk.0)),
            format!(
                "TLS to PostgreSQL: {} holds no CA certificate",
                path(&junk.0)
            ),
        ),
        (
            b"S".to_vec(),
            Some(path(&open.0)),
            format!(
                "TLS to PostgreSQL: reading {}: missing section end marker",
                path(&open.0)
            ),
        ),
    ];
    for (answer, ca, expected) in cases {
        let (dsn, _server) = scripted(vec![now(answer)]);
        let tls = ca.map_or("sslmode=require".to_owned(), |ca| {
            format!("sslrootcert={ca}")
        });
        let dsn = Dsn::parse(&dsn.to_string().replace("sslmode=disable", &tls)).unwrap();
        let err = Conn::connect(&dsn, false).err().unwrap().to_string();
        assert!(err.starts_with(&expected), "{err:?} is not {expected:?}");
    }
}

#[test]
fn a_changed_column_type_starts_over() {
    let pg = postgres();
    let mut db = Db::new(&pg, "altered", TABLES);
    db.run(&Rng::new(1).trip(1));
    let mut writer = db.writer();
    writer.step().unwrap();
    compare(&mut db, writer.app(), 2).expect("ready");

    db.run("ALTER TABLE $S.trips ALTER COLUMN fare TYPE numeric(12, 3)");
    db.run("INSERT INTO $S.trips VALUES (2, to_timestamp(1705276800), 'cab', 1, 2.125, 'bo', 3)");
    let err = writer.step().unwrap_err().to_string();
    assert_eq!(err, "a change no longer fits trips");
    writer.step().unwrap();
    db.run(&Rng::new(2).trip(3));
    applied(&mut writer);
    compare(&mut db, writer.app(), 3).expect("ready");

    // A column straum reads is gone: straum stops and the setup check says why.
    db.run("ALTER TABLE $S.trips DROP COLUMN rider");
    db.run(
        "INSERT INTO $S.trips (id, at, kind, zone_id, fare, tip)
         VALUES (4, to_timestamp(1705276800), 'cab', 1, 1.000, 1)",
    );
    let err = writer.step().unwrap_err().to_string();
    assert!(err.contains("column rider of"), "{err}");
    let err = writer.step().unwrap_err().to_string();
    assert!(err.contains("has no column rider"), "{err}");
}

#[test]
fn a_value_left_out_starts_over() {
    let pg = postgres();
    let mut db = Db::new(&pg, "toast", TABLES);
    // Too big and too random to stay inline: PostgreSQL TOASTs it, and an
    // update that leaves it alone doesn't send it.
    db.run(
        "INSERT INTO $S.zones
         SELECT 1, string_agg(md5(random()::text), '') FROM generate_series(1, 2000)",
    );
    db.run("ALTER TABLE $S.zones ADD COLUMN note text");
    db.run(&Rng::new(1).trip(1));
    let mut writer = db.writer();
    writer.step().unwrap();
    compare(&mut db, writer.app(), 2).expect("ready");

    db.run("UPDATE $S.zones SET note = 'x' WHERE id = 1");
    let err = writer.step().unwrap_err().to_string();
    assert_eq!(err, "a change no longer fits zones");
    writer.step().unwrap();
    db.run(&Rng::new(2).trip(2));
    applied(&mut writer);
    compare(&mut db, writer.app(), 2).expect("ready");
}

#[test]
fn the_stream_outlasts_a_busy_writer() {
    let pg = postgres();
    let mut db = Db::new(&pg, "busy", TABLES);
    let mut writer = db.writer();
    writer.step().unwrap();
    // The stream starts with the first commit asked for.
    let mut rng = Rng::new(5);
    db.run(&rng.trip(0));
    applied(&mut writer);
    // More commits than the queue holds, then a pause past
    // wal_sender_timeout (4 s): the thread keeps answering.
    for i in 1..101 {
        db.run(&rng.trip(i));
    }
    thread::sleep(Duration::from_secs(9));
    for _ in 0..100 {
        applied(&mut writer);
    }
    // Commits made after the pause still arrive: the stream survived.
    for i in 101..111 {
        db.run(&rng.trip(i));
        applied(&mut writer);
    }
    compare(&mut db, writer.app(), 2).expect("ready");
}

#[test]
fn plan_sets_up_what_straum_needs() {
    let pg = postgres();
    let mut db = Db::new(&pg, "plan", &TABLES.replace("CREATE PUBLICATION", "-- "));
    db.run("ALTER TABLE $S.trips REPLICA IDENTITY DEFAULT");
    let spec = db.spec(SPEC);
    let sql = crate::plan(&spec, "plan", "straum_plan");
    assert_eq!(
        sql,
        "-- straum setup: run once, as an owner of the tables.
-- wal_level takes a restart. On Azure, set the server parameter instead.
ALTER SYSTEM SET wal_level = logical;
CREATE ROLE \"straum_plan\" LOGIN REPLICATION;
-- Then give \"straum_plan\" a password: \\password \"straum_plan\"
GRANT USAGE ON SCHEMA \"plan\" TO \"straum_plan\";
GRANT SELECT ON \"plan\".\"zones\", \"plan\".\"trips\", \"plan\".\"logins\" TO \"straum_plan\";
CREATE PUBLICATION \"plan\" FOR TABLE \"plan\".\"zones\", \"plan\".\"trips\", \"plan\".\"logins\";
-- fact trips: straum needs the old row of every update and delete; inserts cost no more.
ALTER TABLE \"plan\".\"trips\" REPLICA IDENTITY FULL;
-- fact logins: straum needs the old row of every update and delete; inserts cost no more.
ALTER TABLE \"plan\".\"logins\" REPLICA IDENTITY FULL;
-- dim zones: \"plan\".\"zones\" needs a primary key on id alone, or REPLICA IDENTITY FULL and a unique index on it.
"
    );
    // One statement at a time: ALTER SYSTEM can't run in a transaction.
    for statement in sql.lines().filter(|l| !l.starts_with("--")) {
        db.run(statement);
    }
    db.run("ALTER ROLE straum_plan PASSWORD 'plan'");
    let dsn = pg.dsn.to_string().replace("postgres@", "straum_plan:plan@");
    let pg = Pg::new(Dsn::parse(&dsn).unwrap(), "plan");
    let mut writer = Writer::new(App::new(spec, LIMITS), pg);
    writer.step().unwrap();
    db.run(&Rng::new(1).trip(1));
    applied(&mut writer);
    compare(&mut db, writer.app(), 2).expect("ready");
}

/// Opens with `spec` against `tables` and returns the setup problems.
fn problems(pg: &Postgres, name: &str, tables: &str, spec: &str) -> Vec<String> {
    let db = Db::new(pg, name, tables);
    match db.pg().open(&db.spec(spec)) {
        Ok(_) => Vec::new(),
        Err(e) => match e.downcast::<Error>().map(|e| *e) {
            Ok(Error::Setup(problems)) => problems,
            other => panic!("{name}: {other:?}"),
        },
    }
}

#[test]
fn setup_problems_say_what_to_fix() {
    let pg = postgres();
    let fact = |columns: &str, view: &str| -> (String, String) {
        (
            format!(
                "CREATE TABLE $S.f (id int PRIMARY KEY, at timestamptz, {columns});
                 ALTER TABLE $S.f REPLICA IDENTITY FULL;"
            ),
            format!(
                "CREATE VIEW f WITH (key = id) AS
                 SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket, {view}
                 FROM $S.f t GROUP BY 1;"
            ),
        )
    };
    let publish = "CREATE PUBLICATION $S FOR TABLE $S.f;";
    let (table, spec) = fact("x int", "count(*) AS n, sum(t.x) AS total");
    let cases: Vec<(&str, String, String, &str)> = vec![
        ("ok", format!("{table} {publish}"), spec.clone(), ""),
        (
            "nopub",
            table.clone(),
            spec.clone(),
            "publication nopub does not exist; run the SQL straum plan prints",
        ),
        (
            "notrunc",
            format!(
                "{table} CREATE PUBLICATION $S FOR TABLE $S.f WITH (publish = 'insert, update, delete');"
            ),
            spec.clone(),
            "publication notrunc must publish insert, update, delete and truncate",
        ),
        (
            "missing",
            "CREATE PUBLICATION $S;".into(),
            spec.clone(),
            "f: table missing.f does not exist",
        ),
        (
            "unpub",
            format!("{table} CREATE TABLE $S.g (); CREATE PUBLICATION $S FOR TABLE $S.g;"),
            spec.clone(),
            "f: publication unpub leaves out unpub.f: ALTER PUBLICATION unpub ADD TABLE unpub.f",
        ),
        (
            "partitioned",
            "CREATE TABLE $S.f (id int, at timestamptz, x int) PARTITION BY RANGE (id);
             CREATE PUBLICATION $S FOR TABLE $S.f;"
                .into(),
            spec.clone(),
            "f: partitioned.f is not a plain table",
        ),
        (
            "filtered",
            format!("{table} CREATE PUBLICATION $S FOR TABLE $S.f WHERE (x > 0);"),
            spec.clone(),
            "f: publication filtered filters the rows of filtered.f; straum needs every row",
        ),
        (
            "listed",
            format!("{table} CREATE PUBLICATION $S FOR TABLE $S.f (id, at);"),
            spec.clone(),
            "f: publication listed leaves out column x of listed.f",
        ),
        (
            "nocolumn",
            format!("CREATE TABLE $S.f (id int PRIMARY KEY, at timestamptz); {publish}"),
            spec.clone(),
            "f: nocolumn.f has no column x",
        ),
        (
            "nokey",
            format!("CREATE TABLE $S.f (at timestamptz, x int); {publish}"),
            spec.clone(),
            "f: nokey.f has no column id",
        ),
        (
            "float",
            format!("{} {publish}", fact("x float8", "").0),
            spec.clone(),
            "f: column x of float.f is double precision; \
             straum needs an integer type or numeric(p, s) with p at most 18 there",
        ),
        (
            "naive",
            format!("CREATE TABLE $S.f (id int PRIMARY KEY, at timestamp, x int); {publish}"),
            spec.clone(),
            "f: column at of naive.f is timestamp without time zone; straum needs timestamptz there",
        ),
        (
            "timekey",
            format!("{} {publish}", fact("x int, k timestamptz", "").0),
            fact("", "t.k, count(*) AS n")
                .1
                .replace("GROUP BY 1", "GROUP BY 1, 2"),
            "f: column k of timekey.f is timestamp with time zone; \
             straum needs an integer type, numeric(p, s), boolean or text there",
        ),
        (
            "identity",
            format!("CREATE TABLE $S.f (id int PRIMARY KEY, at timestamptz, x int); {publish}"),
            spec.clone(),
            "fact f: straum needs the whole old row of every update and delete: \
             ALTER TABLE identity.f REPLICA IDENTITY FULL",
        ),
        (
            "filter",
            format!("{} {publish}", fact("k text", "").0),
            fact("", "t.k, count(*) FILTER (WHERE t.k = 1) AS n")
                .1
                .replace("GROUP BY 1", "GROUP BY 1, 2"),
            "fact f: measure n compares k (text) with 1",
        ),
        (
            "filtertext",
            format!("{} {publish}", fact("k int", "").0),
            fact("", "t.k, count(*) FILTER (WHERE t.k = 'a') AS n")
                .1
                .replace("GROUP BY 1", "GROUP BY 1, 2"),
            "fact f: measure n compares k (an integer) with 'a'",
        ),
        (
            "filterbool",
            format!("{} {publish}", fact("k text", "").0),
            fact("", "t.k, count(*) FILTER (WHERE t.k = true) AS n")
                .1
                .replace("GROUP BY 1", "GROUP BY 1, 2"),
            "fact f: measure n compares k (text) with true",
        ),
    ];
    for (name, tables, spec, expected) in cases {
        let found = problems(&pg, name, &tables, &spec);
        let expected: Vec<&str> = [expected].into_iter().filter(|e| !e.is_empty()).collect();
        assert_eq!(found, expected, "{name}");
    }

    let dim_spec = "
        CREATE VIEW f WITH (key = id) AS
        SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket,
               t.d_id, d.name, count(*) AS n
        FROM $S.f t LEFT JOIN $S.d d ON d.k = t.d_id GROUP BY 1, 2, 3;";
    let dim = |d: &str, via: &str| {
        format!(
            "CREATE TABLE $S.f (id int PRIMARY KEY, at timestamptz, d_id {via});
             ALTER TABLE $S.f REPLICA IDENTITY FULL;
             {d}
             CREATE PUBLICATION $S FOR TABLE $S.f, $S.d;"
        )
    };
    let cases = [
        (
            "dimok",
            dim("CREATE TABLE $S.d (k int PRIMARY KEY, name text);", "int"),
            "",
        ),
        (
            "dimfull",
            dim(
                "CREATE TABLE $S.d (k int UNIQUE, name text); ALTER TABLE $S.d REPLICA IDENTITY FULL;",
                "int",
            ),
            "",
        ),
        (
            "dimpk",
            dim(
                "CREATE TABLE $S.d (s serial PRIMARY KEY, k int, name text);",
                "int",
            ),
            "dim d: the replica identity of dimpk.d must be k alone, so a delete names the row \
             straum holds: a primary key on k, REPLICA IDENTITY USING INDEX on a unique index of k, \
             or REPLICA IDENTITY FULL",
        ),
        (
            "dimnounique",
            dim(
                "CREATE TABLE $S.d (k int, name text); ALTER TABLE $S.d REPLICA IDENTITY FULL;",
                "int",
            ),
            "dim d: dimnounique.d needs a primary key or unique index on k alone",
        ),
        (
            "dimkind",
            dim("CREATE TABLE $S.d (k text PRIMARY KEY, name text);", "int"),
            "fact f: ref d joins d_id (an integer) to k (text); they must read alike",
        ),
    ];
    for (name, tables, expected) in cases {
        let found = problems(&pg, name, &tables, dim_spec);
        let expected: Vec<&str> = [expected].into_iter().filter(|e| !e.is_empty()).collect();
        assert_eq!(found, expected, "{name}");
    }
}

#[test]
fn reports_wal_level_and_read_access() {
    let pg = postgres();
    let mut db = Db::new(&pg, "access", TABLES);
    let spec = db.spec(SPEC);
    let mut catalog = catalog::query(&mut db.conn, &spec, "access").unwrap();
    assert!(catalog::check(&spec, &catalog, "access").is_ok());
    catalog.wal_level = "replica".into();
    assert_eq!(
        catalog::check(&spec, &catalog, "access"),
        Err(vec![
            "wal_level is replica; straum needs logical, which takes a restart".to_owned()
        ])
    );

    db.run("CREATE ROLE reader LOGIN REPLICATION PASSWORD 'r'; GRANT USAGE ON SCHEMA $S TO reader");
    let dsn = pg.dsn.to_string().replace("postgres@", "reader:r@");
    let mut reader = Pg::new(Dsn::parse(&dsn).unwrap(), "access");
    let err = reader.open(&spec).unwrap_err().to_string();
    for table in ["zones", "trips", "logins"] {
        let line =
            format!("straum's role can't read access.{table}: GRANT SELECT ON access.{table}");
        assert!(err.contains(&line), "{err}");
    }
}

#[test]
fn connection_errors() {
    let pg = postgres();
    let wrong = pg.dsn.to_string().replace("postgres@", "postgres:wrong@");
    let mut source = Pg::new(Dsn::parse(&wrong).unwrap(), "p");
    let spec = straum_notation::parse(&SPEC.replace("$S", "p")).unwrap();
    let err = source.open(&spec).unwrap_err().to_string();
    assert!(
        err.contains("password authentication failed") && err.contains("(28P01)"),
        "{err}"
    );

    let bare = Dsn::parse(&pg.dsn.to_string()).unwrap();
    let err = Pg::new(bare, "p").open(&spec).unwrap_err().to_string();
    assert_eq!(err, "PostgreSQL sent a password request; the DSN has none");

    let mut db = Db::new(&pg, "errors", "");
    let err = db.conn.execute("SELECT nope").unwrap_err().to_string();
    assert!(
        err.starts_with("PostgreSQL: column \"nope\" does not exist (42703)"),
        "{err}"
    );
    // The connection still works.
    assert_eq!(db.rows("SELECT 1"), [[Some("1".to_owned())]]);

    // Nothing listens on port 1.
    let closed = Dsn::parse("postgres://straum@127.0.0.1:1/straum").unwrap();
    let err = Pg::new(closed, "p").open(&spec).unwrap_err().to_string();
    assert!(err.starts_with("connection to PostgreSQL: "), "{err}");

    // An error's detail comes with its message.
    db.run("CREATE TABLE $S.u (id int PRIMARY KEY); INSERT INTO $S.u VALUES (1)");
    let err = db
        .conn
        .execute("INSERT INTO errors.u VALUES (1)")
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "PostgreSQL: duplicate key value violates unique constraint \"u_pkey\": \
         Key (id)=(1) already exists. (23505)"
    );

    // Before open there is nothing to read or load.
    let mut idle = db.pg();
    let stopped = "the replication stream stopped";
    let err = idle.next_commit(Duration::ZERO).unwrap_err();
    assert_eq!(err.to_string(), stopped);
    assert_eq!(idle.wal().unwrap_err().to_string(), stopped);
    let mut store = Store::new(spec.clone());
    let trips = spec.fact("trips").unwrap();
    let err = idle.load_fact(trips, store.rollup("trips").unwrap());
    assert_eq!(err.unwrap_err().to_string(), stopped);
}

#[test]
fn no_load_once_the_stream_runs() {
    let pg = postgres();
    let mut db = Db::new(&pg, "early", TABLES);
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    let start = source.open(&spec).unwrap();
    db.run(&Rng::new(1).trip(1));
    let commit = source.next_commit(LONG).unwrap().unwrap();
    assert!(commit.lsn > start);
    let mut store = Store::new(spec.clone());
    let err = source.load_fact(spec.fact("trips").unwrap(), store.rollup("trips").unwrap());
    assert_eq!(
        err.unwrap_err().to_string(),
        "the replication stream stopped"
    );
}

/// A message as PostgreSQL frames it: tag, length counting itself, body.
fn message(tag: u8, body: &[u8]) -> Vec<u8> {
    let len = i32::try_from(body.len() + 4).unwrap();
    let mut m = vec![tag];
    m.extend(len.to_be_bytes());
    m.extend(body);
    m
}

/// AuthenticationOk, then ReadyForQuery.
fn signed_in() -> Vec<u8> {
    let mut m = message(b'R', &0_i32.to_be_bytes());
    m.extend(message(b'Z', b"I"));
    m
}

fn copy_data(body: &[u8]) -> Vec<u8> {
    message(b'd', body)
}

fn keepalive(end: u64, reply: bool) -> Vec<u8> {
    let mut body = vec![b'k'];
    body.extend(end.to_be_bytes());
    body.extend(0_i64.to_be_bytes());
    body.push(u8::from(reply));
    copy_data(&body)
}

fn error_response(code: &str, text: &str) -> Vec<u8> {
    message(b'E', format!("SERROR\0C{code}\0M{text}\0\0").as_bytes())
}

/// A stand-in for PostgreSQL: writes each part after its pause, then stops
/// writing, reads until the client hangs up and returns what it sent.
fn scripted(parts: Vec<(Duration, Vec<u8>)>) -> (Dsn, thread::JoinHandle<Vec<u8>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        for (pause, bytes) in parts {
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
    });
    let dsn = format!("postgres://straum@127.0.0.1:{port}/straum?sslmode=disable");
    (Dsn::parse(&dsn).unwrap(), server)
}

fn now(bytes: Vec<u8>) -> (Duration, Vec<u8>) {
    (Duration::ZERO, bytes)
}

fn connect(parts: Vec<(Duration, Vec<u8>)>, replication: bool) -> Result<Conn, Error> {
    let (dsn, _server) = scripted(parts);
    Conn::connect(&dsn, replication)
}

#[test]
fn sign_in_errors() {
    let sasl = |mechanisms: &str| {
        let mut body = 10_i32.to_be_bytes().to_vec();
        body.extend(mechanisms.as_bytes());
        message(b'R', &body)
    };
    let cases = [
        (
            message(b'R', &3_i32.to_be_bytes()),
            "PostgreSQL sent an authentication request straum doesn't support; \
             straum signs in with SCRAM-SHA-256 or trust",
        ),
        (
            sasl("SCRAM-SHA-256-PLUS\0\0"),
            "PostgreSQL sent SASL mechanisms [\"SCRAM-SHA-256-PLUS\"]; straum needs SCRAM-SHA-256",
        ),
        (
            message(b'R', &11_i32.to_be_bytes()),
            "PostgreSQL sent SASL continue",
        ),
        (
            message(b'R', &12_i32.to_be_bytes()),
            "PostgreSQL sent SASL final",
        ),
        (
            error_response("28000", "no pg_hba.conf entry"),
            "PostgreSQL: no pg_hba.conf entry (28000)",
        ),
    ];
    for (reply, expected) in cases {
        let err = connect(vec![now(reply)], false).err().unwrap();
        assert_eq!(err.to_string(), expected);
    }
    // Notices and parameters along the way are fine.
    let mut chatty = message(b'N', b"SNOTICE\0Mhello\0\0");
    chatty.extend(message(b'S', b"TimeZone\0UTC\0"));
    chatty.extend(signed_in());
    assert!(connect(vec![now(chatty)], true).is_ok());
}

#[test]
fn query_errors() {
    // CopyInResponse: straum never copies in.
    let mut conn = connect(
        vec![now(signed_in()), now(message(b'G', &[0, 0, 0]))],
        false,
    )
    .unwrap();
    let err = conn.execute("SELECT 1").unwrap_err().to_string();
    assert_eq!(err, "PostgreSQL sent a message during a query");

    let mut row = 1_i16.to_be_bytes().to_vec();
    row.extend(1_i32.to_be_bytes());
    row.push(0xff);
    let mut conn = connect(vec![now(signed_in()), now(message(b'D', &row))], false).unwrap();
    let err = conn.execute("SELECT 1").unwrap_err().to_string();
    assert!(
        err.starts_with("connection to PostgreSQL: invalid utf-8"),
        "{err}"
    );

    // The server hangs up mid-query.
    let mut conn = connect(vec![now(signed_in())], false).unwrap();
    drop(conn.execute("SELECT 1"));
    let mut conn = connect(
        vec![now(signed_in()), (Duration::ZERO, b"D".to_vec())],
        false,
    )
    .unwrap();
    let err = conn.execute("SELECT 1").unwrap_err().to_string();
    assert_eq!(
        err,
        "connection to PostgreSQL: PostgreSQL closed the connection"
    );
}

#[test]
fn copy_both_errors() {
    let cases = [
        (
            error_response("42704", "replication slot \"x\" does not exist"),
            "PostgreSQL: replication slot \"x\" does not exist (42704)",
        ),
        (
            message(b'C', b"SELECT 1\0"),
            "PostgreSQL sent a message instead of CopyBothResponse",
        ),
    ];
    for (reply, expected) in cases {
        let mut conn = connect(vec![now(signed_in()), now(reply)], true).unwrap();
        let err = conn.copy_both("START_REPLICATION").unwrap_err();
        assert_eq!(err.to_string(), expected);
    }
}

/// A stream from a stand-in server that sends `parts` once it has started.
fn scripted_stream(parts: Vec<(Duration, Vec<u8>)>) -> (Stream, thread::JoinHandle<Vec<u8>>) {
    let mut start = signed_in();
    start.extend(message(b'N', b"SNOTICE\0Mstarting\0\0"));
    start.extend(message(b'W', &[0, 0, 0]));
    let (dsn, server) = scripted([vec![now(start)], parts].concat());
    let mut conn = Conn::connect(&dsn, true).unwrap();
    conn.copy_both("START_REPLICATION").unwrap();
    let stream = Stream::start(conn, Decoder::new(Vec::new()), Lsn(0x10)).unwrap();
    (stream, server)
}

#[test]
fn stream_errors() {
    let cases = [
        (
            copy_data(b"x"),
            "PostgreSQL sent a copy message straum doesn't know",
        ),
        (
            copy_data(b"w"),
            "PostgreSQL sent a copy message straum doesn't know",
        ),
        (
            error_response("57P01", "terminating connection"),
            "PostgreSQL: terminating connection (57P01)",
        ),
        (
            message(b'c', b""),
            "PostgreSQL sent a message that ended the stream",
        ),
        (
            Vec::new(),
            "connection to PostgreSQL: PostgreSQL closed the connection",
        ),
    ];
    for (reply, expected) in cases {
        let (mut stream, _server) = scripted_stream(vec![now(reply)]);
        assert_eq!(stream.next(LONG).unwrap_err().to_string(), expected);
    }
}

/// A commit with no changes, as XLogData messages.
#[test]
fn reads_the_servers_wal() {
    let pg = postgres();
    let mut db = Db::new(&pg, "wal", TABLES);
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    let start = source.open(&spec).unwrap();
    // A new slot has confirmed everything before its consistent point.
    let wal = source.wal().unwrap();
    assert_eq!(wal.confirmed, start);
    assert!(wal.restart <= start && start <= wal.server, "{wal:?}");

    db.run(&Rng::new(1).trip(1));
    let commit = source.next_commit(LONG).unwrap().unwrap();
    assert!(source.wal().unwrap().server >= commit.lsn);
    // Asking for the next commit says this one is applied, and the stream
    // tells the server within a status interval.
    assert_eq!(
        source.next_commit(Duration::from_millis(100)).unwrap(),
        None
    );
    let deadline = Instant::now() + LONG;
    while source.wal().unwrap().confirmed < commit.lsn {
        assert!(
            Instant::now() < deadline,
            "the slot never confirmed the commit"
        );
        thread::sleep(Duration::from_millis(100));
    }

    // The reading's connection dies; the next reading connects again.
    db.run(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
          WHERE backend_type = 'client backend' AND application_name = 'straum'
            AND pid <> pg_backend_pid()",
    );
    let err = source.wal().unwrap_err().to_string();
    assert!(
        err.contains("57P01") || err.starts_with("connection to PostgreSQL"),
        "{err}"
    );
    assert!(source.wal().unwrap().confirmed >= commit.lsn);

    // A slot the server doesn't have.
    let mut conn = Conn::connect(&pg.dsn, false).unwrap();
    let err = read_wal(&mut conn, "nope").unwrap_err().to_string();
    assert_eq!(err, "PostgreSQL sent no WAL position for slot nope");
}

#[test]
fn the_stream_waits_at_most_the_wait() {
    // The server stays quiet past the wait.
    let later = (Duration::from_secs(2), keepalive(0x18, false));
    let (mut stream, _server) = scripted_stream(vec![later]);
    let clock = Instant::now();
    assert_eq!(stream.next(Duration::from_millis(200)).unwrap(), None);
    assert!(clock.elapsed() >= Duration::from_millis(200));
}

fn empty_commit(end: u64) -> Vec<u8> {
    let xlog = |message: &[u8]| {
        let mut body = vec![b'w'];
        body.extend([0; 24]);
        body.extend(message);
        copy_data(&body)
    };
    let mut begin = vec![b'B'];
    begin.extend([0; 20]);
    let mut commit = vec![b'C', 0];
    commit.extend(0_u64.to_be_bytes());
    commit.extend(end.to_be_bytes());
    commit.extend(0_i64.to_be_bytes());
    [xlog(&begin), xlog(&commit)].concat()
}

/// The applied positions the client reported in standby status updates.
fn reported(received: &[u8]) -> Vec<u64> {
    // CopyData of 38 bytes holding a standby status update.
    let frame = [b'd', 0, 0, 0, 38, b'r'];
    received
        .windows(frame.len())
        .enumerate()
        .filter(|(_, w)| *w == frame)
        .map(|(i, _)| u64::from_be_bytes(received[i + 6..i + 14].try_into().unwrap()))
        .collect()
}

#[test]
fn the_stream_answers_and_reports() {
    let pause = Duration::from_millis(1_500);
    let commit = empty_commit(0x20);
    // A message split across a read timeout, a keepalive asking for an
    // answer between transactions, then a commit.
    let (head, tail) = commit.split_at(5);
    let (mut stream, server) = scripted_stream(vec![
        now(keepalive(0x18, false)),
        now(head.to_vec()),
        (pause, tail.to_vec()),
        (pause, keepalive(0x40, true)),
        now(empty_commit(0x50)),
    ]);
    assert_eq!(stream.next(LONG).unwrap().unwrap().lsn, Lsn(0x20));
    assert_eq!(stream.next(LONG).unwrap().unwrap().lsn, Lsn(0x50));
    drop(stream);
    // The start, then once the first commit is applied, everything up to
    // the keepalive at 0x40.
    let reported = reported(&server.join().unwrap());
    assert!(reported.contains(&0x40), "{reported:x?}");
    assert!(
        reported
            .iter()
            .all(|r| [0x10, 0x18, 0x20, 0x40].contains(r)),
        "{reported:x?}"
    );
}

#[test]
fn a_commit_handed_out_is_not_yet_applied() {
    // The writer takes the commit at 0x20 and never asks for the next, so
    // it may still be applying it: nothing past the start is reported, not
    // even the keepalive that follows.
    let (mut stream, server) = scripted_stream(vec![
        now(empty_commit(0x20)),
        (Duration::from_millis(500), keepalive(0x40, true)),
    ]);
    assert_eq!(stream.next(LONG).unwrap().unwrap().lsn, Lsn(0x20));
    thread::sleep(Duration::from_millis(1_500));
    drop(stream);
    let reported = reported(&server.join().unwrap());
    assert!(!reported.is_empty());
    assert!(reported.iter().all(|r| *r == 0x10), "{reported:x?}");
}

/// The taxi month of `bench/data` through straum: load, stream inserts and
/// dim changes, read the fact again after an update, and every time compare
/// the whole `taxi.sql` view with PostgreSQL. Slow, so ignored; fetch the
/// data with `bench/run.sh`, then run
/// `cargo test -p straum-pg --release taxi_month -- --ignored --nocapture`.
#[test]
#[ignore]
fn taxi_month() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let data = root
        .join("bench/data")
        .canonicalize()
        .expect("bench/data, from bench/run.sh");
    let spec_text =
        std::fs::read_to_string(root.join("crates/straum-notation/tests/taxi.sql")).unwrap();
    let container = GenericImage::new("postgres", "18")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stdout(READY))
        .with_wait_for(WaitFor::message_on_stderr(READY))
        .with_env_var("POSTGRES_PASSWORD", PASSWORD)
        .with_mount(
            Mount::bind_mount(data.to_str().unwrap(), "/data")
                .with_access_mode(AccessMode::ReadOnly),
        )
        .with_shm_size(1 << 30)
        .with_cmd(["-c", "wal_level=logical"])
        .start()
        .expect("Docker runs postgres:18");
    let port = (0..50)
        .find_map(|_| {
            let port = container.get_host_port_ipv4(5432).ok();
            if port.is_none() {
                thread::sleep(Duration::from_millis(100));
            }
            port
        })
        .unwrap();
    let dsn = Dsn::parse(&format!(
        "postgres://postgres:{PASSWORD}@127.0.0.1:{port}/postgres?sslmode=disable"
    ))
    .unwrap();
    let mut sql = Conn::connect(&dsn, false).unwrap();
    let clock = Instant::now();
    sql.execute(
        "CREATE TABLE taxi_zones (location_id int PRIMARY KEY, borough text, zone text, service_zone text);
         CREATE TABLE yellow_trips (
             id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
             vendor_id int, pickup_at timestamptz, pu_location_id int, do_location_id int,
             payment_type int, fare_amount numeric(12, 2), tip_amount numeric(12, 2),
             trip_distance numeric(12, 2));
         ALTER TABLE yellow_trips REPLICA IDENTITY FULL;
         SET timezone = 'America/New_York';
         COPY taxi_zones FROM '/data/taxi_zone_lookup.csv' WITH (format csv, header);
         COPY yellow_trips (vendor_id, pickup_at, pu_location_id, do_location_id, payment_type,
                            fare_amount, tip_amount, trip_distance)
           FROM '/data/trips_2024-01.csv' WITH (format csv, header);
         SET timezone = 'UTC';
         ANALYZE;
         CREATE PUBLICATION straum FOR TABLE taxi_zones, yellow_trips;",
    )
    .unwrap();
    let rows = |sql: &mut Conn, q: &str| sql.rows(q).unwrap();
    eprintln!(
        "copied {:?} trips in {:.1?}",
        rows(&mut sql, "SELECT count(*) FROM yellow_trips")[0][0],
        clock.elapsed()
    );

    let spec = straum_notation::parse(&spec_text).unwrap();
    let body = {
        let start = spec_text.find("SELECT").unwrap();
        spec_text[start..spec_text.rfind(';').unwrap()].to_owned()
    };
    let reference = format!(
        "SELECT extract(epoch FROM bucket)::int8, payment_type, vendor_id, pu_location_id,
                do_location_id, borough, zone, service_zone, trips, fare, tip_max,
                round(distance, 6), card, manhattan_pickups
           FROM ({body}) v"
    );
    let all = ask(
        "trips",
        &[
            "trips",
            "fare",
            "tip_max",
            "distance",
            "card",
            "manhattan_pickups",
        ],
        &[
            "bucket",
            "payment_type",
            "vendor_id",
            "pickup",
            "dropoff",
            "pickup.borough",
            "pickup.zone",
            "pickup.service_zone",
        ],
    );
    let all_scales = [0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 2, 0, 0];
    // A coarse ask, the use case: it reads every cell, as the one above
    // does, but answers few groups.
    let coarse = ask(
        "trips",
        &["trips", "fare", "tip_max", "card", "manhattan_pickups"],
        &["pickup.borough"],
    );
    let coarse_reference = format!(
        "SELECT borough,
                sum(trips), sum(fare), max(tip_max), sum(card), sum(manhattan_pickups)
           FROM ({body}) v
          GROUP BY 1"
    );
    let coarse_scales = [0, 0, 2, 2, 0, 0];
    // straum's time is the time `ask` holds the read lock.
    let compare =
        |sql: &mut Conn, app: &App, (ask, reference, scales): (&Ask, &str, &[u32]), when: &str| {
            let clock = Instant::now();
            let answered = app.ask(ask).unwrap();
            let asked = clock.elapsed();
            let mut got = printed(&answered.answer, scales);
            let clock = Instant::now();
            let mut expected = sql.rows(reference).unwrap();
            let queried = clock.elapsed();
            got.sort();
            expected.sort();
            let differ = got.iter().zip(&expected).position(|(g, e)| g != e);
            assert!(
                got.len() == expected.len() && differ.is_none(),
                "{when}: {} rows from straum, {} from PostgreSQL; first difference: {:?} vs {:?}",
                got.len(),
                expected.len(),
                differ.map(|i| &got[i]),
                differ.map(|i| &expected[i])
            );
            eprintln!(
                "{when}: {} groups equal; straum asked in {asked:.1?}, PostgreSQL in {queried:.1?}",
                got.len()
            );
        };
    let check = |sql: &mut Conn, app: &App, when: &str| {
        compare(sql, app, (&all, &reference, &all_scales), when);
        let when = format!("{when}, by day and borough");
        compare(
            sql,
            app,
            (&coarse, &coarse_reference, &coarse_scales),
            &when,
        );
    };

    let mut writer = Writer::new(App::new(spec, LIMITS), Pg::new(dsn, "straum"));
    let clock = Instant::now();
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    eprintln!("loaded in {:.1?}", clock.elapsed());
    check(&mut sql, writer.app(), "after the load");

    // Inserts in 200 transactions of 50, a day later than rows already there.
    let clock = Instant::now();
    for k in 0..200 {
        sql.execute(&format!(
            "INSERT INTO yellow_trips (vendor_id, pickup_at, pu_location_id, do_location_id,
                                      payment_type, fare_amount, tip_amount, trip_distance)
             SELECT vendor_id, pickup_at + interval '1 day', pu_location_id, do_location_id,
                    payment_type, fare_amount + 1, tip_amount, trip_distance
               FROM yellow_trips WHERE id % 10007 = {k} LIMIT 50"
        ))
        .unwrap();
    }
    // Dims change: a zone renamed, another moved to a new key.
    sql.execute("UPDATE taxi_zones SET zone = zone || ' (renamed)' WHERE location_id = 161")
        .unwrap();
    sql.execute("UPDATE taxi_zones SET location_id = 999 WHERE location_id = 264")
        .unwrap();
    for _ in 0..202 {
        applied(&mut writer);
    }
    eprintln!("streamed 202 commits in {:.1?}", clock.elapsed());
    check(&mut sql, writer.app(), "after the stream");

    // An update takes the old row out of its cell.
    sql.execute("UPDATE yellow_trips SET tip_amount = tip_amount + 1 WHERE id = 1")
        .unwrap();
    applied(&mut writer);
    sql.execute("UPDATE taxi_zones SET zone = zone WHERE location_id = 1")
        .unwrap();
    applied(&mut writer);
    check(&mut sql, writer.app(), "after an update");
}
