//! `App` against an in-memory PostgreSQL: hand-made cases, then random
//! writes, dropped connections and restarts checked against a fresh load of
//! the database at the same LSN.

mod fake;
mod spill;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, SystemTime};

use straum_app::{
    AnswerError, Answered, App, Change, Error, Held, HeldDim, HeldKey, HeldView, Limits, Lsn,
    Spill, Spilling, Status, Step, Wal, Writer,
};
use straum_domain::{
    Answer, AnswerRow, Ask, AskError, AskKey, ColumnType, Condition, Datum, Literal, SealedId,
    Spec, Table, Value, Width,
};

use fake::{Database, Fake, Row, Write, id, time};
use spill::MemorySpill;

const DAY: i64 = 86_400;
/// Every row's time falls in `[0, RANGE)`.
const RANGE: i64 = 3 * DAY;

const GIB: u64 = 1 << 30;

const LIMITS: Limits = Limits {
    max_lag: Duration::from_secs(30),
    max_retained_wal: 4 * GIB,
};

const SPEC: &str = "
CREATE VIEW trips WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket,
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

CREATE VIEW logins WITH (buckets = '1 day') AS
SELECT date_bin('1 day', l.at, timestamptz 'epoch') AS bucket,
       zone.borough AS borough,
       count(*) AS logins
FROM public.logins AS l
LEFT OUTER JOIN public.zones AS zone ON zone.id = l.zone_id
GROUP BY 1, 2;
";

/// `audit` is published but not in the spec. Only zones and trips have
/// `REPLICA IDENTITY FULL`.
const TABLES: [(&str, bool); 4] = [
    ("zones", true),
    ("trips", true),
    ("logins", false),
    ("audit", false),
];

const BOROUGHS: [&str; 3] = ["Bronx", "Brooklyn", "Queens"];
const RIDERS: [&str; 5] = ["ann", "bo", "cy", "di", "ed"];

fn spec() -> Spec {
    straum_notation::parse(SPEC).unwrap()
}

fn int(value: i64) -> Datum {
    Datum::Int(value)
}

fn text(value: &str) -> Datum {
    Datum::Text(value.into())
}

fn setup(tables: &[(&'static str, bool)]) -> (Rc<RefCell<Database>>, Writer<Fake>) {
    let db = Database::new(tables).shared();
    let writer = Writer::new(App::new(spec(), LIMITS, None), Fake::new(&db));
    (db, writer)
}

/// A writer whose loads call `on_load` with the app and the table, in the
/// middle of each load.
fn watched(
    app: App,
    mut on_load: impl FnMut(&App, &str) + 'static,
) -> (Rc<RefCell<Database>>, Writer<Fake>) {
    let db = Database::new(&TABLES).shared();
    let mut fake = Fake::new(&db);
    let watch = app.clone();
    fake.on_load = Some(Box::new(move |table| on_load(&watch, table)));
    (db, Writer::new(app, fake))
}

fn insert(table: &'static str, key: i64, row: Row) -> Write {
    Write::Insert {
        table,
        key: int(key),
        row,
    }
}

fn zone(key: i64, borough: &str) -> Write {
    insert("zones", key, vec![int(key), text(borough)])
}

/// `[at, kind, zone_id, fare, rider]`.
fn trip(key: i64, kind: i64, zone: i64, fare: i64) -> Write {
    insert(
        "trips",
        key,
        vec![int(0), int(kind), int(zone), int(fare), text("ann")],
    )
}

/// `[at, zone_id]`.
fn login(key: i64, zone: i64) -> Write {
    insert("logins", key, vec![int(0), int(zone)])
}

fn ask(fact: &str, measures: &[&str], by: &[&str]) -> Ask {
    Ask {
        view: id(fact),
        measures: measures.iter().map(|measure| id(measure)).collect(),
        keys: by.iter().map(|key| ask_key(key)).collect(),
        filter: Vec::new(),
        from: 0,
        to: RANGE,
    }
}

fn ask_key(name: &str) -> AskKey {
    match name {
        "bucket" => AskKey::Bucket,
        _ => AskKey::Key(id(name)),
    }
}

/// The fact's row count over the whole range.
fn count(app: &App, fact: &str) -> Result<Value, AnswerError> {
    let answered = app.ask(&ask(fact, &[fact], &[]))?;
    Ok(answered.answer.rows[0].values[0].clone())
}

fn reference_answer(db: &Database, ask: &Ask) -> Answer {
    db.store_at(&spec(), db.lsn()).ask(ask).unwrap()
}

/// The rows an ask by borough answers.
/// An answer row of one key and one count.
fn row(key: Datum, n: i128) -> AnswerRow {
    AnswerRow {
        keys: vec![key],
        values: vec![Value::Int(n)],
    }
}

fn by_borough(app: &App, fact: &str) -> Vec<AnswerRow> {
    let by_borough = ask(fact, &[fact], &["borough"]);
    app.ask(&by_borough).unwrap().answer.rows
}

#[test]
fn starts_from_one_snapshot() {
    let (db, mut writer) = setup(&TABLES);
    let lsn = db
        .borrow_mut()
        .commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10), login(1, 1)]);

    let before = SystemTime::now();
    assert_eq!(writer.step().unwrap(), Step::Started(lsn));
    assert!(writer.app().status().ready);
    // Dims load first, so the facts' rows find them when asked.
    assert_eq!(db.borrow().loads, ["zones", "trips", "logins"]);
    let answered = writer
        .app()
        .ask(&ask("trips", &["trips", "fare"], &["borough"]))
        .unwrap();
    assert_eq!(answered.applied, lsn);
    // Caught up when the WAL was read after the load.
    let slack = Duration::from_millis(1);
    assert!(answered.as_of + slack >= before, "{:?}", answered.as_of);
    assert!(answered.as_of <= SystemTime::now(), "{:?}", answered.as_of);
    assert_eq!(
        answered.answer.rows,
        [AnswerRow {
            keys: vec![text("Bronx")],
            values: vec![Value::Int(1), Value::Int(10)]
        }]
    );
}

fn by_kind(value: Literal) -> Ask {
    Ask {
        filter: vec![Condition {
            key: id("kind"),
            values: vec![value],
        }],
        ..ask("trips", &["trips"], &[])
    }
}

#[test]
fn a_value_of_another_type_is_refused_once_loaded() {
    let (_db, mut writer) = setup(&TABLES);
    // Only a load knows the types.
    assert_eq!(
        writer.app().ask(&by_kind(Literal::Text("1".into()))),
        Err(AnswerError::NotReady)
    );
    writer.step().unwrap();
    assert_eq!(
        writer.app().ask(&by_kind(Literal::Text("1".into()))),
        Err(AnswerError::Invalid(vec![AskError::WrongType {
            key: id("kind"),
            column: ColumnType::Int,
            value: Literal::Text("1".into()),
        }]))
    );
    assert!(writer.app().ask(&by_kind(Literal::Int(1))).is_ok());
}

