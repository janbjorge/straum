//! weir-pg against PostgreSQL 18 in Docker, one container per test. Every
//! answer is checked against the view body of the spec, run in PostgreSQL.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use testcontainers::core::{AccessMode, IntoContainerPort, Mount, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, GenericImage, ImageExt};
use weir_app::{AnswerError, App, Lsn, Source, Step};
use weir_domain::{Ask, AskKey, Datum, Ident, KeyPath, Spec, Store, TimeKey, Value, Zone};

use crate::catalog;
use crate::dsn::Dsn;
use crate::error::Error;
use crate::source::Pg;
use crate::stream::{Decoder, Stream};
use crate::wire::Conn;

const PASSWORD: &str = "weir";
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

CREATE VIEW logins WITH (key = id, append = true) AS
SELECT date_bin('1 day', l.at, timestamptz 'epoch') AS bucket,
       l.zone_id,
       count(*) AS logins
FROM $S.logins l
LEFT JOIN $S.zones pickup ON pickup.id = l.zone_id
GROUP BY 1, 2;
";

const TABLES: &str = "
CREATE TABLE $S.zones (id int PRIMARY KEY, borough text);
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
CREATE PUBLICATION $S FOR TABLE $S.zones, $S.trips, $S.logins;
";

const BOROUGHS: [&str; 3] = ["Manhattan", "Queens", "Bronx"];
const KINDS: [&str; 3] = ["cab", "pool", "van"];
const RIDERS: [&str; 4] = ["ann", "bo", "cy", "di"];

/// PostgreSQL 18 with logical decoding and a short `wal_sender_timeout`,
/// removed when dropped.
struct Postgres {
    _container: Container<GenericImage>,
    dsn: Dsn,
}

