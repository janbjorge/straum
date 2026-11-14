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

use straum_app::{AnswerError, App, Limits, Lsn, Source, SourceError, Step, Writer};
use straum_domain::{
    Answer, AnswerRow, Ask, AskError, AskKey, ColumnType, Condition, Datum, Identifier, Literal,
    Spec, Store, TimeKey, Value, decimal,
};
use testcontainers::core::{ExecCommand, IntoContainerPort, WaitFor};
use testcontainers::runners::SyncRunner;
use testcontainers::{Container, ContainerRequest, GenericImage, ImageExt};

use crate::catalog::{self, Checked, Tables};
use crate::decode::Decoder;
use crate::dsn::Dsn;
use crate::error::Error;
use crate::load;
use crate::source::{Postgres, parse_lsn, read_wal};
use crate::stream::Stream;
use crate::value::read;
use crate::wire::Connection;

/// The tasks spec as checked: a fact on two refs, one keyed by two
/// columns, with the tables the catalog would give. For the tests that
/// build SQL without a server.
pub(crate) fn tasks_tables() -> (Spec, Tables) {
    let spec = straum_notation::parse(
        "CREATE VIEW tasks WITH (buckets = '1 hour') AS
         SELECT date_bin('1 hour', f.created, timestamp 'epoch') AS bucket,
                f.organization_id AS organization_id, g.category AS category,
                l.location_type AS location_type, count(*) AS tasks
         FROM public.tasks AS f
         LEFT OUTER JOIN public.taskgroups AS g
                ON g.organization_id = f.organization_id AND g.taskgroup_id = f.taskgroup_id
         LEFT OUTER JOIN public.locations AS l ON l.id = f.location_id
         GROUP BY 1, 2, 3, 4",
    )
    .unwrap();
    let table = |name: &str, columns: &[&str], types: Vec<ColumnType>| Checked {
        enums: Vec::new(),
        name: id(name),
        schema: id("public"),
        table: id(name),
        columns: columns.iter().map(|c| id(c)).collect(),
        types,
    };
    let tables = Tables {
        facts: vec![table(
            "tasks",
            &["created", "organization_id", "taskgroup_id", "location_id"],
            vec![
                ColumnType::Time { zoned: false },
                ColumnType::Int,
                ColumnType::Int,
                ColumnType::Numeric { scale: 1 },
            ],
        )],
        dims: vec![
            table(
                "taskgroups",
                &["organization_id", "taskgroup_id", "category"],
                vec![ColumnType::Int, ColumnType::Int, ColumnType::Text],
            ),
            table(
                "locations",
                &["id", "location_type"],
                vec![ColumnType::Numeric { scale: 1 }, ColumnType::Text],
            ),
        ],
    };
    (spec, tables)
}

/// A load copies each table alone, the columns the spec reads of it: a
/// fact's columns, a dim's key and attributes.
#[test]
fn loads_copy_each_table_alone() {
    let (_, tables) = tasks_tables();
    assert_eq!(
        load::copy_sql(&tables.facts[0]),
        "COPY (SELECT \"created\", \"organization_id\", \"taskgroup_id\", \"location_id\" \
         FROM \"public\".\"tasks\") TO STDOUT (FORMAT binary)"
    );
    assert_eq!(
        load::copy_sql(&tables.dims[0]),
        "COPY (SELECT \"organization_id\", \"taskgroup_id\", \"category\" \
         FROM \"public\".\"taskgroups\") TO STDOUT (FORMAT binary)"
    );
}

const PASSWORD: &str = "straum";
/// Long enough for anything a test waits on to come.
const LONG: Duration = Duration::from_secs(10);
const LIMITS: Limits = Limits {
    max_lag: Duration::from_secs(30),
    max_retained_wal: 4 << 30,
};
/// Rows a transaction may change before Straum starts over; no test but
/// the one for it comes near.
const ROW_LIMIT: usize = 10_000;
const READY: &str = "database system is ready to accept connections";
/// 2100-01-01, on every bucket edge.
const END: i64 = 4_102_444_800;
/// 2024-01-15, where test rows start.
const START: i64 = 1_705_276_800;
/// From 5 hours before `START` to 29 after: the day bucket `START` begins
/// and the hours at either end.
const WINDOW_FROM: i64 = START - 5 * 3_600;
const WINDOW_TO: i64 = START + 29 * 3_600;

/// The spec the taxi-shaped tests follow. `$S` is the test's schema.
const SPEC: &str = "
CREATE VIEW trips WITH (buckets = '1 hour, 1 day') AS
SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket,
       f.kind AS kind,
       f.zone_id AS zone_id,
       pickup.borough AS borough,
       count(*) AS trips,
       sum(f.fare) AS fare,
       max(f.fare) AS fare_max,
       avg(f.fare) AS fare_avg,
       count(DISTINCT f.rider) AS riders,
       count(f.rider) AS named,
       sum(f.tip) AS tips,
       count(*) FILTER (WHERE pickup.borough = 'Manhattan') AS manhattan,
       count(*) FILTER (WHERE f.zone_id IN (1, 2)) AS near
FROM $S.trips AS f
LEFT OUTER JOIN $S.zones AS pickup ON pickup.id = f.zone_id
GROUP BY 1, 2, 3, 4;

CREATE VIEW logins WITH (buckets = '1 day, 7 days') AS
SELECT date_bin('1 day', l.at, timestamptz 'epoch') AS bucket,
       l.zone_id AS zone_id,
       pickup.toll AS toll,
       count(*) AS logins
FROM $S.logins AS l
LEFT OUTER JOIN $S.zones AS pickup ON pickup.id = l.zone_id
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
ALTER TABLE $S.zones REPLICA IDENTITY FULL;
CREATE PUBLICATION $S FOR TABLE $S.zones, $S.trips, $S.logins;
";

/// A warehouse-shaped spec: tasks per hour, organization and task group
/// over a `timestamp` column, which has no zone, with the task groups a
/// dim keyed by two columns, one of them grouped by too. `$S` is the
/// test's schema.
const TASKS_SPEC: &str = "
CREATE VIEW tasks WITH (buckets = '1 hour, 1 day') AS
SELECT date_bin('1 hour', f.created, timestamp 'epoch') AS bucket,
       f.organization_id AS organization_id,
       f.taskgroup_id AS taskgroup_id,
       (f.completed IS NOT NULL) AS done,
       g.category::text AS category,
       f.kind::text AS kind,
       count(*) AS tasks,
       sum(f.picks) AS picks,
       max(f.picks) AS picks_max,
       count(*) FILTER (WHERE g.category::text = 'pick') AS pick_tasks,
       count(*) FILTER (WHERE f.completed IS NOT NULL) AS completed_tasks,
       count(*) FILTER (WHERE f.kind::text = 'shelf') AS shelf_tasks
FROM $S.tasks AS f
LEFT OUTER JOIN $S.taskgroups AS g
       ON g.organization_id = f.organization_id AND g.taskgroup_id = f.taskgroup_id
GROUP BY 1, 2, 3, 4, 5, 6;
";

/// The task tables keyed by the group's category alone: the group itself
/// is not selected, so the cells are keyed by the category's few values.
const PICKS_SPEC: &str = "
CREATE VIEW picks WITH (buckets = '1 hour, 1 day') AS
SELECT date_bin('1 hour', f.created, timestamp 'epoch') AS bucket,
       f.organization_id AS organization_id,
       g.category::text AS category,
       (f.completed IS NOT NULL) AS done,
       count(*) AS tasks,
       sum(f.picks) AS picks,
       count(*) FILTER (WHERE g.category::text = 'pick') AS pick_tasks
FROM $S.tasks AS f
LEFT OUTER JOIN $S.taskgroups AS g
       ON g.organization_id = f.organization_id AND g.taskgroup_id = f.taskgroup_id
GROUP BY 1, 2, 3, 4;
";

const TASKS: &str = "
CREATE TYPE $S.category AS ENUM ('pick', 'pack', 'count', 'other');
CREATE TYPE $S.kind AS ENUM ('grid', 'shelf');
CREATE TABLE $S.taskgroups (
    organization_id int,
    taskgroup_id int,
    category $S.category,
    PRIMARY KEY (organization_id, taskgroup_id)
);
CREATE TABLE $S.tasks (
    task_id int PRIMARY KEY,
    organization_id int,
    taskgroup_id int,
    created timestamp NOT NULL,
    completed timestamp,
    picks int,
    kind $S.kind NOT NULL DEFAULT 'grid'
);
ALTER TABLE $S.tasks REPLICA IDENTITY FULL;
ALTER TABLE $S.taskgroups REPLICA IDENTITY FULL;
CREATE PUBLICATION $S FOR TABLE $S.taskgroups, $S.tasks;
";

/// The last of each holds what `COPY` escapes, so loads read escapes back.
const BOROUGHS: [&str; 4] = ["Manhattan", "Queens", "Bronx", "Bø\tro\\ugh\n"];
const KINDS: [&str; 4] = ["cab", "pool", "van", "e\\bike\r\x08\x0b\x0c"];
const RIDERS: [&str; 4] = ["ann", "bo", "cy", "di"];

/// Logical decoding and a short `wal_sender_timeout`.
const FLAGS: [&str; 4] = ["-c", "wal_level=logical", "-c", "wal_sender_timeout=4s"];