#[test]
fn not_ready_before_the_first_step() {
    let (_db, writer) = setup(&TABLES);
    assert!(!writer.app().status().ready);
    assert_eq!(count(writer.app(), "trips"), Err(AnswerError::NotReady));
    // A wrong ask is refused for what it is, ready or not.
    assert_eq!(
        writer.app().ask(&ask("trips", &["nope"], &[])),
        Err(AnswerError::Invalid(vec![AskError::UnknownMeasure(id(
            "nope"
        ))]))
    );
}

fn held_key(name: &str, column_type: ColumnType) -> HeldKey {
    HeldKey {
        name: id(name),
        column_type,
    }
}

#[test]
fn held_says_what_the_store_holds_once_loaded() {
    let (db, mut writer) = setup(&TABLES);
    assert_eq!(writer.app().held(), None);

    (db.borrow_mut()).commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10)]);
    writer.step().unwrap();
    // The trips join on a key of theirs, so bound cells would be as many
    // as their cells; the logins join on a column that is no key.
    let held = Held {
        views: vec![
            HeldView {
                name: id("trips"),
                widths: vec![Width { seconds: 3_600 }],
                bucket: ColumnType::Time { zoned: true },
                keys: vec![
                    held_key("kind", ColumnType::Int),
                    held_key("zone_id", ColumnType::Int),
                    held_key("borough", ColumnType::Text),
                ],
                measures: vec![id("trips"), id("fare"), id("fare_max"), id("riders")],
                cells: 1,
                bound_cells: None,
                range: Some(0..3_600),
            },
            HeldView {
                name: id("logins"),
                widths: vec![Width { seconds: 86_400 }],
                bucket: ColumnType::Time { zoned: true },
                keys: vec![held_key("borough", ColumnType::Text)],
                measures: vec![id("logins")],
                cells: 0,
                bound_cells: Some(0),
                range: None,
            },
        ],
        dims: vec![HeldDim {
            table: Table {
                schema: id("public"),
                name: id("zones"),
            },
            rows: 1,
        }],
    };
    assert_eq!(writer.app().held(), Some(held));

    // A failure leaves a store no ask reads, so nothing is held.
    db.borrow_mut().drop_connection = true;
    writer.step().unwrap_err();
    assert_eq!(writer.app().held(), None);
}

fn one_trip(kind: i64, fare: i128) -> AnswerRow {
    AnswerRow {
        keys: vec![int(0), int(kind), int(1), text("Bronx")],
        values: [1, fare, fare, 1].map(Value::Int).to_vec(),
    }
}

#[test]
fn the_rows_ask_reads_the_whole_view() {
    let (db, mut writer) = setup(&TABLES);
    let app = writer.app().clone();
    assert_eq!(app.rows_ask("zones"), None);
    assert_eq!(app.rows_ask("nope"), None);
    // Before a load the view holds no bucket; the ask is still a whole one.
    let empty = app.rows_ask("logins").unwrap();
    assert_eq!((empty.from, empty.to), (0, DAY));
    assert_eq!(app.ask(&empty), Err(AnswerError::NotReady));

    (db.borrow_mut()).commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10), trip(2, 2, 1, 5)]);
    writer.step().unwrap();
    let rows = app.rows_ask("trips").unwrap();
    assert_eq!(
        rows,
        Ask {
            from: 0,
            to: 3_600,
            ..ask(
                "trips",
                &["trips", "fare", "fare_max", "riders"],
                &["bucket", "kind", "zone_id", "borough"]
            )
        }
    );
    assert_eq!(
        app.ask(&rows).unwrap().answer.rows,
        [one_trip(1, 10), one_trip(2, 5)]
    );
    assert_eq!(app.ask(&empty).unwrap().answer.rows, []);
}

#[test]
fn a_failed_start_is_tried_again() {
    let (db, mut writer) = setup(&TABLES);
    db.borrow_mut().drop_connection = true;
    assert_eq!(writer.step().unwrap_err().to_string(), "connection dropped");
    assert!(!writer.app().status().ready);
    assert_eq!(writer.step().unwrap(), Step::Started(Lsn(100)));
    assert!(writer.app().status().ready);
}

#[test]
fn a_dropped_stream_starts_over() {
    let (db, mut writer) = setup(&TABLES);
    writer.step().unwrap();
    let first = db.borrow_mut().commit(vec![trip(1, 1, 1, 10)]);
    assert_eq!(writer.step().unwrap(), Step::Applied(first));
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(1)));

    let second = db.borrow_mut().commit(vec![trip(2, 1, 1, 10)]);
    db.borrow_mut().drop_connection = true;
    assert!(writer.step().is_err());
    assert!(!writer.app().status().ready);
    assert_eq!(count(writer.app(), "trips"), Err(AnswerError::NotReady));

    // The new stream starts after the second commit, and the new load counts
    // the first trip once.
    assert_eq!(writer.step().unwrap(), Step::Started(second));
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(2)));
}

#[test]
fn fact_rows_change_through_the_old_row() {
    let (db, mut writer) = setup(&TABLES);
    db.borrow_mut()
        .commit(vec![trip(1, 1, 1, 10), trip(2, 1, 1, 20)]);
    writer.step().unwrap();
    db.borrow_mut().commit(vec![
        Write::Update {
            table: "trips",
            key: int(1),
            new_key: int(1),
            row: vec![int(0), int(2), int(1), int(30), text("bo")],
        },
        Write::Delete {
            table: "trips",
            key: int(2),
        },
    ]);
    writer.step().unwrap();

    assert_eq!(db.borrow().loads.len(), 3, "no reload");
    let by_kind = ask("trips", &["trips", "fare", "riders"], &["kind"]);
    let answered = writer.app().ask(&by_kind).unwrap();
    assert_eq!(
        answered.answer.rows,
        [AnswerRow {
            keys: vec![int(2)],
            values: vec![Value::Int(1), Value::Int(30), Value::Int(1)]
        }]
    );
    assert_eq!(answered.answer, reference_answer(&db.borrow(), &by_kind));
}

#[test]
fn fact_change_without_the_old_row_starts_over() {
    let (db, mut writer) = setup(&TABLES);
    db.borrow_mut().commit(vec![login(1, 1), login(2, 1)]);
    writer.step().unwrap();

    db.borrow_mut().commit(vec![Write::Delete {
        table: "logins",
        key: int(1),
    }]);
    assert_eq!(
        writer.step().unwrap_err().to_string(),
        "a change no longer fits logins"
    );
    assert!(!writer.app().status().ready);
    let lsn = db.borrow().lsn();
    assert_eq!(writer.step().unwrap(), Step::Started(lsn));
    assert_eq!(db.borrow().loads[3..], ["zones", "trips", "logins"]);
    assert_eq!(count(writer.app(), "logins"), Ok(Value::Int(1)));

    db.borrow_mut().commit(vec![Write::Update {
        table: "logins",
        key: int(2),
        new_key: int(2),
        row: vec![int(DAY), int(2)],
    }]);
    assert!(writer.step().is_err());
    writer.step().unwrap();
    let by_bucket = ask("logins", &["logins"], &["bucket", "borough"]);
    assert_eq!(
        writer.app().ask(&by_bucket).unwrap().answer,
        reference_answer(&db.borrow(), &by_bucket)
    );
}