fn postgres() -> Postgres {
    let container = GenericImage::new("postgres", "18")
        .with_exposed_port(5432.tcp())
        // The init server says it on stdout, the real one on stderr.
        .with_wait_for(WaitFor::message_on_stdout(READY))
        .with_wait_for(WaitFor::message_on_stderr(READY))
        .with_env_var("POSTGRES_PASSWORD", PASSWORD)
        .with_env_var("POSTGRES_HOST_AUTH_METHOD", "scram-sha-256")
        .with_cmd(["-c", "wal_level=logical", "-c", "wal_sender_timeout=4s"])
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
    let dsn = format!("postgres://postgres:{PASSWORD}@127.0.0.1:{port}/postgres");
    Postgres {
        _container: container,
        dsn: Dsn::parse(&dsn).unwrap(),
    }
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
        weir_notation::parse(&self.sql(text)).unwrap()
    }

    fn pg(&self) -> Pg {
        Pg::new(self.dsn.clone(), &self.name)
    }

    fn app(&self) -> App<Pg> {
        App::new(self.spec(SPEC), self.pg())
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
        zone: Zone::utc(),
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

fn datum(d: &Datum) -> Option<String> {
    match d {
        Datum::Null => None,
        Datum::Int(n) => Some(n.to_string()),
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

/// Asks weir what each view body answers in PostgreSQL: trips by bucket,
/// kind, zone and borough; logins by day and zone. `fare_scale` follows
/// `fare`'s type. `None` when weir isn't ready.
fn compare(db: &mut Db, app: &App<Pg>, fare_scale: u32) -> Option<()> {
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
        vec![0, fare_scale, fare_scale, fare_scale, 0, 0, 0, 0],
    );
    let logins = (
        format!(
            "SELECT extract(epoch FROM bucket)::int8, zone_id, logins FROM ({}) v",
            body("logins")
        ),
        ask("logins", &["logins"], &["bucket", "pickup"]),
        vec![0],
    );
    for (sql, ask, scales) in [trips, logins] {
        let answered = match app.ask(&ask) {
            Ok(answered) => answered,
            Err(AnswerError::NotReady) => return None,
            Err(e) => panic!("{e}"),
        };
        let mut got: Vec<Vec<Option<String>>> = answered
            .answer
            .rows
            .iter()
            .map(|(key, values)| {
                key.iter()
                    .map(datum)
                    .chain(values.iter().zip(&scales).map(|(v, s)| text(v, *s)))
                    .collect()
            })
            .collect();
        let mut expected = db.rows(&sql);
        got.sort();
        expected.sort();
        assert_eq!(got, expected, "{}", ask.fact);
    }
    Some(())
}

fn applied(app: &mut App<Pg>) {
    match app.step() {
        Ok(Step::Applied(_)) => {}
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
            "INSERT INTO $S.zones VALUES ({zone}, '{}')",
            rng.pick(&BOROUGHS)
        ));
    }
    for _ in 0..400 {
        db.run(&rng.trip(next));
        trips.push(next);
        next += 1;
    }

    let mut app = db.app();
    assert!(matches!(app.step(), Ok(Step::Started(_))));
    compare(&mut db, &app, 2).expect("ready after the load");

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
                    "INSERT INTO $S.zones VALUES ({}, '{}') ON CONFLICT (id)
                     DO UPDATE SET borough = excluded.borough;",
                    1 + rng.below(6),
                    rng.pick(&BOROUGHS)
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
        // An append fact changed after all reads it again; truncates empty
        // a table in place, a mutable fact's included.
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
        applied(&mut app);
        if compare(&mut db, &app, 2).is_some() {
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
        let writer = {
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
        let mut app = db.app();
        assert!(matches!(app.step(), Ok(Step::Started(_))));
        assert!(
            !done.load(Ordering::Acquire),
            "round {round}: the writer finished before the load"
        );
        let loaded = app.ask(&ask("trips", &["trips"], &[])).unwrap();
        assert_ne!(loaded.answer.rows[0].1[0], Value::Int(0), "round {round}");
        writer.join().unwrap();
        db.run("INSERT INTO $S.trips VALUES (-1, now(), 'marker', 1, 1.00, 'ann', 2)");
        let marker = ask("trips", &["trips"], &["kind"]);
        let mut streamed = 0;
        loop {
            applied(&mut app);
            streamed += 1;
            let answered = app.ask(&marker).unwrap();
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
        compare(&mut db, &app, 2).expect("ready");
    }
}

#[test]
fn reads_a_table_again_on_a_second_connection() {
    let pg = postgres();
    let mut db = Db::new(&pg, "reload", TABLES);
    db.run("INSERT INTO $S.logins VALUES (1, to_timestamp(1705276800), 1)");
    let mut app = db.app();
    app.step().unwrap();
    compare(&mut db, &app, 2).expect("ready");

    // logins is append-only and has no full old rows, so an update reads
    // it again, at a new slot's consistent point past this commit.
    db.run("UPDATE $S.logins SET zone_id = 2");
    applied(&mut app);
    assert_eq!(
        app.ask(&ask("logins", &["logins"], &[])),
        Err(AnswerError::NotReady)
    );
    // Trips didn't change, so they keep answering.
    assert!(app.ask(&ask("trips", &["trips"], &[])).is_ok());
    db.run("INSERT INTO $S.logins VALUES (2, to_timestamp(1705276800), 3)");
    applied(&mut app);
    compare(&mut db, &app, 2).expect("ready once the stream passes the reload");
}

#[test]
fn a_changed_column_type_reads_the_table_again() {
    let pg = postgres();
    let mut db = Db::new(&pg, "altered", TABLES);
    db.run(&Rng::new(1).trip(1));
    let mut app = db.app();
    app.step().unwrap();
    compare(&mut db, &app, 2).expect("ready");

    db.run("ALTER TABLE $S.trips ALTER COLUMN fare TYPE numeric(12, 3)");
    db.run("INSERT INTO $S.trips VALUES (2, to_timestamp(1705276800), 'cab', 1, 2.125, 'bo', 3)");
    applied(&mut app);
    db.run(&Rng::new(2).trip(3));
    applied(&mut app);
    compare(&mut db, &app, 3).expect("ready once the stream passes the reload");

    // A column weir reads is gone: weir stops and the setup check says why.
    db.run("ALTER TABLE $S.trips DROP COLUMN rider");
    db.run(
        "INSERT INTO $S.trips (id, at, kind, zone_id, fare, tip)
         VALUES (4, to_timestamp(1705276800), 'cab', 1, 1.000, 1)",
    );
    let err = app.step().unwrap_err().to_string();
    assert!(err.contains("column rider of"), "{err}");
    let err = app.step().unwrap_err().to_string();
    assert!(err.contains("has no column rider"), "{err}");
}

#[test]
fn a_value_left_out_reads_the_table_again() {
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
    let mut app = db.app();
    app.step().unwrap();
    compare(&mut db, &app, 2).expect("ready");

    db.run("UPDATE $S.zones SET note = 'x' WHERE id = 1");
    applied(&mut app);
    assert!(compare(&mut db, &app, 2).is_none(), "zones is read again");
    // The reload's consistent point is past every commit so far; the next
    // one passes it.
    db.run(&Rng::new(2).trip(2));
    applied(&mut app);
    compare(&mut db, &app, 2).expect("ready once the stream passes the reload");
}

#[test]
fn the_stream_outlasts_a_busy_writer() {
    let pg = postgres();
    let mut db = Db::new(&pg, "busy", TABLES);
    let mut app = db.app();
    app.step().unwrap();
    // The stream starts with the first commit asked for.
    let mut rng = Rng::new(5);
    db.run(&rng.trip(0));
    applied(&mut app);
    // More commits than the queue holds, then a pause past
    // wal_sender_timeout (4 s): the thread keeps answering.
    for i in 1..101 {
        db.run(&rng.trip(i));
    }
    thread::sleep(Duration::from_secs(9));
    for _ in 0..100 {
        applied(&mut app);
    }
    // Commits made after the pause still arrive: the stream survived.
    for i in 101..111 {
        db.run(&rng.trip(i));
        applied(&mut app);
    }
    compare(&mut db, &app, 2).expect("ready");
}

#[test]
fn plan_sets_up_what_weir_needs() {
    let pg = postgres();
    let mut db = Db::new(&pg, "plan", &TABLES.replace("CREATE PUBLICATION", "-- "));
    db.run("ALTER TABLE $S.trips REPLICA IDENTITY DEFAULT");
    let spec = db.spec(SPEC);
    let sql = crate::plan(&spec, "plan", "weir_plan");
    assert_eq!(
        sql,
        "-- weir setup: run once, as an owner of the tables.
-- wal_level takes a restart. On Azure, set the server parameter instead.
ALTER SYSTEM SET wal_level = logical;
CREATE ROLE \"weir_plan\" LOGIN REPLICATION;
-- Then give \"weir_plan\" a password: \\password \"weir_plan\"
GRANT USAGE ON SCHEMA \"plan\" TO \"weir_plan\";
GRANT SELECT ON \"plan\".\"zones\", \"plan\".\"trips\", \"plan\".\"logins\" TO \"weir_plan\";
CREATE PUBLICATION \"plan\" FOR TABLE \"plan\".\"zones\", \"plan\".\"trips\", \"plan\".\"logins\";
-- fact trips is not append-only: weir needs the old row of every update and delete.
ALTER TABLE \"plan\".\"trips\" REPLICA IDENTITY FULL;
-- dim zones: \"plan\".\"zones\" needs a primary key on id alone, or REPLICA IDENTITY FULL and a unique index on it.
"
    );
    // One statement at a time: ALTER SYSTEM can't run in a transaction.
    for statement in sql.lines().filter(|l| !l.starts_with("--")) {
        db.run(statement);
    }
    db.run("ALTER ROLE weir_plan PASSWORD 'plan'");
    let dsn = pg.dsn.to_string().replace("postgres@", "weir_plan:plan@");
    let mut app = App::new(spec, Pg::new(Dsn::parse(&dsn).unwrap(), "plan"));
    app.step().unwrap();
    db.run(&Rng::new(1).trip(1));
    applied(&mut app);
    compare(&mut db, &app, 2).expect("ready");
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
            format!("CREATE TABLE $S.f (id int PRIMARY KEY, at timestamptz, {columns});"),
            format!(
                "CREATE VIEW f WITH (key = id, append = true) AS
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
            "publication nopub does not exist; run the SQL weir plan prints",
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
            "f: publication filtered filters the rows of filtered.f; weir needs every row",
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
             weir needs an integer type or numeric(p, s) with p at most 18 there",
        ),
        (
            "naive",
            format!("CREATE TABLE $S.f (id int PRIMARY KEY, at timestamp, x int); {publish}"),
            spec.clone(),
            "f: column at of naive.f is timestamp without time zone; weir needs timestamptz there",
        ),
        (
            "timekey",
            format!("{} {publish}", fact("x int, k timestamptz", "").0),
            fact("", "t.k, count(*) AS n")
                .1
                .replace("GROUP BY 1", "GROUP BY 1, 2"),
            "f: column k of timekey.f is timestamp with time zone; \
             weir needs an integer type, numeric(p, s), boolean or text there",
        ),
        (
            "mutable",
            format!("{table} {publish}"),
            spec.replace("append = true", "append = false"),
            "fact f: not append-only, so weir needs the whole old row of every update and \
             delete: ALTER TABLE mutable.f REPLICA IDENTITY FULL",
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
        CREATE VIEW f WITH (key = id, append = true) AS
        SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket,
               t.d_id, d.name, count(*) AS n
        FROM $S.f t LEFT JOIN $S.d d ON d.k = t.d_id GROUP BY 1, 2, 3;";
    let dim = |d: &str, via: &str| {
        format!(
            "CREATE TABLE $S.f (id int PRIMARY KEY, at timestamptz, d_id {via});
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
             weir holds: a primary key on k, REPLICA IDENTITY USING INDEX on a unique index of k, \
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
            "wal_level is replica; weir needs logical, which takes a restart".to_owned()
        ])
    );

    db.run("CREATE ROLE reader LOGIN REPLICATION PASSWORD 'r'; GRANT USAGE ON SCHEMA $S TO reader");
    let dsn = pg.dsn.to_string().replace("postgres@", "reader:r@");
    let mut reader = Pg::new(Dsn::parse(&dsn).unwrap(), "access");
    let err = reader.open(&spec).unwrap_err().to_string();
    for table in ["zones", "trips", "logins"] {
        let line = format!("weir's role can't read access.{table}: GRANT SELECT ON access.{table}");
        assert!(err.contains(&line), "{err}");
    }
}

#[test]
fn connection_errors() {
    let pg = postgres();
    let wrong = pg.dsn.to_string().replace("postgres@", "postgres:wrong@");
    let mut source = Pg::new(Dsn::parse(&wrong).unwrap(), "p");
    let spec = weir_notation::parse(&SPEC.replace("$S", "p")).unwrap();
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
    let closed = Dsn::parse("postgres://weir@127.0.0.1:1/weir").unwrap();
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
    assert_eq!(idle.next_commit().unwrap_err().to_string(), stopped);
    let mut store = Store::new(spec.clone());
    let trips = spec.fact("trips").unwrap();
    let err = idle.load_fact(trips, store.rollup("trips").unwrap());
    assert_eq!(err.unwrap_err().to_string(), stopped);
}

#[test]
fn streams_before_every_table_is_loaded() {
    let pg = postgres();
    let mut db = Db::new(&pg, "early", TABLES);
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    let start = source.open(&spec).unwrap();
    db.run(&Rng::new(1).trip(1));
    let commit = source.next_commit().unwrap();
    assert!(commit.lsn > start);
    // A table loaded now reads a snapshot of its own, past the commit.
    let mut store = Store::new(spec.clone());
    let at = source.load_fact(spec.fact("trips").unwrap(), store.rollup("trips").unwrap());
    assert!(at.unwrap() >= commit.lsn);
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
    let dsn = Dsn::parse(&format!("postgres://weir@127.0.0.1:{port}/weir")).unwrap();
    (dsn, server)
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
            "PostgreSQL sent an authentication request weir doesn't support; \
             weir signs in with SCRAM-SHA-256 or trust",
        ),
        (
            sasl("SCRAM-SHA-256-PLUS\0\0"),
            "PostgreSQL sent SASL mechanisms [\"SCRAM-SHA-256-PLUS\"]; weir needs SCRAM-SHA-256",
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
    // CopyInResponse: weir never copies in.
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
            "PostgreSQL sent a copy message weir doesn't know",
        ),
        (
            copy_data(b"w"),
            "PostgreSQL sent a copy message weir doesn't know",
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
        assert_eq!(stream.next().unwrap_err().to_string(), expected);
    }
}

/// A commit with no changes, as XLogData messages.
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
    assert_eq!(stream.next().unwrap().lsn, Lsn(0x20));
    assert_eq!(stream.next().unwrap().lsn, Lsn(0x50));
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
    assert_eq!(stream.next().unwrap().lsn, Lsn(0x20));
    thread::sleep(Duration::from_millis(1_500));
    drop(stream);
    let reported = reported(&server.join().unwrap());
    assert!(!reported.is_empty());
    assert!(reported.iter().all(|r| *r == 0x10), "{reported:x?}");
}

