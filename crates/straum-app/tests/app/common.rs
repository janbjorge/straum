//! What every case shares: the spec of two views over a fake database,
//! the limits, and helpers that write rows, build asks and start a writer.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use straum_app::{AnswerError, Answered, App, Limits, Lsn, Wal, Writer};
use straum_domain::{Answer, AnswerRow, Ask, AskKey, Datum, Spec, Value};

use crate::fake::{Database, Fake, Identity, Published, Row, Write, id};

/// Seconds in a day.
pub const DAY: i64 = 86_400;
/// Every row's time falls in `[0, RANGE)`.
pub const RANGE: i64 = 3 * DAY;

/// Bytes in a gibibyte.
pub const BYTES_PER_GIB: u64 = 1 << 30;

/// Limits no case reaches unless it means to.
pub const LIMITS: Limits = Limits {
    max_lag: Duration::from_secs(30),
    max_retained_wal: 4 * BYTES_PER_GIB,
    max_load_memory: 1 << 30,
};

const SPEC: &str = "
CREATE VIEW trips AS
SELECT buckets(t.at, '1 hour') AS bucket,
       t.kind AS kind,
       t.zone_id AS zone_id,
       zone.borough AS borough,
       count(*) AS trips,
       sum(t.fare) AS fare,
       max(t.fare) AS fare_max,
       count(DISTINCT t.rider) AS riders
FROM public.trips AS t
LEFT OUTER JOIN public.zones AS zone ON zone.id = t.zone_id
GROUP BY 1, 2, 3, 4;

CREATE VIEW logins AS
SELECT buckets(l.at, '1 day') AS bucket,
       zone.borough AS borough,
       count(*) AS logins
FROM public.logins AS l
LEFT OUTER JOIN public.zones AS zone ON zone.id = l.zone_id
GROUP BY 1, 2;
";

/// `audit` is published but not in the spec. Only zones and trips have
/// `REPLICA IDENTITY FULL`.
const TABLES: [Published; 4] = [
    Published {
        name: "zones",
        identity: Identity::Full,
    },
    Published {
        name: "trips",
        identity: Identity::Full,
    },
    Published {
        name: "logins",
        identity: Identity::Default,
    },
    Published {
        name: "audit",
        identity: Identity::Default,
    },
];

/// The boroughs zones lie in.
pub const BOROUGHS: [&str; 3] = ["Bronx", "Brooklyn", "Queens"];
/// The riders trips name.
pub const RIDERS: [&str; 5] = ["ann", "bo", "cy", "di", "ed"];

/// A writer over a fake database, and the database, to write to while the
/// writer reads it.
pub struct Harness {
    pub database: Rc<RefCell<Database>>,
    pub writer: Writer<Fake>,
}

/// The spec of the `trips` and `logins` views.
pub fn spec() -> Spec {
    straum_notation::parse(SPEC).unwrap()
}

/// An integer as a row holds it.
pub fn int(value: i64) -> Datum {
    Datum::Int(value)
}

/// Text as a row holds it.
pub fn text(value: &str) -> Datum {
    Datum::Text(value.into())
}

/// The published tables, empty, shared between a case and its writers.
pub fn database() -> Rc<RefCell<Database>> {
    Database::new(&TABLES).shared()
}

/// A writer over the empty tables.
pub fn setup() -> Harness {
    let database = database();
    let writer = Writer::new(App::new(spec(), LIMITS), Fake::new(&database));
    Harness { database, writer }
}

/// A new writer over the same database, as whatever runs Straum starts
/// it again after a failure: a new app, empty until its first step.
pub fn again(database: &Rc<RefCell<Database>>) -> Writer<Fake> {
    Writer::new(App::new(spec(), LIMITS), Fake::new(database))
}

/// A writer whose loads call `on_load` with the app and the table, in the
/// middle of each load.
// Takes the case's own closure, so it may capture what the case checks
// after; `'static` because the fake keeps it.
pub fn watched(app: App, on_load: impl FnMut(&App, &str) + 'static) -> Harness {
    let database = database();
    let writer = watching(app, &database, on_load);
    Harness { database, writer }
}

