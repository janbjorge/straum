//! The first step: every table loaded from one snapshot, asks refused
//! until then, and what the app says it holds once loaded.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, SystemTime};

use straum_app::{AnswerError, App, Held, HeldDim, HeldKey, HeldView, Lsn, Step, ViewKey, Writer};
use straum_domain::{
    AnswerRow, Ask, AskError, ColumnType, Condition, Instead, Literal, Table, Value, Width,
};

use crate::common::{
    Harness, LIMITS, again, ask, count, count_aside, int, login, setup, spec, text, trip, watched,
    zone,
};
use crate::fake::{Database, Fake, Identity, Published, id};

#[test]
fn starts_from_one_snapshot() {
    let Harness {
        database,
        mut writer,
    } = setup();
    let lsn = database
        .borrow_mut()
        .commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10), login(1, 1)]);

    let before = SystemTime::now();
    assert_eq!(writer.step().unwrap(), Step::Started(lsn));
    assert!(writer.app().status().ready);
    // Dims load first, so the facts' rows find them when asked.
    assert_eq!(database.borrow().loads, ["zones", "trips", "logins"]);
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
    let Harness { mut writer, .. } = setup();
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
    let Harness { writer, .. } = setup();
    assert!(!writer.app().status().ready);
    assert_eq!(count(writer.app(), "trips"), Err(AnswerError::NotReady));
    // A wrong ask is refused for what it is, ready or not.
    assert_eq!(
        writer.app().ask(&ask("trips", &["nope"], &[])),
        Err(AnswerError::Invalid(vec![AskError::UnknownMeasure {
            name: id("nope"),
            instead: Instead::Every(vec![id("trips"), id("fare"), id("fare_max"), id("riders")])
        }]))
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
    let Harness {
        database,
        mut writer,
    } = setup();
    assert_eq!(writer.app().held(), None);

    (database.borrow_mut()).commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10)]);
    writer.step().unwrap();
    // The trips join on a key of theirs, so bound cells would be as many
    // as their cells; the logins join on a column that is no key.
    let held = Held {
        views: vec![
            HeldView {
                name: id("trips"),
                widths: vec![Width { seconds: 3_600 }],
                time: held_key("bucket", ColumnType::Time { zoned: true }),
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
                time: held_key("bucket", ColumnType::Time { zoned: true }),
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
            // Both views read the zone's borough; neither reads its id.
            asked_as: vec![
                ViewKey {
                    view: id("trips"),
                    key: id("borough"),
                },
                ViewKey {
                    view: id("logins"),
                    key: id("borough"),
                },
            ],
        }],
    };
    assert_eq!(writer.app().held(), Some(held));

    // A failure leaves a store no ask reads, so nothing is held.
    database.borrow_mut().drop_connection = true;
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
    let Harness {
        database,
        mut writer,
    } = setup();
    let app = writer.app().clone();
    assert_eq!(app.rows_ask("zones"), None);
    assert_eq!(app.rows_ask("nope"), None);
    // Both ends are open, so the ask reads whatever buckets are held.
    let empty = app.rows_ask("logins").unwrap();
    assert_eq!((empty.from, empty.to), (None, None));
    assert_eq!(app.ask(&empty), Err(AnswerError::NotReady));

    (database.borrow_mut()).commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10), trip(2, 2, 1, 5)]);
    writer.step().unwrap();
    let rows = app.rows_ask("trips").unwrap();
    assert_eq!(
        rows,
        Ask {
            from: None,
            to: None,
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

/// A start that fails leaves Straum not ready; a new writer, as Straum
/// started again, loads.
#[test]
fn a_failed_start_leaves_straum_not_ready() {
    let Harness { database, writer } = setup();
    let mut failed = writer;
    database.borrow_mut().drop_connection = true;
    assert_eq!(failed.step().unwrap_err().to_string(), "connection dropped");
    assert!(!failed.app().status().ready);
    let mut writer = again(&database);
    assert_eq!(writer.step().unwrap(), Step::Started(Lsn(100)));
    assert!(writer.app().status().ready);
    assert!(!failed.app().status().ready);
}

#[test]
fn asks_wait_while_the_tables_load() {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let log = Rc::clone(&seen);
    let app = App::new(spec(), LIMITS);
    let Harness {
        database,
        mut writer,
    } = watched(app, move |app, table| {
        log.borrow_mut().push(DuringLoad {
            table: table.to_owned(),
            trips: count_aside(app, "trips"),
            logins: count_aside(app, "logins"),
        });
    });
    database
        .borrow_mut()
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

thread_local! {
    /// What the writer under test reported, line by line.
    static REPORTED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn record(line: &str) {
    REPORTED.with_borrow_mut(|lines| lines.push(line.to_owned()));
}

/// A start says what it does as it goes: the stream it opened, then each
/// table loaded, dims first. A start that fails before the stream opens
/// says nothing.
#[test]
fn a_start_reports_each_table_it_loads() {
    let Harness { database, writer } = setup();
    let mut failed = writer.reporting(record);
    database.borrow_mut().drop_connection = true;
    assert!(failed.step().is_err());
    assert_eq!(REPORTED.with_borrow(Vec::len), 0);

    let mut writer = again(&database).reporting(record);
    let lsn = database.borrow().lsn();
    assert_eq!(writer.step().unwrap(), Step::Started(lsn));
    let lines = REPORTED.take();
    assert_eq!(lines.len(), 4, "{lines:?}");
    assert_eq!(
        lines[0],
        format!(
            "opened a stream at {lsn}; loading 3 tables, estimated 0 rows and 0.0 MiB: \
             public.zones (dim zones) 0 rows, 0.0 MiB, columns id, borough; \
             public.trips (view trips) 0 rows, 0.0 MiB, \
             columns at, kind, zone_id, fare, rider; \
             public.logins (view logins) 0 rows, 0.0 MiB, columns at, zone_id"
        )
    );
    let expected = [
        "loaded dim zones in ",
        "loaded view trips in ",
        "loaded view logins in ",
    ];
    for (line, start) in lines[1..].iter().zip(expected) {
        assert!(line.starts_with(start), "{line}");
        assert!(line.contains(" s: 0 rows, 0.0 MiB on disk"), "{line}");
    }
}

/// Two views of one fact table, `public.trips`.
const SHARED: &str = "
CREATE VIEW trips AS
SELECT buckets(t.at, '1 hour') AS bucket, count(*) AS trips
FROM public.trips AS t
GROUP BY 1;

CREATE VIEW fares AS
SELECT buckets(t.at, '1 day') AS bucket, t.kind AS kind, sum(t.fare) AS fare
FROM public.trips AS t
GROUP BY 1, 2;
";

/// The views of one fact table load together: the source is asked once
/// for both, and the start says so in one line.
#[test]
fn views_of_one_table_load_together() {
    let views = [
        Published {
            name: "trips",
            identity: Identity::Full,
        },
        Published {
            name: "fares",
            identity: Identity::Full,
        },
    ];
    let database = Database::new(&views).shared();
    let app = App::new(straum_notation::parse(SHARED).unwrap(), LIMITS);
    let mut writer = Writer::new(app, Fake::new(&database)).reporting(record);
    let lsn = database.borrow().lsn();
    assert_eq!(writer.step().unwrap(), Step::Started(lsn));
    let lines = REPORTED.take();
    assert_eq!(lines.len(), 2, "{lines:?}");
    let plan = "loading 1 table, estimated 0 rows and 0.0 MiB: \
                public.trips (views trips, fares) 0 rows, 0.0 MiB, columns at, kind, fare";
    assert_eq!(lines[0], format!("opened a stream at {lsn}; {plan}"));
    let loaded = &lines[1];
    assert!(
        loaded.starts_with("loaded views trips, fares in ") && loaded.contains(" s: 0 rows, "),
        "{loaded}"
    );
    assert_eq!(database.borrow().loads, ["trips", "fares"]);
}