/// A cell is keyed by the join column, so a dim change is one row in the
/// dim's projection and the next ask reads the dim as it is: no cell
/// moves, nothing starts over.
#[test]
fn dim_changes_show_at_once() {
    let (db, mut writer) = setup(&TABLES);
    db.borrow_mut()
        .commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10), trip(2, 1, 1, 20)]);
    writer.step().unwrap();
    assert_eq!(by_borough(writer.app(), "trips"), [row(text("Bronx"), 2)]);

    let lsn = db.borrow_mut().commit(vec![Write::Update {
        table: "zones",
        key: int(1),
        new_key: int(1),
        row: vec![int(1), text("Queens")],
    }]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    assert_eq!(by_borough(writer.app(), "trips"), [row(text("Queens"), 2)]);
    assert_eq!(
        writer
            .app()
            .ask(&ask("trips", &["trips"], &["borough"]))
            .unwrap()
            .answer,
        reference_answer(&db.borrow(), &ask("trips", &["trips"], &["borough"]))
    );

    // A new row and a changed one join the dim as it is.
    db.borrow_mut().commit(vec![trip(3, 1, 1, 30)]);
    writer.step().unwrap();
    assert_eq!(by_borough(writer.app(), "trips"), [row(text("Queens"), 3)]);
    let lsn = db.borrow_mut().commit(vec![Write::Update {
        table: "trips",
        key: int(1),
        new_key: int(1),
        row: vec![int(0), int(1), int(1), int(11), text("ann")],
    }]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    assert_eq!(by_borough(writer.app(), "trips"), [row(text("Queens"), 3)]);
    assert_eq!(db.borrow().loads.len(), 3, "no reload");

    // A dim key moved: the rows that pointed at the old key read null.
    let lsn = db.borrow_mut().commit(vec![Write::Update {
        table: "zones",
        key: int(1),
        new_key: int(9),
        row: vec![int(9), text("Queens")],
    }]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    assert_eq!(by_borough(writer.app(), "trips"), [row(Datum::Null, 3)]);
}

/// A second dim row under a key Straum holds is a change like any other:
/// the join finds both, so the trips count once under each borough, as
/// PostgreSQL's join would repeat them.
#[test]
fn a_dim_key_held_twice_joins_twice() {
    let (db, mut writer) = setup(&TABLES);
    db.borrow_mut()
        .commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10), trip(2, 1, 1, 20)]);
    writer.step().unwrap();
    assert_eq!(by_borough(writer.app(), "trips"), [row(text("Bronx"), 2)]);

    let lsn = db.borrow_mut().send(vec![Change::Insert {
        table: id("zones"),
        row: vec![int(1), text("Staten Island")],
    }]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    assert_eq!(
        by_borough(writer.app(), "trips"),
        [row(text("Bronx"), 2), row(text("Staten Island"), 2)]
    );
    let lsn = db.borrow_mut().send(vec![Change::Delete {
        table: id("zones"),
        old: vec![int(1), text("Bronx")],
    }]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    assert_eq!(
        by_borough(writer.app(), "trips"),
        [row(text("Staten Island"), 2)]
    );
    assert_eq!(db.borrow().loads.len(), 3, "no reload");
}

/// A row whose dim row does not exist reads null, as the view's `LEFT
/// OUTER JOIN` gives, until the dim row comes; a dim row deleted or
/// truncated leaves null for every row that pointed at it.
#[test]
fn a_missing_dim_row_reads_null_until_it_comes() {
    let (db, mut writer) = setup(&TABLES);
    db.borrow_mut().commit(vec![trip(1, 1, 2, 10)]);
    writer.step().unwrap();
    assert_eq!(by_borough(writer.app(), "trips"), [row(Datum::Null, 1)]);

    db.borrow_mut()
        .commit(vec![zone(2, "Queens"), trip(2, 1, 2, 20)]);
    writer.step().unwrap();
    assert_eq!(by_borough(writer.app(), "trips"), [row(text("Queens"), 2)]);

    db.borrow_mut().commit(vec![
        Write::Delete {
            table: "zones",
            key: int(2),
        },
        trip(3, 1, 2, 30),
    ]);
    writer.step().unwrap();
    assert_eq!(by_borough(writer.app(), "trips"), [row(Datum::Null, 3)]);
    assert_eq!(db.borrow().loads.len(), 3, "no reload");
}

#[test]
fn truncate_empties_the_table() {
    let (db, mut writer) = setup(&TABLES);
    db.borrow_mut()
        .commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10), trip(2, 1, 1, 10)]);
    writer.step().unwrap();
    db.borrow_mut().commit(vec![
        Write::Truncate { table: "trips" },
        Write::Truncate { table: "zones" },
        trip(3, 1, 1, 10),
    ]);
    writer.step().unwrap();

    assert_eq!(db.borrow().loads.len(), 3, "no reload");
    let by_borough = ask("trips", &["trips"], &["borough"]);
    let answer = writer.app().ask(&by_borough).unwrap().answer;
    assert_eq!(answer.rows, [row(Datum::Null, 1)]);
    assert_eq!(answer, reference_answer(&db.borrow(), &by_borough));
}

#[test]
fn altered_table_starts_over() {
    let (db, mut writer) = setup(&TABLES);
    db.borrow_mut().commit(vec![trip(1, 1, 1, 10)]);
    writer.step().unwrap();
    db.borrow_mut().commit(vec![
        Write::Alter { table: "trips" },
        trip(2, 1, 1, 10),
        Write::Alter { table: "trips" },
    ]);
    assert!(matches!(writer.step(), Err(Error::Unfit(table)) if table == id("trips")));
    writer.step().unwrap();
    assert_eq!(db.borrow().loads.len(), 6);
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(2)));

    // A dim altered starts over too: Straum holds its rows.
    db.borrow_mut()
        .commit(vec![Write::Alter { table: "zones" }]);
    assert!(matches!(writer.step(), Err(Error::Unfit(table)) if table == id("zones")));
    writer.step().unwrap();
    assert_eq!(db.borrow().loads.len(), 9);
}

/// The insert before the unfit change is in the store, and not in any
/// answer: the stream is cleared before the write lock drops. The next
/// start loads the commit whole, from the snapshot.
#[test]
fn a_commit_that_stops_fitting_is_not_answered() {
    let (db, mut writer) = setup(&TABLES);
    db.borrow_mut().commit(vec![trip(1, 1, 1, 10)]);
    writer.step().unwrap();
    db.borrow_mut()
        .commit(vec![trip(2, 1, 1, 20), Write::Alter { table: "trips" }]);
    assert!(matches!(writer.step(), Err(Error::Unfit(table)) if table == id("trips")));
    assert_eq!(count(writer.app(), "trips"), Err(AnswerError::NotReady));
    assert_eq!(writer.app().status().applied, None);
    writer.step().unwrap();
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(2)));
    assert!(writer.app().status().ready);
}