/// The taxi month of `bench/data` through weir: load, stream inserts and
/// dim changes, read the fact again after an update, and every time compare
/// the whole `taxi.sql` view with PostgreSQL. Slow, so ignored; fetch the
/// data with `bench/run.sh`, then run
/// `cargo test -p weir-pg --release taxi_month -- --ignored --nocapture`.
#[test]
#[ignore]
fn taxi_month() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let data = root
        .join("bench/data")
        .canonicalize()
        .expect("bench/data, from bench/run.sh");
    let spec_text =
        std::fs::read_to_string(root.join("crates/weir-notation/tests/taxi.sql")).unwrap();
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
        "postgres://postgres:{PASSWORD}@127.0.0.1:{port}/postgres"
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
         SET timezone = 'America/New_York';
         COPY taxi_zones FROM '/data/taxi_zone_lookup.csv' WITH (format csv, header);
         COPY yellow_trips (vendor_id, pickup_at, pu_location_id, do_location_id, payment_type,
                            fare_amount, tip_amount, trip_distance)
           FROM '/data/trips_2024-01.csv' WITH (format csv, header);
         SET timezone = 'UTC';
         ANALYZE;
         CREATE PUBLICATION weir FOR TABLE taxi_zones, yellow_trips;",
    )
    .unwrap();
    let rows = |sql: &mut Conn, q: &str| sql.rows(q).unwrap();
    eprintln!(
        "copied {:?} trips in {:.1?}",
        rows(&mut sql, "SELECT count(*) FROM yellow_trips")[0][0],
        clock.elapsed()
    );

    let spec = weir_notation::parse(&spec_text).unwrap();
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
    let scales = [0, 2, 2, 2, 0, 0];
    let check = |sql: &mut Conn, app: &App<Pg>, when: &str| {
        let clock = Instant::now();
        let answered = app.ask(&all).unwrap();
        let mut got: Vec<Vec<Option<String>>> = answered
            .answer
            .rows
            .iter()
            .map(|(key, values)| {
                key.iter()
                    .map(datum)
                    .chain(values.iter().zip(scales).map(|(v, s)| text(v, s)))
                    .collect()
            })
            .collect();
        let asked = clock.elapsed();
        let clock = Instant::now();
        let mut expected = sql.rows(&reference).unwrap();
        let queried = clock.elapsed();
        got.sort();
        expected.sort();
        let differ = got.iter().zip(&expected).position(|(g, e)| g != e);
        assert!(
            got.len() == expected.len() && differ.is_none(),
            "{when}: {} rows from weir, {} from PostgreSQL; first difference: {:?} vs {:?}",
            got.len(),
            expected.len(),
            differ.map(|i| &got[i]),
            differ.map(|i| &expected[i])
        );
        eprintln!(
            "{when}: {} groups equal; weir asked in {asked:.1?}, PostgreSQL in {queried:.1?}",
            got.len()
        );
    };

    let mut app = App::new(spec, Pg::new(dsn, "weir"));
    let clock = Instant::now();
    assert!(matches!(app.step(), Ok(Step::Started(_))));
    eprintln!("loaded in {:.1?}", clock.elapsed());
    check(&mut sql, &app, "after the load");

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
        applied(&mut app);
    }
    eprintln!("streamed 202 commits in {:.1?}", clock.elapsed());
    check(&mut sql, &app, "after the stream");

    // trips is append-only: an update reads it all again.
    sql.execute("UPDATE yellow_trips SET tip_amount = tip_amount + 1 WHERE id = 1")
        .unwrap();
    let clock = Instant::now();
    applied(&mut app);
    eprintln!("read trips again in {:.1?}", clock.elapsed());
    sql.execute("UPDATE taxi_zones SET zone = zone WHERE location_id = 1")
        .unwrap();
    applied(&mut app);
    check(&mut sql, &app, "after the reload");
}