/// As [`watched`], over a database the case already has.
// `impl FnMut` and `'static` as in `watched`, which hands its closure on.
pub fn watching(
    app: App,
    database: &Rc<RefCell<Database>>,
    mut on_load: impl FnMut(&App, &str) + 'static,
) -> Writer<Fake> {
    let mut fake = Fake::new(database);
    let watch = app.clone();
    fake.on_load = Some(Box::new(move |table| on_load(&watch, table)));
    Writer::new(app, fake)
}

/// A new row under primary key `key`.
pub fn insert(table: &'static str, key: i64, row: Row) -> Write {
    Write::Insert {
        table,
        key: int(key),
        row,
    }
}

/// The row under primary key `key`, changed in place.
pub fn update(table: &'static str, key: i64, row: Row) -> Write {
    Write::Update {
        table,
        key: int(key),
        new_key: int(key),
        row,
    }
}

/// A zone row, `[id, borough]`.
pub fn zone(key: i64, borough: &str) -> Write {
    insert("zones", key, vec![int(key), text(borough)])
}

/// `[at, kind, zone_id, fare, rider]`.
pub fn trip(key: i64, kind: i64, zone: i64, fare: i64) -> Write {
    insert(
        "trips",
        key,
        vec![int(0), int(kind), int(zone), int(fare), text("ann")],
    )
}

/// `[at, zone_id]`.
pub fn login(key: i64, zone: i64) -> Write {
    insert("logins", key, vec![int(0), int(zone)])
}

/// An ask of the fact's measures by these keys, over the whole range.
pub fn ask(fact: &str, measures: &[&str], by: &[&str]) -> Ask {
    let mut keys = Vec::new();
    for name in by {
        keys.push(AskKey::Key(id(name)));
    }
    Ask {
        view: id(fact),
        measures: measures.iter().copied().map(id).collect(),
        keys,
        filter: Vec::new(),
        having: Vec::new(),
        from: Some(0),
        to: Some(RANGE),
        bounded: None,
    }
}

/// The fact's row count over the whole range.
pub fn count(app: &App, fact: &str) -> Result<Value, AnswerError> {
    let answered = app.ask(&ask(fact, &[fact], &[]))?;
    Ok(answered.answer.rows[0].values[0].clone())
}

/// As [`count`], asked from another thread as [`aside`] asks.
pub fn count_aside(app: &App, fact: &str) -> Result<Value, AnswerError> {
    let answered = aside(app, ask(fact, &[fact], &[]))?;
    Ok(answered.answer.rows[0].values[0].clone())
}

/// The answer a fresh load of the database gives now.
pub fn reference_answer(database: &Database, ask: &Ask) -> Answer {
    database.store_at(&spec(), database.lsn()).ask(ask).unwrap()
}

/// An answer row of one key and one count.
pub fn row(key: Datum, count: i128) -> AnswerRow {
    AnswerRow {
        keys: vec![key],
        values: vec![Value::Int(count)],
    }
}

/// The rows an ask by borough answers.
pub fn by_borough(app: &App, fact: &str) -> Vec<AnswerRow> {
    let by_borough = ask(fact, &[fact], &["borough"]);
    app.ask(&by_borough).unwrap().answer.rows
}

/// Asks the app from another thread, so a writer that holds the lock while
/// it loads fails the test instead of hanging it.
pub fn aside(app: &App, ask: Ask) -> Result<Answered, AnswerError> {
    let (sender, receiver) = mpsc::channel();
    let app = app.clone();
    thread::spawn(move || sender.send(app.ask(&ask)));
    receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("the writer held the store while it loaded")
}

/// A WAL reading with every commit up to `applied` applied, and the slot
/// holding WAL from there on.
pub fn wal(written: u64, applied: u64) -> Wal {
    Wal {
        written: Lsn(written),
        applied: Lsn(applied),
        restart: Lsn(applied),
    }
}