/// The first major where `wal_level = replica` is enough; `19rc1` until 19
/// ships.
const NINETEEN: &str = "19rc1";

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
struct PostgresContainer {
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

/// CI sets `STRAUM_TEST_POSTGRES` to each major Straum supports.
fn tag() -> String {
    std::env::var("STRAUM_TEST_POSTGRES").unwrap_or_else(|_| "18".to_owned())
}

fn postgres() -> PostgresContainer {
    plain(image(&tag()).with_cmd(FLAGS))
}

fn postgres_at_replica(tag: &str) -> PostgresContainer {
    plain(image(tag).with_cmd(["-c", "wal_level=replica"]))
}

fn plain(request: ContainerRequest<GenericImage>) -> PostgresContainer {
    let (container, port) = start(request);
    let dsn = format!("postgres://postgres:{PASSWORD}@127.0.0.1:{port}/postgres?sslmode=disable");
    PostgresContainer {
        _container: container,
        dsn: Dsn::parse(&dsn).unwrap(),
        _ca: None,
    }
}

/// As [`postgres`], with ssl on. The DSN trusts the test's CA alone.
fn postgres_tls() -> PostgresContainer {
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
    let (container, port) = start(image(&tag()).with_entrypoint("sh").with_cmd(cmd));
    let mut cat = container
        .exec(ExecCommand::new(["cat", "/tmp/ca.pem"]))
        .unwrap();
    let ca = TempFile::new(&format!("ca-{port}.pem"), &cat.stdout_to_vec().unwrap());
    let dsn = format!(
        "postgres://postgres:{PASSWORD}@127.0.0.1:{port}/postgres?sslrootcert={}",
        ca.0.display()
    );
    PostgresContainer {
        _container: container,
        dsn: Dsn::parse(&dsn).unwrap(),
        _ca: Some(ca),
    }
}

/// The postgres image at `tag`, ready once PostgreSQL takes connections.
fn image(tag: &str) -> GenericImage {
    GenericImage::new("postgres", tag)
        .with_exposed_port(5432.tcp())
        // The init server says it on stdout, the real one on stderr.
        .with_wait_for(WaitFor::message_on_stdout(READY))
        .with_wait_for(WaitFor::message_on_stderr(READY))
}

/// Runs `request` and waits until PostgreSQL takes connections; returns
/// the container and its port on the host.
fn start(request: ContainerRequest<GenericImage>) -> (Container<GenericImage>, u16) {
    let container = request
        .with_env_var("POSTGRES_PASSWORD", PASSWORD)
        .with_env_var("POSTGRES_HOST_AUTH_METHOD", "scram-sha-256")
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
    (container, port)
}

/// A plain SQL connection, plus the test's schema and publication, both
/// named `name`.
struct Db {
    conn: Connection,
    name: String,
    dsn: Dsn,
}

impl Db {
    fn new(pg: &PostgresContainer, name: &str, tables: &str) -> Db {
        let mut db = Db {
            conn: Connection::connect(&pg.dsn, false).unwrap(),
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

    fn pg(&self) -> Postgres {
        Postgres::new(self.dsn.clone(), &self.name, ROW_LIMIT)
    }

    fn writer(&self) -> Writer<Postgres> {
        Writer::new(App::new(self.spec(SPEC), LIMITS, None), self.pg())
    }
}

fn id(s: &str) -> Identifier {
    Identifier::new(s).unwrap()
}

fn ask(fact: &str, measures: &[&str], by: &[&str]) -> Ask {
    Ask {
        view: id(fact),
        measures: measures.iter().map(|m| id(m)).collect(),
        keys: by
            .iter()
            .map(|k| match *k {
                "bucket" => AskKey::Time(TimeKey::Bucket),
                k => AskKey::Key(id(k)),
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
        Value::Bool(b) => Some(if b { "t" } else { "f" }.to_owned()),
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
fn datum(d: &Datum, scale: u32) -> Option<String> {
    match d {
        Datum::Null => None,
        Datum::Int(n) => Some(decimal(i128::from(*n), scale)),
        Datum::Text(s) => Some(s.to_string()),
        Datum::Bool(b) => Some(if *b { "t" } else { "f" }.to_owned()),
    }
}

/// The `SELECT` of a view in `spec`.
fn body(spec: &str, view: &str) -> String {
    let start = spec.find(&format!("CREATE VIEW {view} ")).unwrap();
    let rest = &spec[start..];
    let select = rest.find("SELECT").unwrap();
    let end = rest.find(';').unwrap();
    rest[select..end].to_owned()
}

/// What an ask must equal in PostgreSQL: the SQL, the ask, and the scale
/// each printed column is compared at.
struct Reference {
    sql: String,
    ask: Ask,
    scales: Vec<u32>,
}

/// The taxi spec's references: trips by bucket, kind, zone and borough; logins
/// by day, zone and toll; then trips without the bucket from 5 hours before
/// `START` to 29 after, which reads the day bucket `START` begins and the
/// hours at either end, against the raw rows. `fare_scale` follows `fare`'s
/// type.
fn taxi_oracles(fare_scale: u32) -> Vec<Reference> {
    const MEASURES: [&str; 9] = [
        "trips",
        "fare",
        "fare_max",
        "fare_avg",
        "riders",
        "named",
        "tips",
        "manhattan",
        "near",
    ];
    let trips = Reference {
        sql: format!(
            "SELECT extract(epoch FROM bucket)::int8, kind, zone_id, borough, trips, fare,
                    fare_max, round(fare_avg, 6), riders, named, tips, manhattan, near
               FROM ({}) v",
            body(SPEC, "trips")
        ),
        ask: ask(
            "trips",
            &MEASURES,
            &["bucket", "kind", "zone_id", "borough"],
        ),
        scales: vec![
            0, 0, 0, 0, 0, fare_scale, fare_scale, fare_scale, 0, 0, 0, 0, 0,
        ],
    };
    let logins = Reference {
        sql: format!(
            "SELECT extract(epoch FROM bucket)::int8, zone_id, toll, logins FROM ({}) v",
            body(SPEC, "logins")
        ),
        ask: ask("logins", &["logins"], &["bucket", "zone_id", "toll"]),
        scales: vec![0, 0, 1, 0],
    };
    let window = Reference {
        sql: format!(
            "SELECT f.kind, f.zone_id, pickup.borough, count(*), sum(f.fare), max(f.fare),
                    round(avg(f.fare), 6), count(DISTINCT f.rider), count(f.rider), sum(f.tip),
                    count(*) FILTER (WHERE pickup.borough = 'Manhattan'),
                    count(*) FILTER (WHERE f.zone_id IN (1, 2))
               FROM $S.trips f LEFT JOIN $S.zones pickup ON pickup.id = f.zone_id
              WHERE f.at >= to_timestamp({WINDOW_FROM}) AND f.at < to_timestamp({WINDOW_TO})
              GROUP BY 1, 2, 3"
        ),
        ask: Ask {
            from: WINDOW_FROM,
            to: WINDOW_TO,
            ..ask("trips", &MEASURES, &["kind", "zone_id", "borough"])
        },
        scales: vec![
            0, 0, 0, 0, fare_scale, fare_scale, fare_scale, 0, 0, 0, 0, 0,
        ],
    };
    vec![trips, logins, window]
}

/// The tasks spec's references: by bucket, organization, task group and
/// category, then by organization and category without the bucket over a
/// window that cuts the day bucket `START` begins. A `timestamp` has no
/// zone, so the window's bounds are wall clock in UTC.
fn task_oracles() -> Vec<Reference> {
    const MEASURES: [&str; 6] = [
        "tasks",
        "picks",
        "picks_max",
        "pick_tasks",
        "completed_tasks",
        "shelf_tasks",
    ];
    // The ask covers every finite bucket; a row at an infinite time is in
    // none, and the view in PostgreSQL would answer it on its own.
    let by_bucket = Reference {
        sql: format!(
            "SELECT extract(epoch FROM bucket)::int8, organization_id, taskgroup_id, done,
                    category, kind, tasks, picks, picks_max, pick_tasks, completed_tasks,
                    shelf_tasks
               FROM ({}) v
              WHERE bucket >= to_timestamp(0) AT TIME ZONE 'UTC'
                AND bucket < to_timestamp({END}) AT TIME ZONE 'UTC'",
            body(TASKS_SPEC, "tasks")
        ),
        ask: ask(
            "tasks",
            &MEASURES,
            &[
                "bucket",
                "organization_id",
                "taskgroup_id",
                "done",
                "category",
                "kind",
            ],
        ),
        scales: vec![0; 12],
    };
    let window = Reference {
        sql: format!(
            "SELECT f.organization_id, g.category, count(*), sum(f.picks), max(f.picks),
                    count(*) FILTER (WHERE g.category = 'pick'),
                    count(*) FILTER (WHERE f.completed IS NOT NULL),
                    count(*) FILTER (WHERE f.kind = 'shelf')
               FROM $S.tasks f
               LEFT JOIN $S.taskgroups g
                      ON g.organization_id = f.organization_id
                     AND g.taskgroup_id = f.taskgroup_id
              WHERE f.created >= to_timestamp({WINDOW_FROM}) AT TIME ZONE 'UTC'
                AND f.created < to_timestamp({WINDOW_TO}) AT TIME ZONE 'UTC'
              GROUP BY 1, 2"
        ),
        ask: Ask {
            from: WINDOW_FROM,
            to: WINDOW_TO,
            ..ask("tasks", &MEASURES, &["organization_id", "category"])
        },
        scales: vec![0; 8],
    };
    // The null-ness key as a filter, against the test it stands for.
    let open = Reference {
        sql: format!(
            "SELECT f.organization_id, count(*), sum(f.picks)
               FROM $S.tasks f
              WHERE f.completed IS NULL
                AND f.created >= to_timestamp({WINDOW_FROM}) AT TIME ZONE 'UTC'
                AND f.created < to_timestamp({WINDOW_TO}) AT TIME ZONE 'UTC'
              GROUP BY 1"
        ),
        ask: Ask {
            from: WINDOW_FROM,
            to: WINDOW_TO,
            filter: vec![Condition {
                key: id("done"),
                values: vec![Literal::Bool(false)],
            }],
            ..ask("tasks", &["tasks", "picks"], &["organization_id"])
        },
        scales: vec![0; 3],
    };
    vec![by_bucket, window, open]
}

/// The picks view's references: by bucket, organization, category and done,
/// then by organization and category over a window.
fn picks_oracles() -> Vec<Reference> {
    const MEASURES: [&str; 3] = ["tasks", "picks", "pick_tasks"];
    let by_bucket = Reference {
        sql: format!(
            "SELECT extract(epoch FROM bucket)::int8, organization_id, category, done,
                    tasks, picks, pick_tasks
               FROM ({}) v
              WHERE bucket >= to_timestamp(0) AT TIME ZONE 'UTC'
                AND bucket < to_timestamp({END}) AT TIME ZONE 'UTC'",
            body(PICKS_SPEC, "picks")
        ),
        ask: ask(
            "picks",
            &MEASURES,
            &["bucket", "organization_id", "category", "done"],
        ),
        scales: vec![0; 7],
    };
    let window = Reference {
        sql: format!(
            "SELECT f.organization_id, g.category, count(*), sum(f.picks),
                    count(*) FILTER (WHERE g.category = 'pick')
               FROM $S.tasks f
               LEFT JOIN $S.taskgroups g
                      ON g.organization_id = f.organization_id
                     AND g.taskgroup_id = f.taskgroup_id
              WHERE f.created >= to_timestamp({WINDOW_FROM}) AT TIME ZONE 'UTC'
                AND f.created < to_timestamp({WINDOW_TO}) AT TIME ZONE 'UTC'
              GROUP BY 1, 2"
        ),
        ask: Ask {
            from: WINDOW_FROM,
            to: WINDOW_TO,
            ..ask("picks", &MEASURES, &["organization_id", "category"])
        },
        scales: vec![0; 5],
    };
    vec![by_bucket, window]
}

/// Asks Straum what each reference's SQL answers in PostgreSQL and compares the
/// rows. `None` when Straum isn't ready.
fn compare(db: &mut Db, app: &App, references: &[Reference]) -> Option<()> {
    for Reference { sql, ask, scales } in references {
        let answered = match app.ask(ask) {
            Ok(answered) => answered,
            Err(AnswerError::NotReady) => return None,
            Err(e) => panic!("{e}"),
        };
        let got_scales: Vec<u32> = answered.answer.columns.iter().map(|c| c.scale).collect();
        assert_eq!(got_scales, *scales, "{}", ask.view);
        let mut got = printed(&answered.answer, scales);
        let mut expected = db.rows(sql);
        got.sort();
        expected.sort();
        assert_eq!(got, expected, "{}", ask.view);
    }
    Some(())
}

/// An answer's rows as PostgreSQL prints them, at `scales`.
fn printed(answer: &Answer, scales: &[u32]) -> Vec<Vec<Option<String>>> {
    (answer.rows.iter())
        .map(|AnswerRow { keys, values }| {
            let (key_scales, value_scales) = scales.split_at(keys.len());
            (keys.iter().zip(key_scales).map(|(k, s)| datum(k, *s)))
                .chain(values.iter().zip(value_scales).map(|(v, s)| text(v, *s)))
                .collect()
        })
        .collect()
}

fn applied(writer: &mut Writer<Postgres>) {
    match writer.step() {
        Ok(Step::Applied(_)) => {}
        other => panic!("expected a commit, got {other:?}"),
    }
}

/// Applies the next commit, or starts over when it doesn't fit a table.
fn applied_or_started(writer: &mut Writer<Postgres>) {
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

    /// A task row over a `timestamp` column: the literal is the UTC wall
    /// clock of `at`, whatever zone the session has. Task groups 3 and 4
    /// have no row, so their category is null.
    fn task(&mut self, id: u64) -> String {
        let at = START + self.below(self.hours * 3_600) as i64;
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
    // Six hours of trips, so a cell holds several rows.
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
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready after the load");

    let mut compared = 0;
    for round in 0..120 {
        // Every transaction inserts a trip, so each is one commit.
        let mut sql = vec!["BEGIN;".to_owned(), rng.trip(next)];
        trips.push(next);
        next += 1;
        // The zones stand still here; `a_dim_change_shows_at_once` and
        // `follows_the_task_tables` move a dim.
        for _ in 0..rng.below(4) {
            let pick = trips[rng.below(trips.len() as u64) as usize];
            let statement = match rng.below(8) {
                0 | 1 => format!(
                    "UPDATE $S.trips SET fare = fare + 1, kind = '{}' WHERE id = {pick};",
                    rng.pick(&KINDS)
                ),
                2 => format!("UPDATE $S.trips SET at = at + interval '5 hours' WHERE id = {pick};"),
                3 => {
                    trips.retain(|t| *t != pick);
                    format!("DELETE FROM $S.trips WHERE id = {pick};")
                }
                // Into zones 5 and 6 too, which have no row.
                4 | 5 => format!(
                    "UPDATE $S.trips SET zone_id = {} WHERE id = {pick};",
                    1 + rng.below(6)
                ),
                6 => format!(
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
        if compare(&mut db, writer.app(), &taxi_oracles(2)).is_some() {
            compared += 1;
        }
    }
    assert!(compared > 100, "compared only {compared} times");
}

/// Naive times and a dim keyed by two columns, one of them a by column.
#[test]
fn follows_the_task_tables() {
    let pg = postgres();
    let mut db = Db::new(&pg, "task_tables", TASKS);
    // The comparison session is far from UTC, so a value that went through
    // a zone on either side would show.
    db.run("SET TIME ZONE 'Europe/Oslo'");
    let mut rng = Rng::new(17);
    for organization in 1..=3 {
        for taskgroup in 1..=2 {
            db.run(&format!(
                "INSERT INTO $S.taskgroups VALUES ({organization}, {taskgroup}, '{}')",
                rng.pick(&["pick", "pack", "count"])
            ));
        }
    }
    for id in 0..200 {
        db.run(&rng.task(id));
    }
    // On a bucket edge, a microsecond before it, and at the infinities,
    // which no bucket holds.
    db.run(&format!(
        "INSERT INTO $S.tasks VALUES
           (1000, 1, 1, to_timestamp({START}) AT TIME ZONE 'UTC', NULL, 1),
           (1001, 1, 1, to_timestamp({START}) AT TIME ZONE 'UTC' - interval '1 microsecond', NULL, 1),
           (1002, 1, 1, 'infinity', NULL, 1),
           (1003, 2, 2, '-infinity', NULL, 2)"
    ));
    let spec = db.spec(TASKS_SPEC);
    assert!(crate::plan(&spec, "p", "r", None).contains(
        "-- dim taskgroups: Straum holds its key and the columns the views read, and follows it."
    ));
    let mut writer = Writer::new(App::new(spec, LIMITS, None), db.pg());
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    compare(&mut db, writer.app(), &task_oracles()).expect("ready after the load");
    let edges = Ask {
        from: START - 3_600,
        to: START + 3_600,
        ..ask("tasks", &["tasks"], &["bucket"])
    };
    let answered = writer.app().ask(&edges).unwrap();
    assert_eq!(answered.answer.rows.len(), 2);
    assert_eq!(
        answered.answer.rows[0],
        AnswerRow {
            keys: vec![Datum::Int(START - 3_600)],
            values: vec![Value::Int(1)]
        }
    );
    assert_eq!(answered.answer.rows[1].keys, [Datum::Int(START)]);

    for id in 200..230 {
        db.run(&rng.task(id));
        applied(&mut writer);
    }
    // Each change is one commit, compared after it: facts moving to
    // another dim row or to a group with no row, times moved, rows gone.
    let changes = [
        // Null-ness flips both ways.
        "UPDATE $S.tasks SET completed = created + interval '1 hour' WHERE task_id % 3 = 0"
            .to_owned(),
        "UPDATE $S.tasks SET completed = NULL WHERE task_id % 6 = 0".to_owned(),
        "UPDATE $S.tasks SET taskgroup_id = 2 WHERE task_id % 5 = 0".to_owned(),
        "UPDATE $S.tasks SET taskgroup_id = 4 WHERE task_id % 17 = 0".to_owned(),
        // The enum key moves, in the fact and through the dim.
        "UPDATE $S.tasks SET kind = 'shelf' WHERE task_id % 4 = 0".to_owned(),
        "UPDATE $S.tasks SET organization_id = 3 WHERE task_id % 13 = 0".to_owned(),
        "UPDATE $S.tasks SET created = created + interval '90 minutes' WHERE task_id % 7 = 0"
            .to_owned(),
        // An infinite time becomes a real one and lands in its bucket; a
        // real one becomes infinite and leaves its bucket; one goes.
        format!(
            "UPDATE $S.tasks SET created = to_timestamp({START}) AT TIME ZONE 'UTC' \
             + interval '2 hours' WHERE task_id = 1002"
        ),
        "UPDATE $S.tasks SET created = 'infinity' WHERE task_id = 1000".to_owned(),
        "DELETE FROM $S.tasks WHERE task_id = 1003".to_owned(),
        "DELETE FROM $S.tasks WHERE task_id % 11 = 0".to_owned(),
        // The dim moves: a group recategorised, one moved to a new key,
        // one gone, one back.
        "UPDATE $S.taskgroups SET category = 'other' WHERE organization_id = 1".to_owned(),
        "UPDATE $S.taskgroups SET taskgroup_id = 3 WHERE organization_id = 2 AND taskgroup_id = 1"
            .to_owned(),
        "DELETE FROM $S.taskgroups WHERE organization_id = 3 AND taskgroup_id = 2".to_owned(),
        "INSERT INTO $S.taskgroups VALUES (3, 2, 'count')".to_owned(),
    ];
    for change in changes {
        db.run(&change);
        applied(&mut writer);
        compare(&mut db, writer.app(), &task_oracles()).unwrap_or_else(|| panic!("{change}"));
    }
    // The key is a boolean, whatever its column is.
    let done_is = |value| Ask {
        filter: vec![Condition {
            key: id("done"),
            values: vec![value],
        }],
        ..ask("tasks", &["tasks"], &[])
    };
    assert!(writer.app().ask(&done_is(Literal::Bool(true))).is_ok());
    assert_eq!(
        writer
            .app()
            .ask(&done_is(Literal::Int(1)))
            .unwrap_err()
            .to_string(),
        "done is a boolean, so it can't be the integer 1"
    );
}

/// A transaction at the limit applies; one past it starts Straum over, and
/// the new load holds its rows.
#[test]
fn a_transaction_past_the_limit_starts_over() {
    const CAP: usize = 50;
    let pg = postgres();
    let mut db = Db::new(&pg, "capped", TABLES);
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5), (2, 'Bronx', NULL)");
    let mut rng = Rng::new(23);
    for id in 0..20 {
        db.run(&rng.trip(id));
    }
    let mut writer = Writer::new(
        App::new(db.spec(SPEC), LIMITS, None),
        Postgres::new(db.dsn.clone(), &db.name, CAP),
    );
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    let batch = |rng: &mut Rng, from: u64, rows: usize| {
        let inserts: Vec<String> = (from..from + rows as u64).map(|id| rng.trip(id)).collect();
        format!("BEGIN;\n{}\nCOMMIT;", inserts.join("\n"))
    };
    db.run(&batch(&mut rng, 100, CAP));
    applied(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");

    db.run(&batch(&mut rng, 200, CAP + 1));
    let err = writer.step().unwrap_err().to_string();
    let lsn = db.rows("SELECT pg_current_wal_lsn()")[0][0]
        .clone()
        .unwrap();
    assert_eq!(
        err,
        format!(
            "transaction {} changes more than {CAP} rows Straum reads, the --max-transaction-rows \
             limit; Straum starts over from a snapshot after it",
            err.split(' ').nth(1).unwrap_or_default()
        ),
        "{err}"
    );
    // The transaction's commit record ends at or before the server's
    // position now, and the failure is what /status shows.
    let named = err.split(' ').nth(1).unwrap();
    assert!(
        parse_lsn(named).unwrap() <= parse_lsn(&lsn).unwrap(),
        "transaction {named} is past the server at {lsn}"
    );
    assert_eq!(writer.app().status().failure.as_deref(), Some(err.as_str()));
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready after the new load");
    db.run(&rng.trip(300));
    applied(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");
}

/// The same tables under two views: a dim change is a change to the dim's
/// projection, and every ask reads the dim as it is now, as PostgreSQL's
/// view does. Nothing starts over.
#[test]
fn a_dim_change_shows_at_once() {
    let pg = postgres();
    let mut db = Db::new(&pg, "ingest", TASKS);
    db.run("SET TIME ZONE 'Europe/Oslo'");
    let mut rng = Rng::new(29);
    for organization in 1..=3 {
        for taskgroup in 1..=2 {
            db.run(&format!(
                "INSERT INTO $S.taskgroups VALUES ({organization}, {taskgroup}, '{}')",
                rng.pick(&["pick", "pack", "count"])
            ));
        }
    }
    for id in 0..200 {
        db.run(&rng.task(id));
    }
    let mut picks = Writer::new(App::new(db.spec(PICKS_SPEC), LIMITS, None), db.pg());
    let mut tasks = Writer::new(App::new(db.spec(TASKS_SPEC), LIMITS, None), db.pg());
    assert!(matches!(picks.step(), Ok(Step::Started(_))));
    assert!(matches!(tasks.step(), Ok(Step::Started(_))));
    compare(&mut db, picks.app(), &picks_oracles()).expect("picks ready");
    compare(&mut db, tasks.app(), &task_oracles()).expect("tasks ready");

    // Fact rows come, move between groups, flip and go; tasks in groups
    // 3 and 4 have no dim row and read null. Then the dim moves: a group
    // recategorised with a new task in the same transaction, a key moved
    // and a row gone each with a task pointing at them, the table emptied
    // and filled again.
    let mut changes: Vec<String> = (200..210).map(|id| rng.task(id)).collect();
    changes.extend([
        "UPDATE $S.tasks SET taskgroup_id = 2 WHERE task_id % 5 = 0".to_owned(),
        "UPDATE $S.tasks SET completed = NULL WHERE task_id % 4 = 0".to_owned(),
        "DELETE FROM $S.tasks WHERE task_id % 11 = 0".to_owned(),
        format!(
            "BEGIN;
             UPDATE $S.taskgroups SET category = 'other' WHERE organization_id = 1;
             INSERT INTO $S.tasks VALUES (300, 1, 1, to_timestamp({START}) AT TIME ZONE 'UTC', NULL, 5);
             COMMIT;"
        ),
        "UPDATE $S.tasks SET picks = coalesce(picks, 0) + 1
          WHERE task_id = (SELECT min(task_id) FROM $S.tasks
                            WHERE organization_id = 1 AND taskgroup_id = 1 AND task_id < 300)"
            .to_owned(),
        format!(
            "BEGIN;
             UPDATE $S.taskgroups SET taskgroup_id = 3 WHERE organization_id = 2 AND taskgroup_id = 1;
             INSERT INTO $S.tasks VALUES (401, 2, 1, to_timestamp({START}) AT TIME ZONE 'UTC', NULL, 1);
             COMMIT;"
        ),
        format!(
            "BEGIN;
             DELETE FROM $S.taskgroups WHERE organization_id = 2 AND taskgroup_id = 3;
             INSERT INTO $S.tasks VALUES (402, 2, 1, to_timestamp({START}) AT TIME ZONE 'UTC', NULL, 1);
             COMMIT;"
        ),
        "TRUNCATE $S.taskgroups".to_owned(),
        "INSERT INTO $S.taskgroups VALUES (1, 1, 'pick'), (2, 2, 'pack'), (3, 1, 'count')"
            .to_owned(),
    ]);
    for change in changes {
        db.run(&change);
        applied(&mut picks);
        applied(&mut tasks);
        compare(&mut db, picks.app(), &picks_oracles()).unwrap_or_else(|| panic!("{change}"));
        compare(&mut db, tasks.app(), &task_oracles()).unwrap_or_else(|| panic!("{change}"));
    }
    assert_eq!(picks.app().status().starts, 1);
    assert_eq!(tasks.app().status().starts, 1);

    // Tasks whose group is gone read null, as the join gives.
    let null_tasks = Ask {
        filter: vec![Condition {
            key: id("organization_id"),
            values: vec![Literal::Int(2)],
        }],
        from: START,
        to: START + 3_600,
        ..ask("picks", &["tasks"], &["category"])
    };
    let answered = picks.app().ask(&null_tasks).unwrap();
    let nulls = (answered.answer.rows.iter())
        .find(|row| row.keys[0] == Datum::Null)
        .map(|row| row.values[0].clone());
    assert!(matches!(nulls, Some(Value::Int(n)) if n >= 2), "{nulls:?}");
}

/// A value Straum can't read stops the load, or the stream, naming the
/// table and column it was in, so the row can be found and fixed.
/// A shop with every join kind at once: orders joined to customers, which
/// may hold a key twice and lack a key, through them to tiers, kept by a
/// promo on their sku and dropped by a block on their customer, under a
/// standing `WHERE`; and a view of the same orders keeping every row.
const SHOP: &str = "
CREATE TABLE $S.customers (id int, country text, tier_id int);
CREATE TABLE $S.tiers (id int, label text);
CREATE TABLE $S.promos (sku int, note text);
CREATE TABLE $S.blocked (id int, reason text);
CREATE TABLE $S.orders (
    id int PRIMARY KEY,
    at timestamptz,
    customer_id int,
    sku int,
    qty int,
    status text,
    gift boolean
);
ALTER TABLE $S.customers REPLICA IDENTITY FULL;
ALTER TABLE $S.tiers REPLICA IDENTITY FULL;
ALTER TABLE $S.promos REPLICA IDENTITY FULL;
ALTER TABLE $S.blocked REPLICA IDENTITY FULL;
ALTER TABLE $S.orders REPLICA IDENTITY FULL;
CREATE PUBLICATION $S FOR TABLE $S.customers, $S.tiers, $S.promos, $S.blocked, $S.orders;
";

const PAID_SPEC: &str = "
CREATE VIEW paid WITH (buckets = '1 hour, 1 day') AS
SELECT date_bin('1 hour', o.at, timestamptz 'epoch') AS bucket,
       o.status AS status,
       c.country AS country,
       t.label AS label,
       count(*) AS orders,
       sum(o.qty) AS qty,
       max(o.qty) AS qty_max,
       avg(o.qty) AS qty_avg,
       count(DISTINCT o.qty) AS sizes,
       count(*) FILTER (WHERE c.country = 'NO') AS norwegian,
       bool_and(o.gift) AS all_gifts,
       bool_or(o.gift) AS any_gift
FROM $S.orders AS o
INNER JOIN $S.customers AS c ON c.id = o.customer_id
LEFT OUTER JOIN $S.tiers AS t ON t.id = c.tier_id
WHERE EXISTS (SELECT 1 FROM $S.promos AS p WHERE p.sku = o.sku)
  AND NOT EXISTS (SELECT 1 FROM $S.blocked AS b WHERE b.id = o.customer_id)
  AND o.status IN ('paid', 'shipped')
  AND c.country IN ('NO', 'SE', 'DK')
GROUP BY 1, 2, 3, 4;
";

const OFFERS_SPEC: &str = "
CREATE VIEW offers WITH (buckets = '1 hour, 1 day') AS
SELECT date_bin('1 hour', o.at, timestamptz 'epoch') AS bucket,
       c.country AS country,
       p.note AS note,
       count(*) AS orders,
       sum(o.qty) AS qty,
       min(o.qty) AS qty_min,
       bool_or(o.gift) FILTER (WHERE c.country = 'NO') AS norwegian_gift
FROM $S.orders AS o
LEFT OUTER JOIN $S.customers AS c ON c.id = o.customer_id
LEFT OUTER JOIN $S.promos AS p ON p.sku = o.sku
GROUP BY 1, 2, 3;
";

/// The shop specs' references, the paid view's then the offers view's: each
/// by bucket, and the paid view by country and label over a window that
/// cuts the day bucket `START` begins, against the raw rows joined as the
/// view says.
fn shop_oracles() -> (Vec<Reference>, Vec<Reference>) {
    const PAID: [&str; 8] = [
        "orders",
        "qty",
        "qty_max",
        "qty_avg",
        "sizes",
        "norwegian",
        "all_gifts",
        "any_gift",
    ];
    let paid = Reference {
        sql: format!(
            "SELECT extract(epoch FROM bucket)::int8, status, country, label, orders, qty,
                    qty_max, round(qty_avg, 6), sizes, norwegian, all_gifts, any_gift
               FROM ({}) v",
            body(PAID_SPEC, "paid")
        ),
        ask: ask("paid", &PAID, &["bucket", "status", "country", "label"]),
        scales: vec![0; 12],
    };
    let offers = Reference {
        sql: format!(
            "SELECT extract(epoch FROM bucket)::int8, country, note, orders, qty, qty_min,
                    norwegian_gift
               FROM ({}) v",
            body(OFFERS_SPEC, "offers")
        ),
        ask: ask(
            "offers",
            &["orders", "qty", "qty_min", "norwegian_gift"],
            &["bucket", "country", "note"],
        ),
        scales: vec![0; 7],
    };
    let window = Reference {
        sql: format!(
            "SELECT c.country, t.label, count(*), sum(o.qty), max(o.qty), round(avg(o.qty), 6),
                    count(DISTINCT o.qty), count(*) FILTER (WHERE c.country = 'NO'),
                    bool_and(o.gift), bool_or(o.gift)
               FROM $S.orders o
               JOIN $S.customers c ON c.id = o.customer_id
               LEFT JOIN $S.tiers t ON t.id = c.tier_id
              WHERE EXISTS (SELECT 1 FROM $S.promos p WHERE p.sku = o.sku)
                AND NOT EXISTS (SELECT 1 FROM $S.blocked b WHERE b.id = o.customer_id)
                AND o.status IN ('paid', 'shipped')
                AND c.country IN ('NO', 'SE', 'DK')
                AND o.at >= to_timestamp({WINDOW_FROM}) AND o.at < to_timestamp({WINDOW_TO})
              GROUP BY 1, 2"
        ),
        ask: Ask {
            from: WINDOW_FROM,
            to: WINDOW_TO,
            ..ask("paid", &PAID, &["country", "label"])
        },
        scales: vec![0; 10],
    };
    (vec![paid, window], vec![offers])
}

/// Every join kind answers as PostgreSQL's view does, through dim rows
/// that come, move, double and go.
#[test]
fn every_join_kind_matches_postgresql() {
    let pg = postgres();
    let mut db = Db::new(&pg, "shop", SHOP);
    let mut rng = Rng::new(31);
    // Customer 7 is held twice, 8 not at all; tier 4 and sku 5 have no
    // row; customer 3 is blocked.
    db.run(
        "INSERT INTO $S.customers VALUES
           (1, 'NO', 1), (2, 'SE', 2), (3, 'NO', 3), (4, 'DK', 4), (5, 'FI', 1),
           (6, NULL, 2), (7, 'NO', 1), (7, 'SE', 3);
         INSERT INTO $S.tiers VALUES (1, 'bronze'), (2, 'silver'), (3, 'gold');
         INSERT INTO $S.promos VALUES (1, 'two for one'), (1, 'free shipping'), (2, 'sale'),
                                      (3, NULL), (4, 'sale');
         INSERT INTO $S.blocked VALUES (3, 'fraud')",
    );
    let order = |id: u64, rng: &mut Rng| {
        let at = START + rng.below(rng.hours * 3_600) as i64;
        let customer = (1 + rng.below(8)).to_string();
        let sku = (1 + rng.below(5)).to_string();
        let qty = rng.below(10).to_string();
        let status = format!("'{}'", rng.pick(&["paid", "shipped", "new", "cancelled"]));
        let gift = rng.pick(&["true", "false"]).to_owned();
        format!(
            "INSERT INTO $S.orders VALUES ({id}, to_timestamp({at}), {}, {}, {}, {}, {})",
            rng.or_null(customer),
            rng.or_null(sku),
            rng.or_null(qty),
            rng.or_null(status),
            rng.or_null(gift)
        )
    };
    for id in 0..300 {
        let sql = order(id, &mut rng);
        db.run(&sql);
    }
    let mut paid = Writer::new(App::new(db.spec(PAID_SPEC), LIMITS, None), db.pg());
    let mut offers = Writer::new(App::new(db.spec(OFFERS_SPEC), LIMITS, None), db.pg());
    assert!(matches!(paid.step(), Ok(Step::Started(_))));
    assert!(matches!(offers.step(), Ok(Step::Started(_))));
    let (paid_oracles, offers_oracles) = shop_oracles();
    compare(&mut db, paid.app(), &paid_oracles).expect("ready");
    compare(&mut db, offers.app(), &offers_oracles).expect("ready");

    let mut changes: Vec<String> = (300..320).map(|id| order(id, &mut rng)).collect();
    changes.extend(
        [
            // A dim row moves, under an inner join and a standing WHERE.
            "UPDATE $S.customers SET country = 'SE' WHERE id = 1",
            // A key held once is held twice, then once again.
            "INSERT INTO $S.customers VALUES (2, 'DK', 3)",
            "DELETE FROM $S.customers WHERE id = 7 AND country = 'SE'",
            // The chain: the far dim changes, then the near dim's link.
            "UPDATE $S.tiers SET label = 'platinum' WHERE id = 3",
            "UPDATE $S.customers SET tier_id = 2 WHERE id = 4",
            "DELETE FROM $S.tiers WHERE id = 1",
            // Semi and anti: a promo for the sku without one, a doubled
            // promo gone whole, a customer blocked and one unblocked.
            "INSERT INTO $S.promos VALUES (5, 'late')",
            "DELETE FROM $S.promos WHERE sku = 1",
            "INSERT INTO $S.blocked VALUES (1, 'chargeback')",
            "DELETE FROM $S.blocked WHERE id = 3",
            // Fact rows move across the WHERE, the joins and out.
            "UPDATE $S.orders SET status = 'paid' WHERE id % 3 = 0",
            // The last false of a cell goes, and a true comes to be null.
            "UPDATE $S.orders SET gift = true WHERE id % 2 = 0 AND NOT gift",
            "UPDATE $S.orders SET gift = NULL WHERE id % 5 = 0 AND gift",
            "UPDATE $S.orders SET customer_id = 8 WHERE id % 7 = 0",
            "UPDATE $S.orders SET sku = 1 WHERE id % 5 = 0",
            "DELETE FROM $S.orders WHERE id % 11 = 0",
            // A dim key moves, a dim is emptied and filled again.
            "UPDATE $S.customers SET id = 8 WHERE id = 5",
            "TRUNCATE $S.tiers",
            "INSERT INTO $S.tiers VALUES (1, 'bronze'), (2, 'silver'), (2, 'silver again')",
        ]
        .map(str::to_owned),
    );
    for change in changes {
        db.run(&change);
        applied(&mut paid);
        applied(&mut offers);
        compare(&mut db, paid.app(), &paid_oracles).unwrap_or_else(|| panic!("{change}"));
        compare(&mut db, offers.app(), &offers_oracles).unwrap_or_else(|| panic!("{change}"));
    }
    assert_eq!(paid.app().status().starts, 1);
    assert_eq!(offers.app().status().starts, 1);
}

#[test]
fn a_poison_value_names_its_column() {
    let pg = postgres();
    let mut db = Db::new(&pg, "poison", TABLES);
    db.run("INSERT INTO $S.trips VALUES (1, to_timestamp(1705276800), 'cab', 1, 'NaN', 'ann', 2)");
    let mut writer = db.writer();
    let err = writer.step().unwrap_err().to_string();
    assert!(
        err.starts_with("can't read \"\\\\x")
            && err.ends_with("as a numeric of scale 2 in column fare of poison.trips"),
        "{err}"
    );
    assert_eq!(writer.app().status().failure.as_deref(), Some(err.as_str()));
    db.run("UPDATE $S.trips SET fare = 1.50 WHERE id = 1");
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");

    // Streamed, the value comes as text.
    db.run("UPDATE $S.trips SET fare = 'NaN' WHERE id = 1");
    let err = writer.step().unwrap_err().to_string();
    assert_eq!(
        err,
        "can't read \"NaN\" as a numeric of scale 2 in column fare of poison.trips"
    );
    db.run("UPDATE $S.trips SET fare = 2.50 WHERE id = 1");
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready after the fix");
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
                let mut conn = Connection::connect(&dsn, false).unwrap();
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
        assert_ne!(
            loaded.answer.rows[0].values[0],
            Value::Int(0),
            "round {round}"
        );
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
                .any(|row| row.keys[0] == Datum::Text("marker".into()))
            {
                break;
            }
        }
        assert!(streamed > 1, "round {round}: nothing was streamed");
        // The marker's commit time, by the server's clock, is now.
        let time = writer.app().status().committed.unwrap();
        let apart = match SystemTime::now().duration_since(time) {
            Ok(behind) => behind,
            Err(ahead) => ahead.duration(),
        };
        assert!(apart < Duration::from_secs(60), "round {round}: {apart:?}");
        compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");
    }
}

#[test]
fn an_unfit_change_starts_over() {
    let pg = postgres();
    let mut db = Db::new(&pg, "unfit", TABLES);
    db.run("INSERT INTO $S.logins VALUES (1, to_timestamp(1705276800), 1)");
    let mut writer = db.writer();
    writer.step().unwrap();
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");

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
        err.contains("fact logins: Straum needs the whole old row of every update and delete"),
        "{err}"
    );
    db.run("ALTER TABLE $S.logins REPLICA IDENTITY FULL");
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready after the new load");
    db.run("INSERT INTO $S.logins VALUES (2, to_timestamp(1705276800), 3)");
    applied(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");

    // A dim is held too, so it owes the same.
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5)");
    applied(&mut writer);
    db.run("ALTER TABLE $S.zones REPLICA IDENTITY DEFAULT");
    db.run("UPDATE $S.zones SET borough = 'Staten Island' WHERE id = 1");
    let err = writer.step().unwrap_err().to_string();
    assert_eq!(err, "a change no longer fits zones");
    let err = writer.step().unwrap_err().to_string();
    assert!(
        err.contains("dim zones: Straum needs the whole old row of every update and delete"),
        "{err}"
    );
    db.run("ALTER TABLE $S.zones REPLICA IDENTITY FULL");
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready after the dim's new load");
}

#[test]
fn asks_take_values_of_the_types_the_load_read() {
    let pg = postgres();
    let db = Db::new(&pg, "types", TABLES);
    let mut writer = db.writer();
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    let filtered = |fact, measure, key: &str, value| Ask {
        filter: vec![Condition {
            key: match ask(fact, &[], &[key]).keys.remove(0) {
                AskKey::Key(name) => name,
                AskKey::Time(_) => unreachable!("cases filter on keys"),
            },
            values: vec![value],
        }],
        ..ask(fact, &[measure], &[])
    };
    let text = |s: &str| Literal::Text(s.into());
    // text, the column a ref joins on (int), a dim's text; then
    // numeric(4, 1).
    let cases = [
        (
            "trips",
            "trips",
            "kind",
            text("cab"),
            Literal::Int(1),
            ColumnType::Text,
        ),
        (
            "trips",
            "trips",
            "zone_id",
            Literal::Int(1),
            text("1"),
            ColumnType::Int,
        ),
        (
            "trips",
            "trips",
            "borough",
            text("Bronx"),
            Literal::Bool(true),
            ColumnType::Text,
        ),
    ];
    for (fact, measure, key, right, wrong, column) in cases {
        let app = writer.app();
        let answered = app.ask(&filtered(fact, measure, key, right));
        assert!(answered.is_ok(), "{key}: {answered:?}");
        let refused = app.ask(&filtered(fact, measure, key, wrong.clone()));
        let Err(AnswerError::Invalid(errors)) = refused else {
            panic!("{key}: {refused:?}");
        };
        assert_eq!(errors.len(), 1, "{key}");
        assert!(
            matches!(&errors[0], AskError::WrongType { column: c, value, .. } if *c == column && *value == wrong),
            "{key}: {errors:?}"
        );
    }
    // A numeric of scale 1 takes no integer: 1 would read as 0.1.
    let toll = filtered("logins", "logins", "toll", Literal::Int(1));
    assert_eq!(
        writer.app().ask(&toll).unwrap_err().to_string(),
        "toll is a numeric of scale 1, so it can't be the integer 1"
    );
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
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready after the load");
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
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");
    let tls = db.rows(
        "SELECT DISTINCT a.backend_type, s.ssl FROM pg_stat_activity a
         JOIN pg_stat_ssl s USING (pid) WHERE a.application_name = 'straum' ORDER BY 1",
    );
    let row = |kind: &str| vec![Some(kind.to_owned()), Some("t".to_owned())];
    assert_eq!(tls, [row("client backend"), row("walsender")]);
}

#[test]
fn follows_at_replica_from_19() {
    let pg = postgres_at_replica(NINETEEN);
    let mut db = Db::new(&pg, "replica", TABLES);
    let mut rng = Rng::new(13);
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5), (2, 'Bronx', NULL)");
    for id in 0..20 {
        db.run(&rng.trip(id));
    }
    let mut writer = db.writer();
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready after the load");
    for id in 20..30 {
        db.run(&rng.trip(id));
        applied(&mut writer);
    }
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");
}

#[test]
fn says_to_set_wal_level_logical() {
    for tag in ["17", "18"] {
        let pg = postgres_at_replica(tag);
        let db = Db::new(&pg, "replica", TABLES);
        let err = db.pg().open(&db.spec(SPEC)).unwrap_err().to_string();
        assert!(
            err.contains("wal_level is replica; Straum needs logical, which takes a restart"),
            "{tag}: {err}"
        );
    }
}

#[test]
fn tls_trusts_only_its_roots() {
    let pg = postgres_tls();
    // Display leaves the password out.
    let dsn = (pg.dsn.to_string()).replace("postgres@", &format!("postgres:{PASSWORD}@"));
    let ssl = |dsn: &str| {
        let mut conn = Connection::connect(&Dsn::parse(dsn).unwrap(), false)?;
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
        let err = Connection::connect(&dsn, false).err().unwrap().to_string();
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
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");

    db.run("ALTER TABLE $S.trips ALTER COLUMN fare TYPE numeric(12, 3)");
    db.run("INSERT INTO $S.trips VALUES (2, to_timestamp(1705276800), 'cab', 1, 2.125, 'bo', 3)");
    let err = writer.step().unwrap_err().to_string();
    assert_eq!(err, "a change no longer fits trips");
    writer.step().unwrap();
    db.run(&Rng::new(2).trip(3));
    applied(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(3)).expect("ready");

    // A column Straum reads is gone: Straum stops and the setup check says why.
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

/// Too big and too random to stay inline: PostgreSQL TOASTs the rider, and
/// an update that leaves it alone doesn't send it. The old row, whole
/// under `REPLICA IDENTITY FULL`, supplies it.
#[test]
fn a_value_left_out_comes_from_the_old_row() {
    let pg = postgres();
    let mut db = Db::new(&pg, "toast", TABLES);
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5)");
    db.run(
        "INSERT INTO $S.trips
         SELECT 1, to_timestamp(1705276800), 'cab', 1, 1.00,
                string_agg(md5(random()::text), ''), 2
           FROM generate_series(1, 2000)",
    );
    let mut writer = db.writer();
    writer.step().unwrap();
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");

    db.run("UPDATE $S.trips SET tip = 3 WHERE id = 1");
    applied(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready after the update");
    db.run("DELETE FROM $S.trips WHERE id = 1");
    applied(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready after the delete");
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
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");
}

#[test]
fn plan_sets_up_what_straum_needs() {
    let pg = postgres();
    let mut db = Db::new(&pg, "plan", &TABLES.replace("CREATE PUBLICATION", "-- "));
    db.run("ALTER TABLE $S.trips REPLICA IDENTITY DEFAULT");
    let spec = db.spec(SPEC);
    let sql = crate::plan(&spec, "plan", "straum_plan", None);
    assert_eq!(
        sql,
        "-- Straum setup: run once, as an owner of the tables.
-- wal_level takes a restart. On Azure, set the server parameter instead.
ALTER SYSTEM SET wal_level = logical;
CREATE ROLE \"straum_plan\" LOGIN REPLICATION;
-- Then give \"straum_plan\" a password: \\password \"straum_plan\"
GRANT USAGE ON SCHEMA \"plan\" TO \"straum_plan\";
GRANT SELECT ON \"plan\".\"zones\", \"plan\".\"trips\", \"plan\".\"logins\" TO \"straum_plan\";
CREATE PUBLICATION \"plan\" FOR TABLE \"plan\".\"zones\", \"plan\".\"trips\", \"plan\".\"logins\";
-- fact trips: Straum needs the old row of every update and delete; inserts cost no more.
ALTER TABLE \"plan\".\"trips\" REPLICA IDENTITY FULL;
-- fact logins: Straum needs the old row of every update and delete; inserts cost no more.
ALTER TABLE \"plan\".\"logins\" REPLICA IDENTITY FULL;
-- dim zones: Straum holds its key and the columns the views read, and follows it.
ALTER TABLE \"plan\".\"zones\" REPLICA IDENTITY FULL;
"
    );
    // One statement at a time: ALTER SYSTEM can't run in a transaction.
    for statement in sql.lines().filter(|l| !l.starts_with("--")) {
        db.run(statement);
    }
    db.run("ALTER ROLE straum_plan PASSWORD 'plan'");
    let dsn = pg.dsn.to_string().replace("postgres@", "straum_plan:plan@");
    let pg = Postgres::new(Dsn::parse(&dsn).unwrap(), "plan", ROW_LIMIT);
    let mut writer = Writer::new(App::new(spec, LIMITS, None), pg);
    writer.step().unwrap();
    db.run(&Rng::new(1).trip(1));
    applied(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");
}

/// The setup problems an open stopped at; none when it opened.
fn setup_problems(name: &str, opened: Result<Lsn, SourceError>) -> Vec<String> {
    match opened {
        Ok(_) => Vec::new(),
        Err(e) => match e.downcast::<Error>().map(|e| *e) {
            Ok(Error::Setup(problems)) => problems,
            other => panic!("{name}: {other:?}"),
        },
    }
}

/// Opens `spec` against `tables` and checks the setup problems found;
/// `None` expects none.
fn expect_problems(
    pg: &PostgresContainer,
    name: &str,
    tables: &str,
    spec: &str,
    expected: Option<&str>,
) {
    let db = Db::new(pg, name, tables);
    let found = setup_problems(name, db.pg().open(&db.spec(spec)));
    assert_eq!(found, Vec::from_iter(expected), "{name}");
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
                "CREATE VIEW f WITH (buckets = '1 hour') AS
                 SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket, {view}
                 FROM $S.f AS t GROUP BY 1;"
            ),
        )
    };
    let publish = "CREATE PUBLICATION $S FOR TABLE $S.f;";
    let enum_type = "CREATE TYPE $S.mood AS ENUM ('ok', 'bad');";
    let (table, spec) = fact("x int", "count(*) AS n, sum(t.x) AS total");
    let cases: Vec<(&str, String, String, Option<&str>)> = vec![
        ("ok", format!("{table} {publish}"), spec.clone(), None),
        (
            "nopub",
            table.clone(),
            spec.clone(),
            Some("publication nopub does not exist; run the SQL straum plan prints"),
        ),
        (
            "notrunc",
            format!(
                "{table} CREATE PUBLICATION $S FOR TABLE $S.f WITH (publish = 'insert, update, delete');"
            ),
            spec.clone(),
            Some("publication notrunc must publish insert, update, delete and truncate"),
        ),
        (
            "missing",
            "CREATE PUBLICATION $S;".into(),
            spec.clone(),
            Some("f: table missing.f does not exist"),
        ),
        (
            "unpub",
            format!("{table} CREATE TABLE $S.g (); CREATE PUBLICATION $S FOR TABLE $S.g;"),
            spec.clone(),
            Some(
                "f: publication unpub leaves out unpub.f: ALTER PUBLICATION unpub ADD TABLE unpub.f",
            ),
        ),
        (
            "partitioned",
            "CREATE TABLE $S.f (id int, at timestamptz, x int) PARTITION BY RANGE (id);
             CREATE PUBLICATION $S FOR TABLE $S.f;"
                .into(),
            spec.clone(),
            Some("f: partitioned.f is not a plain table"),
        ),
        (
            "filtered",
            format!("{table} CREATE PUBLICATION $S FOR TABLE $S.f WHERE (x > 0);"),
            spec.clone(),
            Some("f: publication filtered filters the rows of filtered.f; Straum needs every row"),
        ),
        (
            "listed",
            format!("{table} CREATE PUBLICATION $S FOR TABLE $S.f (id, at);"),
            spec.clone(),
            Some("f: publication listed leaves out column x of listed.f"),
        ),
        (
            "nocolumn",
            format!("CREATE TABLE $S.f (id int PRIMARY KEY, at timestamptz); {publish}"),
            spec.clone(),
            Some("f: nocolumn.f has no column x"),
        ),
        (
            "nokey",
            format!(
                "CREATE TABLE $S.f (at timestamptz, x int); ALTER TABLE $S.f REPLICA IDENTITY FULL; {publish}"
            ),
            spec.clone(),
            None,
        ),
        (
            "float",
            format!("{} {publish}", fact("x float8", "").0),
            spec.clone(),
            Some(
                "f: column x of float.f is double precision; \
             Straum needs an integer type or numeric(p, s) with p at most 18 there",
            ),
        ),
        (
            "naive",
            format!("CREATE TABLE $S.f (id int PRIMARY KEY, at timestamp, x int); {publish}"),
            spec.clone(),
            Some(
                "f: column at of naive.f is timestamp without time zone; the date_bin origin \
             timestamptz 'epoch' needs a timestamptz there; for a timestamp column, \
             write timestamp 'epoch'",
            ),
        ),
        (
            "zoned",
            format!("{table} {publish}"),
            spec.replace("timestamptz 'epoch'", "timestamp 'epoch'"),
            Some(
                "f: column at of zoned.f is timestamp with time zone; the date_bin origin \
             timestamp 'epoch' needs a timestamp there; for a timestamptz column, \
             write timestamptz 'epoch'",
            ),
        ),
        (
            "notime",
            format!("{} {publish}", fact("x int, k timestamptz", "").0),
            spec.replace("t.at", "t.x"),
            Some(
                "f: column x of notime.f is integer; the date_bin origin timestamptz 'epoch' \
             needs a timestamptz there; for a timestamp column, write timestamp 'epoch'",
            ),
        ),
        (
            "timekey",
            format!("{} {publish}", fact("x int, k timestamptz", "").0),
            fact("", "t.k AS k, count(*) AS n")
                .1
                .replace("GROUP BY 1", "GROUP BY 1, 2"),
            Some(
                "f: column k of timekey.f is timestamp with time zone; \
             Straum needs an integer type, numeric(p, s), boolean or text there",
            ),
        ),
        (
            "naivekey",
            format!("{} {publish}", fact("x int, k timestamp", "").0),
            fact("", "t.k AS k, count(*) AS n")
                .1
                .replace("GROUP BY 1", "GROUP BY 1, 2"),
            Some(
                "f: column k of naivekey.f is timestamp without time zone; \
             Straum needs an integer type, numeric(p, s), boolean or text there",
            ),
        ),
        (
            "counted",
            format!("{} {publish}", fact("x int, k timestamptz", "").0),
            fact("", "count(t.k) AS n").1,
            None,
        ),
        (
            "uncounted",
            format!("{} {publish}", fact("x int, k float8", "").0),
            fact("", "count(t.k) AS n").1,
            Some(
                "f: column k of uncounted.f is double precision; straum needs a type it reads \
             there: an integer type, numeric(p, s), boolean, text, timestamptz or timestamp",
            ),
        ),
        (
            "boolean",
            format!("{} {publish}", fact("x int, k boolean", "").0),
            fact("", "bool_and(t.k) AS every, bool_or(t.k) AS some").1,
            None,
        ),
        (
            "notboolean",
            format!("{} {publish}", fact("x int, k int", "").0),
            fact("", "bool_or(t.k) AS some").1,
            Some("f: column k of notboolean.f is integer; Straum needs boolean there"),
        ),
        (
            "identity",
            format!("CREATE TABLE $S.f (id int PRIMARY KEY, at timestamptz, x int); {publish}"),
            spec.clone(),
            Some(
                "fact f: Straum needs the whole old row of every update and delete: \
             ALTER TABLE identity.f REPLICA IDENTITY FULL",
            ),
        ),
        (
            "filter",
            format!("{} {publish}", fact("k text", "").0),
            fact("", "t.k AS k, count(*) FILTER (WHERE t.k = 1) AS n")
                .1
                .replace("GROUP BY 1", "GROUP BY 1, 2"),
            Some("view f: measure n compares k (text) with 1"),
        ),
        (
            "filtertext",
            format!("{} {publish}", fact("k int", "").0),
            fact("", "t.k AS k, count(*) FILTER (WHERE t.k = 'a') AS n")
                .1
                .replace("GROUP BY 1", "GROUP BY 1, 2"),
            Some("view f: measure n compares k (an integer) with 'a'"),
        ),
        (
            "filterbool",
            format!("{} {publish}", fact("k text", "").0),
            fact("", "t.k AS k, count(*) FILTER (WHERE t.k = true) AS n")
                .1
                .replace("GROUP BY 1", "GROUP BY 1, 2"),
            Some("view f: measure n compares k (text) with true"),
        ),
        (
            "filterscale",
            format!("{} {publish}", fact("k numeric(6, 2)", "").0),
            fact("", "t.k AS k, count(*) FILTER (WHERE t.k = 1) AS n")
                .1
                .replace("GROUP BY 1", "GROUP BY 1, 2"),
            Some("view f: measure n compares k (a numeric of scale 2) with 1"),
        ),
        (
            "filterwhole",
            format!("{} {publish}", fact("k numeric(6, 0)", "").0),
            fact("", "t.k AS k, count(*) FILTER (WHERE t.k = 1) AS n")
                .1
                .replace("GROUP BY 1", "GROUP BY 1, 2"),
            None,
        ),
        (
            "nullkeyok",
            format!("{} {publish}", fact("k timestamp", "").0),
            fact(
                "",
                "(t.k IS NULL) AS open, count(*) FILTER (WHERE t.k IS NOT NULL) AS n",
            )
            .1
            .replace("GROUP BY 1", "GROUP BY 1, 2"),
            None,
        ),
        (
            "wheretype",
            format!("{} {publish}", fact("k text", "").0),
            fact("", "t.k AS k, count(*) AS n")
                .1
                .replace("AS t GROUP BY 1", "AS t WHERE t.k = 1 GROUP BY 1, 2"),
            Some("view f: WHERE compares k (text) with 1"),
        ),
        // An enum column is read as its label when written ::text (ADR-0037).
        (
            "enumok",
            format!("{enum_type} {} {publish}", fact("m $S.mood", "").0),
            fact(
                "",
                "t.m::text AS m, (t.m::text IS NULL) AS moody, \
                 count(*) FILTER (WHERE t.m::text = 'ok') AS n",
            )
            .1
            .replace("GROUP BY 1", "GROUP BY 1, 2, 3"),
            None,
        ),
        (
            "enumbare",
            format!("{enum_type} {} {publish}", fact("m $S.mood", "").0),
            fact("", "t.m AS m, count(*) AS n")
                .1
                .replace("GROUP BY 1", "GROUP BY 1, 2"),
            Some("f: column m of enumbare.f is enumbare.mood, an enum; write m::text"),
        ),
        (
            "notenum",
            format!("{} {publish}", fact("k text", "").0),
            fact("", "t.k::text AS k, count(*) AS n")
                .1
                .replace("GROUP BY 1", "GROUP BY 1, 2"),
            Some("f: column k of notenum.f is text, not an enum; drop the ::text"),
        ),
        (
            "nolabel",
            format!("{enum_type} {} {publish}", fact("m $S.mood", "").0),
            fact(
                "",
                "t.m::text AS m, count(*) FILTER (WHERE t.m::text IN ('ok', 'meh')) AS n",
            )
            .1
            .replace("GROUP BY 1", "GROUP BY 1, 2"),
            Some("view f: measure n compares m with 'meh', which is no label of nolabel.mood"),
        ),
    ];
    for (name, tables, spec, expected) in cases {
        expect_problems(&pg, name, &tables, &spec, expected);
    }

    let dim_spec = "
        CREATE VIEW f WITH (buckets = '1 hour') AS
        SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket,
               t.d_id AS d_id, d.name AS name, count(*) AS n
        FROM $S.f AS t LEFT OUTER JOIN $S.d AS d ON d.k = t.d_id GROUP BY 1, 2, 3;";
    // A dim is held and followed, so it is published with the whole old
    // row, as a fact is.
    let dim = |d: &str, via: &str| {
        format!(
            "CREATE TABLE $S.f (id int PRIMARY KEY, at timestamptz, d_id {via});
             ALTER TABLE $S.f REPLICA IDENTITY FULL;
             {d}
             ALTER TABLE $S.d REPLICA IDENTITY FULL;
             CREATE PUBLICATION $S FOR TABLE $S.f, $S.d;"
        )
    };
    let cases = [
        (
            "dimok",
            dim("CREATE TABLE $S.d (k int PRIMARY KEY, name text);", "int"),
            None,
        ),
        (
            "dimunpublished",
            dim("CREATE TABLE $S.d (k int PRIMARY KEY, name text);", "int").replace(", $S.d;", ";"),
            Some(
                "d: publication dimunpublished leaves out dimunpublished.d: \
             ALTER PUBLICATION dimunpublished ADD TABLE dimunpublished.d",
            ),
        ),
        (
            "dimidentity",
            dim("CREATE TABLE $S.d (k int PRIMARY KEY, name text);", "int")
                .replace("ALTER TABLE $S.d REPLICA IDENTITY FULL;", ""),
            Some(
                "dim d: Straum needs the whole old row of every update and delete: \
             ALTER TABLE dimidentity.d REPLICA IDENTITY FULL",
            ),
        ),
        (
            "dimfiltered",
            dim("CREATE TABLE $S.d (k int PRIMARY KEY, name text);", "int")
                .replace("$S.f, $S.d;", "$S.f, $S.d WHERE (k > 0);"),
            Some(
                "d: publication dimfiltered filters the rows of dimfiltered.d; Straum needs every row",
            ),
        ),
        (
            "dimlisted",
            dim("CREATE TABLE $S.d (k int PRIMARY KEY, name text);", "int")
                .replace("$S.f, $S.d;", "$S.f, $S.d (k);"),
            Some("d: publication dimlisted leaves out column name of dimlisted.d"),
        ),
        (
            "dimunique",
            dim("CREATE TABLE $S.d (k int UNIQUE, name text);", "int"),
            None,
        ),
        (
            "dimpk",
            dim(
                "CREATE TABLE $S.d (s serial PRIMARY KEY, k int, name text);",
                "int",
            ),
            None,
        ),
        (
            "dimnounique",
            dim(
                "CREATE TABLE $S.d (k int, name text); ALTER TABLE $S.d REPLICA IDENTITY FULL;",
                "int",
            ),
            None,
        ),
        (
            "dimtype",
            dim("CREATE TABLE $S.d (k text PRIMARY KEY, name text);", "int"),
            Some("view f: join d joins d_id (an integer) to k (text); they must read alike"),
        ),
    ];
    for (name, tables, expected) in cases {
        expect_problems(&pg, name, &tables, dim_spec, expected);
    }
    // As the role that may read the fact but not the dim.
    let name = "dimnoselect";
    let tables = dim("CREATE TABLE $S.d (k int PRIMARY KEY, name text);", "int").replace(
        "CREATE PUBLICATION",
        "CREATE ROLE $S LOGIN REPLICATION PASSWORD 'r'; GRANT USAGE ON SCHEMA $S TO $S; \
             GRANT SELECT ON $S.f TO $S; CREATE PUBLICATION",
    );
    let db = Db::new(&pg, name, &tables);
    let dsn = pg
        .dsn
        .to_string()
        .replace("postgres@", &format!("{name}:r@"));
    let mut source = Postgres::new(Dsn::parse(&dsn).unwrap(), name, ROW_LIMIT);
    assert_eq!(
        setup_problems(name, source.open(&db.spec(dim_spec))),
        ["d: Straum's role can't read dimnoselect.d: GRANT SELECT ON dimnoselect.d"],
        "{name}"
    );

    // A dim keyed by two columns, with or without a key of its own.
    let pair_spec = "
        CREATE VIEW f WITH (buckets = '1 hour') AS
        SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket,
               t.a AS a, t.b AS b, d.name AS name, count(*) AS n
        FROM $S.f AS t LEFT OUTER JOIN $S.d AS d ON d.x = t.a AND d.y = t.b
        GROUP BY 1, 2, 3, 4;";
    let pair = |d: &str, b: &str| {
        format!(
            "CREATE TABLE $S.f (id int PRIMARY KEY, at timestamptz, a int, b {b});
             ALTER TABLE $S.f REPLICA IDENTITY FULL;
             {d}
             ALTER TABLE $S.d REPLICA IDENTITY FULL;
             CREATE PUBLICATION $S FOR TABLE $S.f, $S.d;"
        )
    };
    let cases = [
        (
            "pairok",
            pair(
                "CREATE TABLE $S.d (x int, y int, name text, PRIMARY KEY (x, y));",
                "int",
            ),
            None,
        ),
        (
            "pairorder",
            pair(
                "CREATE TABLE $S.d (x int, y int, name text, PRIMARY KEY (y, x));",
                "int",
            ),
            None,
        ),
        (
            "pairfull",
            pair(
                "CREATE TABLE $S.d (x int, y int, name text, UNIQUE (x, y));
                 ALTER TABLE $S.d REPLICA IDENTITY FULL;",
                "int",
            ),
            None,
        ),
        (
            "pairsuper",
            pair(
                "CREATE TABLE $S.d (x int, y int, name text, UNIQUE (x, y, name));
                 ALTER TABLE $S.d REPLICA IDENTITY FULL;",
                "int",
            ),
            None,
        ),
        (
            "pairpk",
            pair(
                "CREATE TABLE $S.d (x int PRIMARY KEY, y int, name text);",
                "int",
            ),
            None,
        ),
        (
            "pairtype",
            pair(
                "CREATE TABLE $S.d (x int, y text, name text, PRIMARY KEY (x, y));",
                "int",
            ),
            Some("view f: join d joins b (an integer) to y (text); they must read alike"),
        ),
    ];
    for (name, tables, expected) in cases {
        expect_problems(&pg, name, &tables, pair_spec, expected);
    }

    // A chain joins a dim's column to the next dim's key, and a semi join's
    // dim is published like any other.
    let chain_spec = "
        CREATE VIEW f WITH (buckets = '1 hour') AS
        SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket,
               d.name AS name, e.label AS label, count(*) AS n
        FROM $S.f AS t
        LEFT OUTER JOIN $S.d AS d ON d.k = t.d_id
        LEFT OUTER JOIN $S.e AS e ON e.k = d.e_id
        WHERE EXISTS (SELECT 1 FROM $S.s AS s WHERE s.k = t.d_id)
        GROUP BY 1, 2, 3;";
    let chain = |e_key: &str, publish: &str| {
        format!(
            "CREATE TABLE $S.f (id int PRIMARY KEY, at timestamptz, d_id int);
             CREATE TABLE $S.d (k int, name text, e_id int);
             CREATE TABLE $S.e (k {e_key}, label text);
             CREATE TABLE $S.s (k int);
             ALTER TABLE $S.f REPLICA IDENTITY FULL;
             ALTER TABLE $S.d REPLICA IDENTITY FULL;
             ALTER TABLE $S.e REPLICA IDENTITY FULL;
             ALTER TABLE $S.s REPLICA IDENTITY FULL;
             CREATE PUBLICATION $S FOR TABLE {publish};"
        )
    };
    let cases = [
        ("chainok", chain("int", "$S.f, $S.d, $S.e, $S.s"), None),
        (
            "chaintype",
            chain("text", "$S.f, $S.d, $S.e, $S.s"),
            Some("view f: join e joins d.e_id (an integer) to k (text); they must read alike"),
        ),
        (
            "semiunpublished",
            chain("int", "$S.f, $S.d, $S.e"),
            Some(
                "s: publication semiunpublished leaves out semiunpublished.s: \
             ALTER PUBLICATION semiunpublished ADD TABLE semiunpublished.s",
            ),
        ),
    ];
    for (name, tables, expected) in cases {
        expect_problems(&pg, name, &tables, chain_spec, expected);
    }
}

#[test]
fn binary_copies_read_as_text_does() {
    let pg = postgres();
    let mut conn = Connection::connect(&pg.dsn, false).unwrap();
    let types = [
        ColumnType::Int,
        ColumnType::Int,
        ColumnType::Int,
        ColumnType::Numeric { scale: 2 },
        ColumnType::Numeric { scale: 0 },
        ColumnType::Numeric { scale: 4 },
        ColumnType::Bool,
        ColumnType::Text,
        ColumnType::Text,
        ColumnType::Text,
        ColumnType::Time { zoned: true },
        ColumnType::Time { zoned: false },
    ];
    // The session zone must not reach a timestamp's value in either form.
    conn.execute(
        "SET TIME ZONE 'Europe/Oslo';
         CREATE TABLE every (a int2, b int4, c int8, d numeric(10, 2), e numeric(18, 0),
                             f numeric(18, 4), g boolean, h text, i varchar(5), j char(4),
                             k timestamptz, l timestamp);
         INSERT INTO every VALUES
           (-32768, 2147483647, -9223372036854775808, -12345678.9, 999999999999999999,
            0.0001, true, E'tab\\there\\\\ ø', 'abc', 'ab', '2024-02-29 13:47:05.999999+00',
            '2024-02-29 13:47:05.999999'),
           (0, 0, 0, 0, 0, 12345678901234.5678, false, '', '', '', '1969-12-31 23:59:59.5+00',
            '1969-12-31 23:59:59.5'),
           (NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
           (1, 1, 1, 0.01, -1, -0.5, true, 'x', 'xxxxx', 'xxxx', '0001-01-01 00:00:00+00',
            '0001-01-01 00:00:00'),
           (2, 2, 2, 99999999.99, 10000, 100000000, false, 'y', 'y', 'y',
            '294276-12-31 23:59:59+00', '294276-12-31 23:59:59'),
           (3, 3, 3, 1, 1, 1, true, 'z', 'z', 'z', 'infinity', '-infinity');
         SET TIME ZONE 'UTC';",
    )
    .unwrap();
    let columns = "a, b, c, d, e, f, g, h, i, j, k, l";
    let rows = conn.rows(&format!("SELECT {columns} FROM every")).unwrap();
    let texts: Vec<Vec<Datum>> = (rows.iter())
        .map(|fields| {
            let row = types.iter().zip(fields).map(|(column_type, field)| {
                field
                    .as_deref()
                    .map_or(Ok(Datum::Null), |text| read(*column_type, text))
            });
            row.collect::<Result<Vec<_>, _>>().unwrap()
        })
        .collect();
    let mut binaries = Vec::new();
    let every = Checked {
        enums: Vec::new(),
        name: id("every"),
        schema: id("public"),
        table: id("every"),
        columns: columns.split(", ").map(id).collect(),
        types: types.to_vec(),
    };
    load::copy(&mut conn, &every, |batch| {
        binaries.extend(batch.chunks(types.len()).map(<[Datum]>::to_vec));
        Ok(())
    })
    .unwrap();
    assert_eq!(texts.len(), 6);
    assert_eq!(binaries, texts);
    // The infinities read as a null time, both ways.
    assert_eq!(&texts[5][10..], [Datum::Null, Datum::Null]);
}

#[test]
fn reports_wal_level_and_read_access() {
    let pg = postgres();
    let mut db = Db::new(&pg, "access", TABLES);
    let spec = db.spec(SPEC);
    let mut catalog = catalog::query(&mut db.conn, &spec, "access").unwrap();
    assert!(catalog::check(&spec, &catalog, "access").is_ok());
    let major: String = tag().chars().take_while(char::is_ascii_digit).collect();
    assert_eq!(
        catalog.server_version / 10_000,
        major.parse::<u32>().unwrap()
    );
    for (version, wal_level, needs) in [
        (170_000, "replica", Some("logical")),
        (180_000, "replica", Some("logical")),
        (180_000, "logical", None),
        (190_000, "replica", None),
        (190_000, "logical", None),
        (190_000, "minimal", Some("replica or logical")),
        (200_000, "replica", None),
    ] {
        catalog.server_version = version;
        catalog.wal_level = wal_level.into();
        let expected = needs.map(|needs| {
            vec![format!(
                "wal_level is {wal_level}; Straum needs {needs}, which takes a restart"
            )]
        });
        assert_eq!(
            catalog::check(&spec, &catalog, "access").err(),
            expected,
            "{version} {wal_level}"
        );
    }

    db.run("CREATE ROLE reader LOGIN REPLICATION PASSWORD 'r'; GRANT USAGE ON SCHEMA $S TO reader");
    let dsn = pg.dsn.to_string().replace("postgres@", "reader:r@");
    let mut reader = Postgres::new(Dsn::parse(&dsn).unwrap(), "access", ROW_LIMIT);
    let err = reader.open(&spec).unwrap_err().to_string();
    for table in ["zones", "trips", "logins"] {
        let line =
            format!("Straum's role can't read access.{table}: GRANT SELECT ON access.{table}");
        assert!(err.contains(&line), "{err}");
    }
}

#[test]
fn connection_errors() {
    let pg = postgres();
    let wrong = pg.dsn.to_string().replace("postgres@", "postgres:wrong@");
    let mut source = Postgres::new(Dsn::parse(&wrong).unwrap(), "p", ROW_LIMIT);
    let spec = straum_notation::parse(&SPEC.replace("$S", "p")).unwrap();
    let err = source.open(&spec).unwrap_err().to_string();
    assert!(
        err.contains("password authentication failed") && err.contains("(28P01)"),
        "{err}"
    );

    let bare = Dsn::parse(&pg.dsn.to_string()).unwrap();
    let err = Postgres::new(bare, "p", ROW_LIMIT)
        .open(&spec)
        .unwrap_err()
        .to_string();
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
    let err = Postgres::new(closed, "p", ROW_LIMIT)
        .open(&spec)
        .unwrap_err()
        .to_string();
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
    let err = idle.load(&trips.name, &mut store);
    assert_eq!(err.unwrap_err().to_string(), stopped);
}

#[test]
fn no_load_once_the_stream_runs() {
    let pg = postgres();
    let mut db = Db::new(&pg, "streaming", TABLES);
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    let start = source.open(&spec).unwrap();
    db.run(&Rng::new(1).trip(1));
    let commit = source.next_commit(LONG).unwrap().unwrap();
    assert!(commit.lsn > start);
    let mut store = Store::new(spec.clone());
    let err = source.load(&id("trips"), &mut store);
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

/// A fake for PostgreSQL: writes each part after its pause, then stops
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

fn connect(parts: Vec<(Duration, Vec<u8>)>, replication: bool) -> Result<Connection, Error> {
    let (dsn, _server) = scripted(parts);
    Connection::connect(&dsn, replication)
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
            "PostgreSQL sent an authentication request Straum doesn't support; \
             Straum signs in with SCRAM-SHA-256 or trust",
        ),
        (
            sasl("SCRAM-SHA-256-PLUS\0\0"),
            "PostgreSQL sent SASL mechanisms [\"SCRAM-SHA-256-PLUS\"]; Straum needs SCRAM-SHA-256",
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
    // CopyInResponse: Straum never copies in.
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

/// CopyOutResponse for `columns` binary columns.
fn copy_out(columns: i16) -> Vec<u8> {
    let mut body = vec![1];
    body.extend(columns.to_be_bytes());
    body.extend((0..columns).flat_map(|_| 1_i16.to_be_bytes()));
    message(b'H', &body)
}

/// The signature, no flags and a header extension of `extension`.
fn copy_header(extension: &[u8]) -> Vec<u8> {
    let mut header = b"PGCOPY\n\xff\r\n\0".to_vec();
    header.extend(0_i32.to_be_bytes());
    header.extend(u32::try_from(extension.len()).unwrap().to_be_bytes());
    header.extend(extension);
    header
}

/// A binary copy row of `int4` fields, `None` for null.
fn copy_tuple(fields: &[Option<i32>]) -> Vec<u8> {
    let mut row = i16::try_from(fields.len()).unwrap().to_be_bytes().to_vec();
    for field in fields {
        match field {
            None => row.extend((-1_i32).to_be_bytes()),
            Some(v) => {
                row.extend(4_i32.to_be_bytes());
                row.extend(v.to_be_bytes());
            }
        }
    }
    row
}

const COPY_END: [u8; 2] = (-1_i16).to_be_bytes();

/// The rows a fake server's copy of two `int4` columns gives, sent as
/// `messages` CopyData bodies, then `after`.
fn copied(messages: &[Vec<u8>], after: &[u8]) -> Result<Vec<Vec<Datum>>, Error> {
    let mut reply = copy_out(2);
    for m in messages {
        reply.extend(copy_data(m));
    }
    reply.extend(after);
    let mut conn = connect(vec![now(signed_in()), now(reply)], false)?;
    let mut rows = Vec::new();
    load::copy(&mut conn, &two_ints(), |batch| {
        rows.extend(batch.chunks(2).map(<[Datum]>::to_vec));
        Ok(())
    })?;
    Ok(rows)
}

/// Two `int4` columns, `a` and `b` of `t`, as a fake copy sends.
fn two_ints() -> Checked {
    Checked {
        enums: Vec::new(),
        name: id("t"),
        schema: id("public"),
        table: id("t"),
        columns: vec![id("a"), id("b")],
        types: vec![ColumnType::Int; 2],
    }
}

fn copy_done() -> Vec<u8> {
    let mut m = message(b'c', b"");
    m.extend(message(b'C', b"COPY 2\0"));
    m.extend(message(b'Z', b"I"));
    m
}

#[test]
fn reads_binary_copies() {
    let rows = [
        vec![Datum::Int(1), Datum::Null],
        vec![Datum::Int(-3), Datum::Int(4)],
    ];
    // As PostgreSQL sends it: the header with the first row, then a
    // message per row and one for the end.
    let mut first = copy_header(b"");
    first.extend(copy_tuple(&[Some(1), None]));
    let sent = [first, copy_tuple(&[Some(-3), Some(4)]), COPY_END.to_vec()];
    assert_eq!(copied(&sent, &copy_done()).unwrap(), rows);
    // Header, rows and end in one message, with a header extension.
    let mut one = copy_header(b"ext");
    one.extend(copy_tuple(&[Some(1), None]));
    one.extend(copy_tuple(&[Some(-3), Some(4)]));
    one.extend(COPY_END);
    assert_eq!(copied(&[one], &copy_done()).unwrap(), rows);
    // An empty table.
    let mut empty = copy_header(b"");
    empty.extend(COPY_END);
    assert_eq!(
        copied(&[empty], &copy_done()).unwrap(),
        Vec::<Vec<Datum>>::new()
    );
}

#[test]
fn copies_hand_over_bounded_batches() {
    let count = 2 * load::BATCH + 7;
    let mut sent = vec![copy_header(b"")];
    sent.extend((0..count).map(|i| copy_tuple(&[Some(i32::try_from(i).unwrap()), None])));
    sent.push(COPY_END.to_vec());
    let mut reply = copy_out(2);
    for m in &sent {
        reply.extend(copy_data(m));
    }
    reply.extend(copy_done());
    let mut conn = connect(vec![now(signed_in()), now(reply.clone())], false).unwrap();
    let mut batches = Vec::new();
    let mut next = 0;
    load::copy(&mut conn, &two_ints(), |batch| {
        batches.push(batch.len() / 2);
        for row in batch.chunks(2) {
            assert_eq!(row, [Datum::Int(next), Datum::Null]);
            next += 1;
        }
        Ok(())
    })
    .unwrap();
    let full = load::BATCH;
    assert_eq!(batches, [full, full, 7]);

    // A failed batch stops the copy, and its error is the one returned.
    let mut conn = connect(vec![now(signed_in()), now(reply)], false).unwrap();
    let mut calls = 0;
    let err = load::copy(&mut conn, &two_ints(), |_| {
        calls += 1;
        Err(Error::Protocol("a row Straum refused".into()))
    })
    .unwrap_err();
    assert_eq!(err.to_string(), "PostgreSQL sent a row Straum refused");
    assert_eq!(calls, 1);
}

#[test]
fn binary_copy_errors() {
    let header = copy_header(b"");
    let row = copy_tuple(&[Some(1), Some(2)]);
    let with = |parts: &[&[u8]]| parts.concat();
    let mut cut = with(&[&header, &row]);
    cut.pop();
    let mut cut_header = copy_header(b"ext");
    cut_header.pop();
    let cases = [
        (
            vec![b"1\t2\n".to_vec()],
            "PostgreSQL sent a copy that isn't binary",
        ),
        (
            vec![header[..13].to_vec()],
            "PostgreSQL sent a copy row cut short",
        ),
        (vec![cut_header], "PostgreSQL sent a copy header cut short"),
        (
            vec![with(&[&header, &copy_tuple(&[Some(1)])])],
            "PostgreSQL sent a copy row of 1 columns, not 2",
        ),
        (
            vec![with(&[&header, &row[..3]])],
            "PostgreSQL sent a copy row cut short",
        ),
        (vec![cut], "PostgreSQL sent a copy value cut short"),
        (
            vec![with(&[&header, &row, &COPY_END, &row])],
            "PostgreSQL sent data after the end of a copy",
        ),
        (
            vec![with(&[&header, &row])],
            "PostgreSQL sent a copy without its end",
        ),
        (
            vec![with(&[
                &header,
                &2_i16.to_be_bytes(),
                &4_i32.to_be_bytes(),
                &1_i32.to_be_bytes(),
                &3_i32.to_be_bytes(),
                &[0, 0, 1],
                &COPY_END,
            ])],
            "can't read \"\\\\x000001\" as an integer in column b of public.t",
        ),
    ];
    for (sent, expected) in cases {
        let err = copied(&sent, &copy_done()).unwrap_err().to_string();
        assert!(err.starts_with(expected), "{err}");
    }
    // The server fails mid-copy, or sends what a copy doesn't.
    let mut failed = error_response("57014", "canceling statement");
    failed.extend(message(b'Z', b"I"));
    let err = copied(&[with(&[&header, &row])], &failed).unwrap_err();
    assert_eq!(err.to_string(), "PostgreSQL: canceling statement (57014)");
    let err = copied(&[header], &message(b'D', &0_i16.to_be_bytes())).unwrap_err();
    assert_eq!(err.to_string(), "PostgreSQL sent a message during a copy");
    // Notices and parameters along the way are fine.
    let mut chatty = message(b'N', b"SNOTICE\0Mhello\0\0");
    chatty.extend(message(b'S', b"TimeZone\0UTC\0"));
    chatty.extend(copy_done());
    let mut empty = copy_header(b"");
    empty.extend(COPY_END);
    assert!(copied(&[empty], &chatty).unwrap().is_empty());
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

/// A stream from a fake server that sends `parts` once it has started.
fn scripted_stream(parts: Vec<(Duration, Vec<u8>)>) -> (Stream, thread::JoinHandle<Vec<u8>>) {
    let mut start = signed_in();
    start.extend(message(b'N', b"SNOTICE\0Mstarting\0\0"));
    start.extend(message(b'W', &[0, 0, 0]));
    let (dsn, server) = scripted([vec![now(start)], parts].concat());
    let mut conn = Connection::connect(&dsn, true).unwrap();
    conn.copy_both("START_REPLICATION").unwrap();
    let stream = Stream::start(conn, Decoder::new(Vec::new(), ROW_LIMIT), Lsn(0x10)).unwrap();
    (stream, server)
}

#[test]
fn stream_errors() {
    let cases = [
        (copy_data(b"x"), "unknown pgoutput tag 'x'"),
        (copy_data(b"w"), "pgoutput message ends early"),
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
    assert!(wal.restart <= start && start <= wal.written, "{wal:?}");

    db.run(&Rng::new(1).trip(1));
    let commit = source.next_commit(LONG).unwrap().unwrap();
    assert!(source.wal().unwrap().written >= commit.lsn);
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
    let mut conn = Connection::connect(&pg.dsn, false).unwrap();
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

/// The taxi month through straum: load, stream inserts and dim changes, read
/// the fact again after an update, and every time compare the whole
/// `taxi.sql` view with PostgreSQL. `bench/load_taxi_trips.py` puts the month
/// in PostgreSQL, so it needs uv. Slow, so ignored; run
/// `cargo test -p straum-pg --release taxi_month -- --ignored --nocapture`.
#[test]
#[ignore]
fn taxi_month() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let spec_text =
        std::fs::read_to_string(root.join("crates/straum-notation/tests/taxi.sql")).unwrap();
    let (container, port) = start(
        image("18")
            .with_shm_size(1 << 30)
            .with_cmd(["-c", "wal_level=logical"]),
    );
    let url = format!("postgres://postgres:{PASSWORD}@127.0.0.1:{port}/postgres?sslmode=disable");
    let dsn = Dsn::parse(&url).unwrap();
    let clock = Instant::now();
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
    let loaded = loader.wait().unwrap();
    assert!(loaded.success(), "bench/load_taxi_trips.py: {loaded}");
    let mut sql = Connection::connect(&dsn, false).unwrap();
    sql.execute(
        "ALTER TABLE yellow_trips REPLICA IDENTITY FULL;
         ALTER TABLE taxi_zones REPLICA IDENTITY FULL;
         CREATE PUBLICATION straum FOR TABLE taxi_zones, yellow_trips;
         ANALYZE;",
    )
    .unwrap();
    let rows = |sql: &mut Connection, q: &str| sql.rows(q).unwrap();
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
            "pu_location_id",
            "do_location_id",
            "borough",
            "zone",
            "service_zone",
        ],
    );
    let all_scales = [0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 2, 0, 0];
    // A coarse ask, the use case: it reads every cell, as the one above
    // does, but answers few groups.
    let coarse = ask(
        "trips",
        &["trips", "fare", "tip_max", "card", "manhattan_pickups"],
        &["borough"],
    );
    let coarse_reference = format!(
        "SELECT borough,
                sum(trips), sum(fare), max(tip_max), sum(card), sum(manhattan_pickups)
           FROM ({body}) v
          GROUP BY 1"
    );
    let coarse_scales = [0, 0, 2, 2, 0, 0];
    // Straum's time is the time `ask` holds the read lock.
    let compare = |sql: &mut Connection,
                   app: &App,
                   (ask, reference, scales): (&Ask, &str, &[u32]),
                   when: &str| {
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
            "{when}: {} rows from Straum, {} from PostgreSQL; first difference: {:?} vs {:?}",
            got.len(),
            expected.len(),
            differ.map(|i| &got[i]),
            differ.map(|i| &expected[i])
        );
        eprintln!(
            "{when}: {} groups equal; Straum asked in {asked:.1?}, PostgreSQL in {queried:.1?}",
            got.len()
        );
    };
    let check = |sql: &mut Connection, app: &App, when: &str| {
        compare(sql, app, (&all, &reference, &all_scales), when);
        let when = format!("{when}, by borough");
        compare(
            sql,
            app,
            (&coarse, &coarse_reference, &coarse_scales),
            &when,
        );
    };

    let mut writer = Writer::new(
        App::new(spec, LIMITS, None),
        Postgres::new(dsn, "straum", ROW_LIMIT),
    );
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
    for _ in 0..200 {
        applied(&mut writer);
    }
    eprintln!("streamed 200 commits in {:.1?}", clock.elapsed());
    check(&mut sql, writer.app(), "after the stream");

    // An update takes the old row out of its cell.
    sql.execute("UPDATE yellow_trips SET tip_amount = tip_amount + 1 WHERE id = 1")
        .unwrap();
    applied(&mut writer);
    check(&mut sql, writer.app(), "after an update");

    // Dims change: a zone renamed, another moved to a new key, then a
    // trip in the renamed zone. Each is one commit applied; nothing
    // starts over.
    let change =
        |sql: &mut Connection, writer: &mut Writer<Postgres>, what: &str, statement: &str| {
            sql.execute(statement).unwrap();
            let clock = Instant::now();
            applied(writer);
            eprintln!("applied {what} in {:.1?}", clock.elapsed());
        };
    change(
        &mut sql,
        &mut writer,
        "a zone renamed",
        "UPDATE taxi_zones SET zone = zone || ' (renamed)' WHERE location_id = 161",
    );
    check(&mut sql, writer.app(), "after a zone was renamed");
    change(
        &mut sql,
        &mut writer,
        "a zone moved",
        "UPDATE taxi_zones SET location_id = 999 WHERE location_id = 264",
    );
    check(&mut sql, writer.app(), "after a zone moved");
    change(
        &mut sql,
        &mut writer,
        "a trip in the renamed zone",
        "UPDATE yellow_trips SET tip_amount = tip_amount + 1
          WHERE id = (SELECT min(id) FROM yellow_trips WHERE pu_location_id = 161)",
    );
    check(&mut sql, writer.app(), "after the zones changed");
    assert_eq!(writer.app().status().starts, 1);
}