#[test]
fn tables_outside_the_spec_are_skipped() {
    let (db, mut writer) = setup(&TABLES);
    writer.step().unwrap();
    let lsn = db
        .borrow_mut()
        .commit(vec![insert("audit", 1, vec![text("x")])]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    assert_eq!(db.borrow().loads.len(), 3, "no reload");
    assert!(writer.app().status().ready);
}

/// Asks the app from another thread, so a writer that holds the lock while
/// it loads fails the test instead of hanging it.
fn aside(app: &App, ask: Ask) -> Result<Answered, AnswerError> {
    let (tx, rx) = mpsc::channel();
    let app = app.clone();
    thread::spawn(move || tx.send(app.ask(&ask)));
    rx.recv_timeout(Duration::from_secs(10))
        .expect("the writer held the store while it loaded")
}

#[test]
fn asks_wait_while_the_tables_load() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let log = Rc::clone(&seen);
    let app = App::new(spec(), LIMITS, None);
    let (db, mut writer) = watched(app, move |app, table| {
        log.borrow_mut().push(DuringLoad {
            table: table.to_owned(),
            trips: count_aside(app, "trips"),
            logins: count_aside(app, "logins"),
        });
    });
    db.borrow_mut()
        .commit(vec![login(1, 1), login(2, 1), trip(1, 1, 1, 10)]);
    writer.step().unwrap();
    assert_eq!(
        *seen.borrow(),
        [not_ready("zones"), not_ready("trips"), not_ready("logins")]
    );
}

#[derive(Debug, PartialEq, Eq)]
struct DuringLoad {
    table: String,
    trips: Result<Value, AnswerError>,
    logins: Result<Value, AnswerError>,
}

fn not_ready(table: &str) -> DuringLoad {
    DuringLoad {
        table: table.to_owned(),
        trips: Err(AnswerError::NotReady),
        logins: Err(AnswerError::NotReady),
    }
}

fn count_aside(app: &App, fact: &str) -> Result<Value, AnswerError> {
    let answered = aside(app, ask(fact, &[fact], &[]))?;
    Ok(answered.answer.rows[0].values[0].clone())
}

fn wal(written: u64, confirmed: u64) -> Wal {
    Wal {
        written: Lsn(written),
        confirmed: Lsn(confirmed),
        restart: Lsn(confirmed),
    }
}

#[test]
fn ready_only_within_the_lag_bound() {
    let limits = Limits {
        max_lag: Duration::ZERO,
        ..LIMITS
    };
    let app = App::new(spec(), limits, None);
    // Loading takes time, so WAL written meanwhile has an age.
    let (db, mut writer) = watched(app, |_, _| thread::sleep(Duration::from_millis(1)));
    assert_eq!(writer.app().status().lag, None);

    // WAL written during the load that the slot hasn't confirmed.
    db.borrow_mut().wal = Some(wal(200, 100));
    assert_eq!(writer.step().unwrap(), Step::Started(Lsn(100)));
    assert!(writer.app().status().lag.unwrap() >= Duration::from_millis(2));
    assert!(!writer.app().status().ready);
    // Answers are as of before the load, not the reading after it.
    let behind = writer.app().ask(&ask("trips", &["trips"], &[])).unwrap();
    assert!(behind.as_of + Duration::from_millis(2) <= SystemTime::now());
    assert_eq!(writer.app().status().last_failure, None);
    // Asks still answer, at the LSN they reflect.
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(0)));

    // While it waits a second for a commit, Straum reads the WAL again and
    // finds the slot caught up.
    db.borrow_mut().wal = Some(wal(200, 200));
    db.borrow_mut().waits = 1;
    let lsn = db.borrow_mut().commit(vec![trip(1, 1, 1, 10)]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    assert_eq!(writer.app().status().lag, Some(Duration::ZERO));
    assert!(writer.app().status().ready);
    let caught_up = writer.app().ask(&ask("trips", &["trips"], &[])).unwrap();
    assert!(caught_up.as_of >= behind.as_of + Duration::from_secs(1));

    // A failure forgets the lag along with the tables.
    db.borrow_mut().drop_connection = true;
    db.borrow_mut().commit(vec![trip(2, 1, 1, 10)]);
    writer.step().unwrap_err();
    assert_eq!(writer.app().status().lag, None);
}

#[test]
fn status_reports_what_straum_did() {
    let (db, mut writer) = setup(&TABLES);
    status_before_the_first_step(writer.app());
    let start = status_after_a_start(&db, &mut writer);
    status_while_waiting_for_a_commit(&db, &mut writer, start);
    let failed = status_after_a_failure(&db, &mut writer);
    status_after_the_next_start(&db, &mut writer, &failed);
}

fn status_before_the_first_step(app: &App) {
    assert_eq!(
        app.status(),
        Status {
            ready: false,
            applied: None,
            committed: None,
            as_of: None,
            lag: None,
            wal: None,
            limits: LIMITS,
            last_failure: None,
            starts: 0,
            failures: 0,
            sealed: None,
            unseals: 0,
        }
    );
}

fn status_after_a_start(db: &RefCell<Database>, writer: &mut Writer<Fake>) -> Lsn {
    let start = db.borrow_mut().commit(vec![trip(1, 1, 1, 10)]);
    writer.step().unwrap();
    let status = writer.app().status();
    assert!(status.ready);
    assert_eq!((status.applied, status.committed), (Some(start), None));
    // Answers say the same.
    let answered = writer.app().ask(&ask("trips", &["trips"], &[])).unwrap();
    let apart = match answered.as_of.duration_since(status.as_of.unwrap()) {
        Ok(later) => later,
        Err(earlier) => earlier.duration(),
    };
    assert!(apart < Duration::from_millis(1), "{apart:?}");
    assert_eq!(status.lag, Some(Duration::ZERO));
    let caught_up = Wal {
        written: start,
        confirmed: start,
        restart: start,
    };
    assert_eq!(status.wal, Some(caught_up));
    assert_eq!((status.starts, status.failures), (1, 0));
    start
}

// A reading while it waits for the next commit.
fn status_while_waiting_for_a_commit(
    db: &RefCell<Database>,
    writer: &mut Writer<Fake>,
    start: Lsn,
) {
    let held = Wal {
        written: Lsn(start.0 + 50),
        confirmed: start,
        restart: Lsn(start.0 - 20),
    };
    db.borrow_mut().wal = Some(held);
    db.borrow_mut().waits = 1;
    let lsn = db.borrow_mut().commit(vec![trip(2, 1, 1, 10)]);
    writer.step().unwrap();
    let status = writer.app().status();
    assert_eq!(
        (status.applied, status.committed),
        (Some(lsn), Some(time(lsn)))
    );
    assert_eq!(status.wal, Some(held));
}

fn status_after_a_failure(db: &RefCell<Database>, writer: &mut Writer<Fake>) -> Status {
    db.borrow_mut().drop_connection = true;
    db.borrow_mut().commit(vec![trip(3, 1, 1, 10)]);
    writer.step().unwrap_err();
    let status = writer.app().status();
    assert!(!status.ready);
    assert_eq!((status.applied, status.lag, status.wal), (None, None, None));
    assert_eq!(status.as_of, None);
    let failure = status.last_failure.clone().unwrap();
    assert_eq!(failure.error, "connection dropped");
    assert_eq!((status.starts, status.failures), (1, 1));
    status
}

// The next start keeps the last failure.
fn status_after_the_next_start(db: &RefCell<Database>, writer: &mut Writer<Fake>, failed: &Status) {
    db.borrow_mut().wal = None;
    writer.step().unwrap();
    let after = writer.app().status();
    assert!(after.ready);
    assert_eq!(after.last_failure, failed.last_failure);
    assert_eq!((after.starts, after.failures), (2, 1));
}

#[test]
fn too_much_wal_starts_over() {
    let (db, mut writer) = setup(&TABLES);
    let limit = LIMITS.max_retained_wal;
    let message = "PostgreSQL holds 5120.0 MiB of WAL for Straum's slot, \
                   past the limit of 4096.0 MiB";

    db.borrow_mut().wal = Some(wal(100 + 5 * GIB, 100));
    let err = writer.step().unwrap_err();
    assert!(
        matches!(err, Error::Retained { retained, limit } if retained == 5 * GIB && limit == 4 * GIB)
    );
    assert_eq!(err.to_string(), message);
    assert!(!writer.app().status().ready);
    assert_eq!(writer.app().status().last_failure.unwrap().error, message);

    // Up to the limit is fine.
    db.borrow_mut().wal = Some(wal(100 + limit, 100));
    assert_eq!(writer.step().unwrap(), Step::Started(Lsn(100)));
    assert!(writer.app().status().ready);

    // Past it while streaming, at the next reading.
    db.borrow_mut().wal = Some(wal(100 + 5 * GIB, 100));
    db.borrow_mut().waits = 1;
    db.borrow_mut().commit(vec![trip(1, 1, 1, 10)]);
    assert_eq!(writer.step().unwrap_err().to_string(), message);
    assert!(!writer.app().status().ready);
}

#[test]
fn a_row_that_does_not_fit_stops_straum() {
    let (db, mut writer) = setup(&TABLES);
    writer.step().unwrap();
    let mut row = vec![int(0), int(1), int(1), text("ten"), text("ann")];
    db.borrow_mut()
        .commit(vec![insert("trips", 1, row.clone())]);

    // The insert fails, and so does loading trips again.
    let err = writer.step().unwrap_err();
    assert_eq!(err.to_string(), "a change no longer fits trips");
    let err = writer.step().unwrap_err();
    assert_eq!(err.to_string(), "column fare is not an integer");
    assert!(!writer.app().status().ready);

    // Once the row is fixed, Straum starts again.
    row[3] = int(10);
    let lsn = db.borrow_mut().commit(vec![Write::Update {
        table: "trips",
        key: int(1),
        new_key: int(1),
        row,
    }]);
    assert_eq!(writer.step().unwrap(), Step::Started(lsn));
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(1)));
}

fn trip_row() -> Row {
    vec![int(0), int(1), int(1), int(10), text("ann")]
}

fn never_added() -> Row {
    vec![int(0), int(9), int(9), int(9), text("zed")]
}

#[test]
fn a_change_that_does_not_fit_starts_over() {
    let cases = [
        (
            "fact update from a row Straum never saw",
            Change::Update {
                table: id("trips"),
                old: never_added(),
                new: trip_row(),
            },
        ),
        // The old row is already out of its cell when the new one fails.
        (
            "fact update to a row of the wrong width",
            Change::Update {
                table: id("trips"),
                old: trip_row(),
                new: vec![int(0)],
            },
        ),
        (
            "fact delete of a row Straum never saw",
            Change::Delete {
                table: id("trips"),
                old: never_added(),
            },
        ),
        (
            "fact insert of the wrong width",
            Change::Insert {
                table: id("trips"),
                row: vec![int(0)],
            },
        ),
        (
            "dim delete of a row Straum never held",
            Change::Delete {
                table: id("zones"),
                old: vec![int(7), text("Staten Island")],
            },
        ),
        (
            "dim insert of the wrong width",
            Change::Insert {
                table: id("zones"),
                row: vec![int(3)],
            },
        ),
    ];
    for (name, change) in cases {
        starts_over_after(name, change);
    }
}

fn starts_over_after(name: &str, change: Change) {
    let (db, mut writer) = setup(&TABLES);
    db.borrow_mut()
        .commit(vec![zone(1, "Bronx"), zone(2, "Queens"), trip(1, 1, 1, 10)]);
    writer.step().unwrap();
    let table = change.table().clone();
    let lsn = db.borrow_mut().send(vec![change]);

    let err = writer.step().unwrap_err();
    assert!(
        matches!(&err, Error::Unfit(unfit) if *unfit == table),
        "{name}"
    );
    assert!(!writer.app().status().ready, "{name}");
    assert_eq!(writer.step().unwrap(), Step::Started(lsn), "{name}");
    let by_borough = ask("trips", &["trips", "fare"], &["borough"]);
    assert_eq!(
        writer.app().ask(&by_borough).unwrap().answer,
        reference_answer(&db.borrow(), &by_borough),
        "{name}"
    );
}

/// Long enough that no bucket here is old enough to seal.
const NEVER: Duration = Duration::from_secs(1_000 * 365 * DAY as u64);

/// A writer that seals buckets once they ended `seal_after` ago.
fn spilling(seal_after: Duration) -> (Rc<RefCell<Database>>, Writer<Fake>, Arc<MemorySpill>) {
    let db = Database::new(&TABLES).shared();
    let spill = Arc::new(MemorySpill::default());
    let spilling = Spilling {
        spill: spill.clone(),
        seal_after,
    };
    let app = App::new(spec(), LIMITS, Some(spilling));
    (Rc::clone(&db), Writer::new(app, Fake::new(&db)), spill)
}

fn hour_of_trips() -> SealedId {
    SealedId {
        fact: id("trips"),
        width: 3_600,
        bucket_start: 0,
    }
}

fn day_of_logins() -> SealedId {
    SealedId {
        fact: id("logins"),
        width: DAY,
        bucket_start: 0,
    }
}

/// A spilling writer started on a trip and a login in the Bronx.
fn started(seal_after: Duration) -> (Rc<RefCell<Database>>, Writer<Fake>, Arc<MemorySpill>) {
    let (db, mut writer, spill) = spilling(seal_after);
    db.borrow_mut()
        .commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10), login(1, 1)]);
    writer.step().unwrap();
    (db, writer, spill)
}

#[test]
fn old_buckets_are_sealed_and_answer_as_before() {
    let (db, writer, spill) = started(Duration::ZERO);
    assert_eq!(spill.ids(), [day_of_logins(), hour_of_trips()]);
    for fact in ["trips", "logins"] {
        assert_eq!(
            by_borough(writer.app(), fact),
            reference_answer(&db.borrow(), &ask(fact, &[fact], &["borough"])).rows,
            "{fact}"
        );
    }
    assert_eq!(spill.gets(), 2);
}

#[test]
fn buckets_younger_than_seal_after_stay() {
    let (_db, writer, spill) = started(NEVER);
    assert_eq!(spill.ids(), []);
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(1)));
    assert_eq!(spill.gets(), 0);
}

#[test]
fn a_change_unseals_the_buckets_it_falls_in() {
    let (db, mut writer, spill) = started(Duration::ZERO);
    let lsn = db.borrow_mut().commit(vec![trip(2, 1, 1, 5)]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    // Back in memory until the writer next seals.
    assert_eq!(spill.ids(), [day_of_logins()]);
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(2)));
    let lsn = db.borrow_mut().commit(vec![Write::Delete {
        table: "trips",
        key: int(1),
    }]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(1)));
    assert_eq!(count(writer.app(), "logins"), Ok(Value::Int(1)));
}

#[test]
fn each_start_empties_the_spill() {
    let (db, mut writer, spill) = spilling(NEVER);
    (spill.buckets.lock().unwrap()).insert(hour_of_trips(), stale_bucket());
    writer.step().unwrap();
    assert_eq!(spill.ids(), []);
    db.borrow_mut().drop_connection = true;
    db.borrow_mut().commit(vec![trip(1, 1, 1, 10)]);
    assert!(writer.step().is_err());
    writer.step().unwrap();
    assert_eq!(spill.clears(), 2);
}

/// The old store goes before the new load, so the two never take memory
/// at once: while the tables load again, nothing of what the old store
/// had sealed is left to count.
#[test]
fn the_old_store_goes_before_the_new_load() {
    let spilling = Spilling {
        spill: Arc::new(MemorySpill::default()),
        seal_after: Duration::ZERO,
    };
    let seen = Rc::new(RefCell::new(Vec::new()));
    let log = Rc::clone(&seen);
    let app = App::new(spec(), LIMITS, Some(spilling));
    let (db, mut writer) = watched(app, move |app, _| {
        log.borrow_mut().push(app.status().sealed);
    });
    db.borrow_mut()
        .commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10), login(1, 1)]);
    writer.step().unwrap();
    assert_eq!(writer.app().status().sealed, Some(2));

    db.borrow_mut().drop_connection = true;
    writer.step().unwrap_err();
    // The failure leaves the store as it was until the next start.
    assert_eq!(writer.app().status().sealed, Some(2));
    seen.borrow_mut().clear();
    writer.step().unwrap();
    assert_eq!(*seen.borrow(), [Some(0); 3]);
    assert_eq!(writer.app().status().sealed, Some(2));
}

/// A bucket no store sealed: a cell keyed by codes it never gave.
fn stale_bucket() -> straum_domain::Sealed {
    straum_domain::Sealed {
        id: hour_of_trips(),
        keys: vec![7, 7],
        counts: vec![1],
        columns: Vec::new(),
    }
}

#[test]
fn a_truncate_removes_its_sealed_buckets_from_the_spill() {
    let (db, mut writer, spill) = started(Duration::ZERO);
    assert_eq!(writer.app().status().sealed, Some(2));
    db.borrow_mut()
        .commit(vec![Write::Truncate { table: "trips" }]);
    writer.step().unwrap();
    assert_eq!(spill.ids(), [day_of_logins()]);
    assert_eq!(writer.app().status().sealed, Some(1));
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(0)));
    assert_eq!(count(writer.app(), "logins"), Ok(Value::Int(1)));
}

#[test]
fn a_truncate_the_spill_cannot_take_starts_over_without_a_leak() {
    let (db, mut writer, spill) = started(Duration::ZERO);
    // A dim has no buckets: its truncate leaves the spill alone.
    db.borrow_mut()
        .commit(vec![Write::Truncate { table: "zones" }]);
    writer.step().unwrap();
    assert_eq!(spill.ids(), [day_of_logins(), hour_of_trips()]);

    db.borrow_mut()
        .commit(vec![Write::Truncate { table: "trips" }]);
    spill.break_it(true);
    assert!(matches!(writer.step(), Err(Error::Spill(_))));
    spill.break_it(false);
    writer.step().unwrap();
    // The start emptied the spill and sealed what the store holds now.
    assert_eq!(spill.ids(), [day_of_logins()]);
    assert_eq!(writer.app().status().sealed, Some(1));
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(0)));
}

#[test]
fn status_counts_sealed_buckets_and_unseals() {
    let (db, mut writer, _spill) = started(Duration::ZERO);
    let status = writer.app().status();
    assert_eq!((status.sealed, status.unseals), (Some(2), 0));
    db.borrow_mut().commit(vec![trip(2, 1, 1, 5), login(2, 1)]);
    writer.step().unwrap();
    let status = writer.app().status();
    assert_eq!((status.sealed, status.unseals), (Some(0), 2));
}

#[test]
fn a_spill_that_fails_starts_over() {
    let (db, mut writer, spill) = spilling(Duration::ZERO);
    db.borrow_mut()
        .commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10)]);
    spill.break_it(true);
    let err = writer.step().unwrap_err();
    assert!(matches!(err, Error::Spill(_)), "{err:?}");
    assert_eq!(err.to_string(), "spill: broken");
    assert!(!writer.app().status().ready);
    spill.break_it(false);
    writer.step().unwrap();
    assert_eq!(spill.ids(), [hour_of_trips()]);

    // Unsealing for a change fails too.
    db.borrow_mut().commit(vec![trip(2, 1, 1, 5)]);
    spill.break_it(true);
    assert!(matches!(writer.step(), Err(Error::Spill(_))));
    spill.break_it(false);
    writer.step().unwrap();
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(2)));
}

#[test]
fn an_ask_fails_when_a_sealed_bucket_cannot_be_read() {
    let (db, mut writer, spill) = spilling(Duration::ZERO);
    db.borrow_mut()
        .commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10)]);
    writer.step().unwrap();
    spill.break_it(true);
    assert_eq!(
        count(writer.app(), "trips"),
        Err(AnswerError::Spill("broken".into()))
    );
    spill.break_it(false);
    spill.buckets.lock().unwrap().clear();
    assert_eq!(
        count(writer.app(), "trips").unwrap_err().to_string(),
        "reading a sealed bucket failed: no trips bucket of 3600s at 0"
    );
    // An ask outside the sealed buckets needs none of them.
    let later = Ask {
        from: RANGE,
        to: 2 * RANGE,
        ..ask("trips", &["trips"], &[])
    };
    assert!(writer.app().ask(&later).is_ok());
}

#[test]
fn a_spilled_bucket_that_does_not_fit_starts_over() {
    let (db, mut writer, spill) = spilling(Duration::ZERO);
    db.borrow_mut()
        .commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10)]);
    writer.step().unwrap();
    spill
        .buckets
        .lock()
        .unwrap()
        .insert(hour_of_trips(), stale_bucket());
    let reason = "sealed trips bucket of 3600s at 0: a code the store never gave";
    let asked = count(writer.app(), "trips").unwrap_err();
    assert_eq!(asked, AnswerError::Spill(reason.into()));
    db.borrow_mut().commit(vec![trip(2, 1, 1, 5)]);
    let err = writer.step().unwrap_err();
    assert!(matches!(err, Error::Sealed(_)), "{err:?}");
    assert_eq!(err.to_string(), reason);
    // The start after holds every row again.
    writer.step().unwrap();
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(2)));
}

#[test]
fn lsn_display() {
    assert_eq!(Lsn(0x16_B374_D848).to_string(), "16/B374D848");
    assert_eq!(Lsn(0x1_0000_0000).to_string(), "1/0");
    assert_eq!(Lsn(0).to_string(), "0/0");
}

#[test]
fn answer_error_display() {
    assert_eq!(AnswerError::NotReady.to_string(), "not ready");
    let invalid = AnswerError::Invalid(vec![
        AskError::UnknownMeasure(id("nope")),
        AskError::NoMeasures,
    ]);
    assert_eq!(
        invalid.to_string(),
        "unknown measure nope\nno measures asked"
    );
}

// Random writes, dropped connections and restarts, the zones moving too:
// a fresh load at any LSN, dims and facts, is what Straum must hold.

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut bits = self.0;
        bits = (bits ^ (bits >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        bits = (bits ^ (bits >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        bits ^ (bits >> 31)
    }

    fn below(&mut self, bound: usize) -> usize {
        let bound = u64::try_from(bound).expect("a usize fits a u64");
        usize::try_from(self.next() % bound).expect("below a usize")
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn int(&mut self, low: i64, high: i64) -> i64 {
        let span = u64::try_from(high - low).expect("low below high");
        low + i64::try_from(self.next() % span).expect("below an i64 span")
    }

    /// An integer in `low..high`, or null one time in ten.
    fn maybe(&mut self, low: i64, high: i64) -> Datum {
        if self.chance(10) {
            Datum::Null
        } else {
            int(self.int(low, high))
        }
    }

    fn maybe_text(&mut self, choices: &[&str]) -> Datum {
        if self.chance(10) {
            Datum::Null
        } else {
            text(choices[self.below(choices.len())])
        }
    }

    fn pick<T: Clone>(&mut self, items: &[T]) -> Option<T> {
        (!items.is_empty()).then(|| items[self.below(items.len())].clone())
    }
}

fn random_trip(rng: &mut Rng) -> Row {
    vec![
        int(rng.int(0, RANGE)),
        rng.maybe(1, 4),
        // Zones 7 and 8 never exist, so their attributes are null.
        rng.maybe(1, 9),
        rng.maybe(-50, 200),
        rng.maybe_text(&RIDERS),
    ]
}

fn random_login(rng: &mut Rng) -> Row {
    vec![int(rng.int(0, RANGE)), rng.maybe(1, 9)]
}

/// One transaction's writes. `next_key` hands out fact keys.
fn random_writes(rng: &mut Rng, db: &Database, next_key: &mut i64) -> Vec<Write> {
    let mut transaction = Transaction::new(db, next_key);
    let mut writes = Vec::new();
    for _ in 0..=rng.below(4) {
        let Some(write) = transaction.random_write(rng) else {
            continue;
        };
        transaction.record(&write);
        writes.push(write);
    }
    writes
}

/// Keys as the transaction goes, so no write misses its row.
struct Transaction<'a> {
    keys: BTreeMap<&'static str, BTreeSet<Datum>>,
    next_key: &'a mut i64,
}

impl<'a> Transaction<'a> {
    fn new(db: &Database, next_key: &'a mut i64) -> Transaction<'a> {
        let mut keys = BTreeMap::new();
        for table in ["zones", "trips", "logins"] {
            keys.insert(table, db.keys(table).into_iter().collect());
        }
        Transaction { keys, next_key }
    }

    fn new_key(&mut self) -> Datum {
        *self.next_key += 1;
        int(*self.next_key)
    }

    fn existing(&self, table: &str) -> Vec<Datum> {
        self.keys[table].iter().cloned().collect()
    }

    /// `None` when the write it drew has no row to change.
    fn random_write(&mut self, rng: &mut Rng) -> Option<Write> {
        let write = match rng.below(100) {
            0..40 => Write::Insert {
                table: "trips",
                key: self.new_key(),
                row: random_trip(rng),
            },
            40..55 => self.trip_update(rng)?,
            55..65 => Write::Delete {
                table: "trips",
                key: rng.pick(&self.existing("trips"))?,
            },
            65..80 => Write::Insert {
                table: "logins",
                key: self.new_key(),
                row: random_login(rng),
            },
            // Logins send no old rows; these make Straum start over.
            80..84 => self.login_change(rng)?,
            84..87 => Write::Insert {
                table: "audit",
                key: self.new_key(),
                row: vec![text("x")],
            },
            // The zones move: relabelled, moved to a new key, gone, back.
            87..92 => self.zone_change(rng)?,
            92..95 => Write::Truncate {
                table: rng.pick(&["trips", "logins"]).unwrap(),
            },
            _ => Write::Alter {
                table: rng.pick(&["zones", "trips", "logins"]).unwrap(),
            },
        };
        Some(write)
    }

    fn trip_update(&mut self, rng: &mut Rng) -> Option<Write> {
        let key = rng.pick(&self.existing("trips"))?;
        let new_key = if rng.chance(20) {
            self.new_key()
        } else {
            key.clone()
        };
        Some(Write::Update {
            table: "trips",
            key,
            new_key,
            row: random_trip(rng),
        })
    }

    fn login_change(&self, rng: &mut Rng) -> Option<Write> {
        let key = rng.pick(&self.existing("logins"))?;
        if rng.chance(50) {
            return Some(Write::Delete {
                table: "logins",
                key,
            });
        }
        Some(Write::Update {
            table: "logins",
            new_key: key.clone(),
            key,
            row: random_login(rng),
        })
    }

    fn zone_change(&self, rng: &mut Rng) -> Option<Write> {
        let key = int(rng.int(1, 9));
        let borough = text(rng.pick(&BOROUGHS).unwrap());
        if !self.keys["zones"].contains(&key) {
            return Some(Write::Insert {
                table: "zones",
                row: vec![key.clone(), borough],
                key,
            });
        }
        if rng.chance(30) {
            return Some(Write::Delete {
                table: "zones",
                key,
            });
        }
        let new_key = if rng.chance(20) {
            int(rng.int(1, 9))
        } else {
            key.clone()
        };
        if new_key != key && self.keys["zones"].contains(&new_key) {
            return None;
        }
        Some(Write::Update {
            table: "zones",
            row: vec![new_key.clone(), borough],
            key,
            new_key,
        })
    }

    fn record(&mut self, write: &Write) {
        match write {
            // audit keeps no keys here.
            Write::Insert { table, key, .. } => {
                if let Some(held) = self.keys.get_mut(table) {
                    held.insert(key.clone());
                }
            }
            Write::Update {
                table,
                key,
                new_key,
                ..
            } => {
                let held = self.keys.get_mut(table).unwrap();
                held.remove(key);
                held.insert(new_key.clone());
            }
            Write::Delete { table, key } => {
                self.keys.get_mut(table).unwrap().remove(key);
            }
            Write::Truncate { table } => self.keys.get_mut(table).unwrap().clear(),
            Write::Alter { .. } => {}
        }
    }
}

fn random_ask(rng: &mut Rng) -> Ask {
    let (fact, measures, keys): (_, &[&str], &[&str]) = if rng.chance(70) {
        (
            "trips",
            &["trips", "fare", "fare_max", "riders"],
            &["kind", "zone_id", "borough"],
        )
    } else {
        ("logins", &["logins"], &["borough"])
    };
    let mut chosen: Vec<&str> = measures
        .iter()
        .copied()
        .filter(|_| rng.chance(50))
        .collect();
    if chosen.is_empty() {
        chosen.push(measures[0]);
    }
    let mut by: Vec<&str> = keys.iter().copied().filter(|_| rng.chance(40)).collect();
    if rng.chance(30) {
        by.push("bucket");
    }
    let mut ask = ask(fact, &chosen, &by);
    if rng.chance(30) {
        let values = BOROUGHS
            .iter()
            .filter(|_| rng.chance(50))
            .map(|borough| Literal::Text(borough.to_string()))
            .collect::<Vec<_>>();
        if !values.is_empty() {
            ask.filter.push(Condition {
                key: id("borough"),
                values,
            });
        }
    }
    // Sometimes one day of the range.
    if rng.chance(30) {
        let from = DAY * rng.int(0, 3);
        ask.from = from;
        ask.to = from + DAY;
    }
    ask
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Seen {
    answered: usize,
    failed: usize,
    /// Failures from a change that no longer fit.
    unfit: usize,
    /// Asks made while a table loaded.
    aside: usize,
}

/// Checks, in the middle of a load, that asks answer not ready instead of
/// waiting on the writer.
fn check_aside(app: &App) -> usize {
    for fact in ["trips", "logins"] {
        let ask = ask(fact, &[fact], &["borough"]);
        assert_eq!(aside(app, ask), Err(AnswerError::NotReady), "{fact}");
    }
    2
}

/// Asks against a fresh load of the database at `lsn`.
fn check(app: &App, db: &Database, lsn: Lsn, rng: &mut Rng, tally: &mut Seen) {
    let reference = db.store_at(&spec(), lsn);
    for _ in 0..4 {
        let ask = random_ask(rng);
        let answered = app
            .ask(&ask)
            .unwrap_or_else(|error| panic!("{error} at {lsn} for {ask:?}"));
        assert_eq!(answered.applied, lsn);
        assert_eq!(answered.answer, reference.ask(&ask).unwrap(), "{ask:?}");
        tally.answered += 1;
    }
}

/// Follows random writes, checking asks at every LSN. With a spill, every
/// bucket is sealed on each start, so changes unseal them as they come.
fn follow(seed: u64, tally: &mut Seen, spill: Option<&Arc<MemorySpill>>) {
    let mut rng = Rng(seed);
    let spilling = spill.map(|spill| Spilling {
        spill: Arc::clone(spill) as Arc<dyn Spill>,
        seal_after: Duration::ZERO,
    });
    let aside = Rc::new(RefCell::new(0));
    let count = Rc::clone(&aside);
    let app = App::new(spec(), LIMITS, spilling);
    let (db, mut writer) = watched(app, move |app, _| {
        *count.borrow_mut() += check_aside(app);
    });
    let zones: Vec<Write> = (1..=6)
        .map(|key| {
            zone(
                key,
                BOROUGHS[usize::try_from(key).unwrap() % BOROUGHS.len()],
            )
        })
        .collect();
    db.borrow_mut().commit(zones);
    let mut next_key = 0;
    let mut position = None;
    for _ in 0..150 {
        for _ in 0..rng.below(4) {
            let transaction = random_writes(&mut rng, &db.borrow(), &mut next_key);
            db.borrow_mut().commit(transaction);
        }
        if rng.chance(5) {
            db.borrow_mut().drop_connection = true;
        }
        let latest = db.borrow().lsn();
        let mut steps = 0;
        while position != Some(latest) {
            steps += 1;
            assert!(steps < 100, "seed {seed}: stuck");
            position = match writer.step() {
                Ok(Step::Started(lsn) | Step::Applied(lsn)) => Some(lsn),
                Err(error) => {
                    tally.failed += 1;
                    tally.unfit += usize::from(matches!(error, Error::Unfit(_)));
                    None
                }
            };
            if let Some(lsn) = position {
                check(writer.app(), &db.borrow(), lsn, &mut rng, tally);
            }
            // The spill holds what the store has sealed and no more. After
            // a failed step it may hold more, until the next start empties
            // it.
            if let (Some(spill), Some(_)) = (spill, position) {
                assert_eq!(
                    writer.app().status().sealed,
                    Some(spill.ids().len()),
                    "seed {seed}"
                );
            }
        }
        assert!(writer.app().status().ready, "seed {seed}");
    }
    tally.aside += *aside.borrow();
}

#[test]
fn follows_the_database() {
    let mut seen = Seen::default();
    let spill = Arc::new(MemorySpill::default());
    for seed in 0..16 {
        let (mut plain, mut spilled) = (Seen::default(), Seen::default());
        follow(seed, &mut plain, None);
        // Spilling changes no answer and adds no failure: each seed runs as
        // it does with every bucket in memory.
        follow(seed, &mut spilled, Some(&spill));
        assert_eq!(spilled, plain, "seed {seed}");
        seen.answered += plain.answered;
        seen.failed += plain.failed;
        seen.unfit += plain.unfit;
        seen.aside += plain.aside;
    }
    // The run went through every path it claims to test.
    assert!(seen.answered > 1_000, "{seen:?}");
    assert!(seen.failed > seen.unfit, "{seen:?}");
    assert!(seen.unfit > 0, "{seen:?}");
    assert!(seen.aside > 0, "{seen:?}");
    assert!(spill.gets() > 1_000, "{}", spill.gets());
}
