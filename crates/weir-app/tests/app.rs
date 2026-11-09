//! `App` against an in-memory PostgreSQL: hand-made cases, then random
//! writes, dropped connections and reloads checked against a fresh load of
//! the database at the same LSN.

mod fake;

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, UNIX_EPOCH};

use weir_app::{
    AnswerError, Answered, App, Change, Commit, Lsn, Old, Source, SourceError, Status, Step,
    TableStatus, Writer,
};
use weir_domain::{
    Answer, Ask, AskError, AskKey, Cond, Datum, Dim, DimTable, Fact, Ident, KeyPath, Literal,
    Rollup, Spec, Store, TimeKey, Value, Zone,
};

use fake::{Database, Fake, Row, Write, id, time};

const DAY: i64 = 86_400;
/// Every row's time falls in `[0, RANGE)`.
const RANGE: i64 = 3 * DAY;

const SPEC: &str = "
CREATE VIEW trips WITH (key = id) AS
SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket,
       t.kind,
       t.zone_id,
       zone.borough,
       count(*) AS trips,
       sum(t.fare) AS fare,
       max(t.fare) AS fare_max,
       count(DISTINCT t.rider) AS riders
FROM public.trips t
LEFT JOIN public.zones zone ON zone.id = t.zone_id
GROUP BY 1, 2, 3, 4;

CREATE VIEW logins WITH (key = id, append = true) AS
SELECT date_bin('1 day', l.at, timestamptz 'epoch') AS bucket,
       l.zone_id,
       count(*) AS logins
FROM public.logins l
LEFT JOIN public.zones zone ON zone.id = l.zone_id
GROUP BY 1, 2;
";

/// `audit` is published but not in the spec. Only trips has
/// `REPLICA IDENTITY FULL`.
const TABLES: [(&str, bool); 4] = [
    ("zones", false),
    ("trips", true),
    ("logins", false),
    ("audit", false),
];

const BOROUGHS: [&str; 3] = ["Bronx", "Brooklyn", "Queens"];
const RIDERS: [&str; 5] = ["ann", "bo", "cy", "di", "ed"];

fn spec() -> Spec {
    weir_notation::parse(SPEC).unwrap()
}

fn int(v: i64) -> Datum {
    Datum::Int(v)
}

fn text(s: &str) -> Datum {
    Datum::Text(s.into())
}

fn setup(tables: &[(&'static str, bool)]) -> (Rc<RefCell<Database>>, Writer<Fake>) {
    let db = Database::new(tables).shared();
    let writer = Writer::new(App::new(spec()), Fake::new(&db));
    (db, writer)
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
        fact: id(fact),
        measures: measures.iter().map(|m| id(m)).collect(),
        by: by.iter().map(|k| ask_key(k)).collect(),
        filter: Vec::new(),
        zone: Zone::utc(),
        from: 0,
        to: RANGE,
    }
}

fn ask_key(s: &str) -> AskKey {
    match s {
        "bucket" => AskKey::Time(TimeKey::Bucket),
        "hour" => AskKey::Time(TimeKey::Hour),
        "day" => AskKey::Time(TimeKey::Day),
        _ => AskKey::Key(path(s)),
    }
}

fn path(s: &str) -> KeyPath {
    match s.split_once('.') {
        Some((head, attr)) => KeyPath {
            head: id(head),
            attr: Some(id(attr)),
        },
        None => KeyPath {
            head: id(s),
            attr: None,
        },
    }
}

/// The fact's row count over the whole range.
fn count(app: &App, fact: &str) -> Result<Value, AnswerError> {
    let answered = app.ask(&ask(fact, &[fact], &[]))?;
    Ok(answered.answer.rows[0].1[0].clone())
}

/// A store loaded from the database as it was at `lsn`.
fn reference(db: &Database, lsn: Lsn) -> Store {
    let mut store = Store::new(spec());
    for row in db.rows_at(lsn, "zones") {
        store.dim("zones").unwrap().upsert(&row).unwrap();
    }
    for fact in ["trips", "logins"] {
        for row in db.rows_at(lsn, fact) {
            store.rollup(fact).unwrap().add(&row).unwrap();
        }
    }
    store
}

fn expected(db: &Database, ask: &Ask) -> Answer {
    reference(db, db.lsn()).ask(ask).unwrap()
}

#[test]
fn fake_rows_hold_the_rollup_columns() {
    let mut store = Store::new(spec());
    let mut columns = |fact| {
        let rollup = store.rollup(fact).unwrap();
        rollup
            .columns()
            .iter()
            .map(Ident::to_string)
            .collect::<Vec<_>>()
    };
    assert_eq!(columns("trips"), ["at", "kind", "zone_id", "fare", "rider"]);
    assert_eq!(columns("logins"), ["at", "zone_id"]);
}

#[test]
fn starts_from_one_snapshot() {
    let (db, mut writer) = setup(&TABLES);
    let lsn = db
        .borrow_mut()
        .commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10), login(1, 1)]);

    assert_eq!(writer.step().unwrap(), Step::Started(lsn));
    assert!(writer.app().ready());
    assert_eq!(db.borrow().loads, ["zones", "trips", "logins"]);
    let answered = writer
        .app()
        .ask(&ask("trips", &["trips", "fare"], &["zone.borough"]))
        .unwrap();
    assert_eq!(answered.lsn, lsn);
    assert_eq!(answered.time, None, "no commit since the start");
    assert_eq!(
        answered.answer.rows,
        [(vec![text("Bronx")], vec![Value::Int(1), Value::Int(10)])]
    );
}

#[test]
fn not_ready_before_the_first_step() {
    let (_db, writer) = setup(&TABLES);
    assert!(!writer.app().ready());
    assert_eq!(count(writer.app(), "trips"), Err(AnswerError::NotReady));
    // A wrong ask is refused for what it is, ready or not.
    assert_eq!(
        writer.app().ask(&ask("trips", &["nope"], &[])),
        Err(AnswerError::Invalid(vec![AskError::UnknownMeasure(id(
            "nope"
        ))]))
    );
}

#[test]
fn a_failed_start_is_tried_again() {
    let (db, mut writer) = setup(&TABLES);
    db.borrow_mut().drop_connection = true;
    assert_eq!(writer.step().unwrap_err().to_string(), "connection dropped");
    assert!(!writer.app().ready());
    assert_eq!(writer.step().unwrap(), Step::Started(Lsn(100)));
    assert!(writer.app().ready());
}

#[test]
fn a_dropped_stream_starts_over() {
    let (db, mut writer) = setup(&TABLES);
    writer.step().unwrap();
    let first = db.borrow_mut().commit(vec![trip(1, 1, 1, 10)]);
    assert_eq!(writer.step().unwrap(), Step::Applied(first));
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(1)));
    let trips = ask("trips", &["trips"], &[]);
    assert_eq!(writer.app().ask(&trips).unwrap().time, Some(time(first)));

    let second = db.borrow_mut().commit(vec![trip(2, 1, 1, 10)]);
    db.borrow_mut().drop_connection = true;
    assert!(writer.step().is_err());
    assert!(!writer.app().ready());
    assert_eq!(count(writer.app(), "trips"), Err(AnswerError::NotReady));

    // The new stream starts after the second commit, and the reload counts
    // the first trip once.
    assert_eq!(writer.step().unwrap(), Step::Started(second));
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(2)));
    assert_eq!(writer.app().ask(&trips).unwrap().time, None);
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
        [(
            vec![int(2)],
            vec![Value::Int(1), Value::Int(30), Value::Int(1)]
        )]
    );
    assert_eq!(answered.answer, expected(&db.borrow(), &by_kind));
}

#[test]
fn fact_change_without_the_old_row_reads_the_fact_again() {
    let (db, mut writer) = setup(&TABLES);
    db.borrow_mut().commit(vec![login(1, 1), login(2, 1)]);
    writer.step().unwrap();

    db.borrow_mut().commit(vec![Write::Delete {
        table: "logins",
        key: int(1),
    }]);
    writer.step().unwrap();
    assert_eq!(db.borrow().loads[3..], ["logins"]);
    assert_eq!(count(writer.app(), "logins"), Ok(Value::Int(1)));

    db.borrow_mut().commit(vec![Write::Update {
        table: "logins",
        key: int(2),
        new_key: int(2),
        row: vec![int(DAY), int(2)],
    }]);
    writer.step().unwrap();
    assert_eq!(db.borrow().loads[3..], ["logins", "logins"]);
    let by_day = ask("logins", &["logins"], &["day", "zone"]);
    assert_eq!(
        writer.app().ask(&by_day).unwrap().answer,
        expected(&db.borrow(), &by_day)
    );
}

#[test]
fn dim_key_change_drops_the_old_key() {
    // Without and with `REPLICA IDENTITY FULL` on zones.
    for full in [false, true] {
        let (db, mut writer) = setup(&[("zones", full), ("trips", true), ("logins", false)]);
        db.borrow_mut()
            .commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10), trip(2, 1, 2, 10)]);
        writer.step().unwrap();

        // Key 1 becomes 2: trip 1 loses its zone, trip 2 gains one.
        db.borrow_mut().commit(vec![Write::Update {
            table: "zones",
            key: int(1),
            new_key: int(2),
            row: vec![int(2), text("Queens")],
        }]);
        writer.step().unwrap();
        let by_zone = ask("trips", &["trips"], &["zone", "zone.borough"]);
        let answer = writer.app().ask(&by_zone).unwrap().answer;
        assert_eq!(
            answer.rows,
            [
                (vec![int(1), Datum::Null], vec![Value::Int(1)]),
                (vec![int(2), text("Queens")], vec![Value::Int(1)]),
            ],
            "full {full}"
        );

        // An update that keeps the key keeps the row.
        db.borrow_mut().commit(vec![Write::Update {
            table: "zones",
            key: int(2),
            new_key: int(2),
            row: vec![int(2), text("Bronx")],
        }]);
        writer.step().unwrap();
        let answer = writer.app().ask(&by_zone).unwrap().answer;
        assert_eq!(answer, expected(&db.borrow(), &by_zone), "full {full}");
        assert_eq!(answer.rows[1].0, [int(2), text("Bronx")], "full {full}");
        assert_eq!(db.borrow().loads.len(), 3, "no reload, full {full}");
    }
}

#[test]
fn dim_delete_drops_the_key() {
    let (db, mut writer) = setup(&TABLES);
    db.borrow_mut()
        .commit(vec![zone(1, "Bronx"), zone(2, "Queens"), trip(1, 1, 1, 10)]);
    writer.step().unwrap();
    db.borrow_mut().commit(vec![Write::Delete {
        table: "zones",
        key: int(1),
    }]);
    writer.step().unwrap();
    let by_borough = ask("trips", &["trips"], &["zone.borough"]);
    assert_eq!(
        writer.app().ask(&by_borough).unwrap().answer.rows,
        [(vec![Datum::Null], vec![Value::Int(1)])]
    );
    assert_eq!(db.borrow().loads.len(), 3, "no reload");

    // A delete that doesn't say which key: read the dim again.
    db.borrow_mut().send(vec![Change::Delete {
        table: id("zones"),
        old: Old::SameKey,
    }]);
    writer.step().unwrap();
    assert_eq!(db.borrow().loads[3..], ["zones"]);
    assert!(writer.app().ready());
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
    let by_borough = ask("trips", &["trips"], &["zone.borough"]);
    let answer = writer.app().ask(&by_borough).unwrap().answer;
    assert_eq!(answer.rows, [(vec![Datum::Null], vec![Value::Int(1)])]);
    assert_eq!(answer, expected(&db.borrow(), &by_borough));
}

#[test]
fn altered_table_is_read_again() {
    let (db, mut writer) = setup(&TABLES);
    db.borrow_mut().commit(vec![trip(1, 1, 1, 10)]);
    writer.step().unwrap();
    // Read once, however often the commit touches trips after the Alter.
    db.borrow_mut().commit(vec![
        Write::Alter { table: "trips" },
        trip(2, 1, 1, 10),
        Write::Alter { table: "trips" },
    ]);
    writer.step().unwrap();
    assert_eq!(db.borrow().loads[3..], ["trips"]);
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(2)));
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
    assert!(writer.app().ready());
}

#[test]
fn a_table_read_again_waits_for_the_stream() {
    let (db, mut writer) = setup(&TABLES);
    db.borrow_mut().commit(vec![login(1, 1)]);
    writer.step().unwrap();

    // The delete makes logins load again; by then the database is past the
    // second commit.
    let first = db.borrow_mut().commit(vec![Write::Delete {
        table: "logins",
        key: int(1),
    }]);
    let second = db.borrow_mut().commit(vec![login(2, 1), trip(1, 1, 1, 10)]);

    assert_eq!(writer.step().unwrap(), Step::Applied(first));
    assert!(!writer.app().ready());
    assert_eq!(count(writer.app(), "logins"), Err(AnswerError::NotReady));
    let trips = writer.app().ask(&ask("trips", &["trips"], &[])).unwrap();
    assert_eq!(trips.lsn, first);
    assert_eq!(trips.answer.rows[0].1, [Value::Int(0)]);

    // The second commit is already in the logins load and only reaches trips.
    assert_eq!(writer.step().unwrap(), Step::Applied(second));
    assert!(writer.app().ready());
    assert_eq!(count(writer.app(), "logins"), Ok(Value::Int(1)));
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(1)));
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
fn asks_go_on_while_a_table_loads() {
    let db = Database::new(&TABLES).shared();
    let app = App::new(spec());
    let mut fake = Fake::new(&db);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let (watch, log) = (app.clone(), Rc::clone(&seen));
    fake.on_load = Some(Box::new(move |table| {
        let count =
            |fact| aside(&watch, ask(fact, &[fact], &[])).map(|a| a.answer.rows[0].1[0].clone());
        log.borrow_mut()
            .push((table.to_owned(), count("trips"), count("logins")));
    }));
    let mut writer = Writer::new(app, fake);
    db.borrow_mut()
        .commit(vec![login(1, 1), login(2, 1), trip(1, 1, 1, 10)]);
    writer.step().unwrap();
    let not_ready = Err(AnswerError::NotReady);
    let starting = |table: &str| (table.to_owned(), not_ready.clone(), not_ready.clone());
    assert_eq!(
        *seen.borrow(),
        [starting("zones"), starting("trips"), starting("logins")]
    );

    // Without the old row, the delete reads logins again; trips answers
    // meanwhile, at the delete.
    seen.borrow_mut().clear();
    db.borrow_mut().commit(vec![Write::Delete {
        table: "logins",
        key: int(1),
    }]);
    writer.step().unwrap();
    assert_eq!(
        *seen.borrow(),
        [("logins".to_owned(), Ok(Value::Int(1)), not_ready)]
    );
    assert_eq!(count(writer.app(), "logins"), Ok(Value::Int(1)));
}

#[test]
fn status_reports_tables_and_counts() {
    let (db, mut writer) = setup(&TABLES);
    let table = |name: &str, size, ready| TableStatus {
        name: id(name),
        size,
        ready,
    };
    assert_eq!(
        writer.app().status(),
        Status {
            ready: false,
            lsn: None,
            time: None,
            facts: vec![table("trips", 0, false), table("logins", 0, false)],
            dims: vec![table("zones", 0, false)],
            starts: 0,
            commits: 0,
            reloads: 0,
            failures: 0,
            error: None,
        }
    );

    let start = db.borrow_mut().commit(vec![
        zone(1, "Bronx"),
        zone(2, "Queens"),
        trip(1, 1, 1, 10),
        trip(2, 1, 1, 10),
        trip(3, 2, 1, 10),
        login(1, 1),
    ]);
    writer.step().unwrap();
    let status = writer.app().status();
    assert!(status.ready);
    assert_eq!((status.lsn, status.time), (Some(start), None));
    assert_eq!(
        (status.facts, status.dims),
        (
            vec![table("trips", 2, true), table("logins", 1, true)],
            vec![table("zones", 2, true)]
        )
    );

    let delete = db.borrow_mut().commit(vec![Write::Delete {
        table: "logins",
        key: int(1),
    }]);
    let later = db.borrow_mut().commit(vec![login(2, 1)]);
    writer.step().unwrap();
    let status = writer.app().status();
    assert!(!status.ready, "logins waits for the stream");
    assert_eq!(status.facts[1], table("logins", 1, false));
    assert_eq!(
        (status.lsn, status.time),
        (Some(delete), Some(time(delete)))
    );
    assert_eq!((status.commits, status.reloads), (1, 1));
    writer.step().unwrap();
    let status = writer.app().status();
    assert!(status.ready);
    assert_eq!(status.lsn, Some(later));
    assert_eq!((status.commits, status.reloads), (2, 1));

    db.borrow_mut().drop_connection = true;
    db.borrow_mut().commit(vec![login(3, 1)]);
    writer.step().unwrap_err();
    let status = writer.app().status();
    assert!(!status.ready);
    assert_eq!(status.lsn, None);
    assert_eq!(status.facts[0], table("trips", 2, false));
    assert_eq!((status.failures, status.starts), (1, 1));
    assert_eq!(status.error.as_deref(), Some("connection dropped"));

    writer.step().unwrap();
    let status = writer.app().status();
    assert!(status.ready);
    assert_eq!((status.failures, status.starts), (1, 2));
    assert_eq!(status.error, None);
}

#[test]
fn a_row_that_does_not_fit_stops_weir() {
    let (db, mut writer) = setup(&TABLES);
    writer.step().unwrap();
    let mut row = vec![int(0), int(1), int(1), text("ten"), text("ann")];
    db.borrow_mut()
        .commit(vec![insert("trips", 1, row.clone())]);

    // The insert fails, and so does reading trips again.
    let err = writer.step().unwrap_err();
    assert_eq!(err.to_string(), "column fare is not an integer");
    assert!(!writer.app().ready());
    assert!(writer.step().is_err());
    assert!(!writer.app().ready());

    // Once the row is fixed, weir starts again.
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

#[test]
fn a_dim_row_that_does_not_fit_stops_weir() {
    let (db, mut writer) = setup(&TABLES);
    writer.step().unwrap();
    db.borrow_mut()
        .commit(vec![insert("zones", 3, vec![int(3)])]);

    // The upsert fails, and so does reading zones again.
    let err = writer.step().unwrap_err();
    assert_eq!(err.to_string(), "row has 1 columns, expected 2");
    assert!(!writer.app().ready());
}

#[test]
fn a_change_that_does_not_fit_reads_the_table_again() {
    let trip_row = || vec![int(0), int(1), int(1), int(10), text("ann")];
    let never_added = || vec![int(0), int(9), int(9), int(9), text("zed")];
    let cases = [
        (
            "dim insert of the wrong width",
            "zones",
            Change::Insert {
                table: id("zones"),
                row: vec![int(3)],
            },
        ),
        (
            "dim update of the wrong width",
            "zones",
            Change::Update {
                table: id("zones"),
                old: Old::SameKey,
                new: vec![int(1)],
            },
        ),
        (
            "dim key change from a key weir never saw",
            "zones",
            Change::Update {
                table: id("zones"),
                old: Old::Key(int(7)),
                new: vec![int(1), text("Bronx")],
            },
        ),
        (
            "dim delete of a key weir never saw",
            "zones",
            Change::Delete {
                table: id("zones"),
                old: Old::Key(int(7)),
            },
        ),
        (
            "fact update from a row weir never saw",
            "trips",
            Change::Update {
                table: id("trips"),
                old: Old::Row(never_added()),
                new: trip_row(),
            },
        ),
        // The old row is already out of its cell when the new one fails.
        (
            "fact update to a row of the wrong width",
            "trips",
            Change::Update {
                table: id("trips"),
                old: Old::Row(trip_row()),
                new: vec![int(0)],
            },
        ),
        (
            "fact delete of a row weir never saw",
            "trips",
            Change::Delete {
                table: id("trips"),
                old: Old::Row(never_added()),
            },
        ),
    ];
    let by_borough = ask("trips", &["trips", "fare"], &["zone.borough"]);
    for (name, table, change) in cases {
        let (db, mut writer) = setup(&TABLES);
        db.borrow_mut()
            .commit(vec![zone(1, "Bronx"), zone(2, "Queens"), trip(1, 1, 1, 10)]);
        writer.step().unwrap();
        let lsn = db.borrow_mut().send(vec![change]);

        assert_eq!(writer.step().unwrap(), Step::Applied(lsn), "{name}");
        assert_eq!(db.borrow().loads[3..], [table], "{name}");
        assert!(writer.app().ready(), "{name}");
        assert_eq!(
            writer.app().ask(&by_borough).unwrap().answer,
            expected(&db.borrow(), &by_borough),
            "{name}"
        );
    }
}

/// Opens at 20, loads every table at `at`, then streams one commit at 30
/// that alters zones.
struct Behind {
    at: Lsn,
}

impl Source for Behind {
    fn open(&mut self, _: &Spec) -> Result<Lsn, SourceError> {
        Ok(Lsn(20))
    }

    fn load_fact(&mut self, _: &Fact, _: &mut Rollup) -> Result<Lsn, SourceError> {
        Ok(self.at)
    }

    fn load_dim(&mut self, _: &Dim, _: &mut DimTable) -> Result<Lsn, SourceError> {
        Ok(self.at)
    }

    fn next_commit(&mut self) -> Result<Commit, SourceError> {
        Ok(Commit {
            lsn: Lsn(30),
            time: UNIX_EPOCH,
            changes: vec![Change::Altered { table: id("zones") }],
        })
    }
}

#[test]
fn a_load_behind_the_stream_is_an_error() {
    let mut writer = Writer::new(App::new(spec()), Behind { at: Lsn(19) });
    let err = writer.step().unwrap_err();
    assert_eq!(
        err.to_string(),
        "zones was read at 0/13, behind the stream at 0/14"
    );
    assert!(!writer.app().ready());

    // A load at the stream's start is fine, but not once the stream is past.
    let mut writer = Writer::new(App::new(spec()), Behind { at: Lsn(20) });
    assert_eq!(writer.step().unwrap(), Step::Started(Lsn(20)));
    let err = writer.step().unwrap_err();
    assert_eq!(
        err.to_string(),
        "zones was read at 0/14, behind the stream at 0/1E"
    );
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

// Random writes, dropped connections and reloads.

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn int(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % (hi - lo) as u64) as i64
    }

    /// An integer in `lo..hi`, or null one time in ten.
    fn maybe(&mut self, lo: i64, hi: i64) -> Datum {
        if self.chance(10) {
            Datum::Null
        } else {
            int(self.int(lo, hi))
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
    // Keys as the transaction goes, so no write misses its row.
    let mut keys: BTreeMap<&'static str, BTreeSet<Datum>> = ["zones", "trips", "logins"]
        .into_iter()
        .map(|t| (t, db.keys(t).into_iter().collect()))
        .collect();
    let mut new_key = || {
        *next_key += 1;
        int(*next_key)
    };
    let existing = |keys: &BTreeMap<_, BTreeSet<Datum>>, table| -> Vec<Datum> {
        keys[table].iter().cloned().collect()
    };
    let mut writes = Vec::new();
    for _ in 0..1 + rng.below(4) {
        let write = match rng.below(100) {
            0..40 => Write::Insert {
                table: "trips",
                key: new_key(),
                row: random_trip(rng),
            },
            40..55 => match rng.pick(&existing(&keys, "trips")) {
                Some(key) => Write::Update {
                    table: "trips",
                    new_key: if rng.chance(20) {
                        new_key()
                    } else {
                        key.clone()
                    },
                    key,
                    row: random_trip(rng),
                },
                None => continue,
            },
            55..65 => match rng.pick(&existing(&keys, "trips")) {
                Some(key) => Write::Delete {
                    table: "trips",
                    key,
                },
                None => continue,
            },
            65..75 => {
                let key = int(rng.int(1, 7));
                let borough = rng.maybe_text(&BOROUGHS);
                if !keys["zones"].contains(&key) {
                    Write::Insert {
                        table: "zones",
                        row: vec![key.clone(), borough],
                        key,
                    }
                } else {
                    // Sometimes to a free key, so the old one goes.
                    let free = int(rng.int(1, 7));
                    let new_key = if keys["zones"].contains(&free) {
                        key.clone()
                    } else {
                        free
                    };
                    Write::Update {
                        table: "zones",
                        row: vec![new_key.clone(), borough],
                        key,
                        new_key,
                    }
                }
            }
            75..80 => match rng.pick(&existing(&keys, "zones")) {
                Some(key) => Write::Delete {
                    table: "zones",
                    key,
                },
                None => continue,
            },
            80..90 => Write::Insert {
                table: "logins",
                key: new_key(),
                row: random_login(rng),
            },
            // Logins are append-only; these make weir read them again.
            90..92 => match rng.pick(&existing(&keys, "logins")) {
                Some(key) if rng.chance(50) => Write::Delete {
                    table: "logins",
                    key,
                },
                Some(key) => Write::Update {
                    table: "logins",
                    new_key: key.clone(),
                    key,
                    row: random_login(rng),
                },
                None => continue,
            },
            92..95 => Write::Insert {
                table: "audit",
                key: new_key(),
                row: vec![text("x")],
            },
            95..97 => Write::Truncate {
                table: rng.pick(&["zones", "trips", "logins"]).unwrap(),
            },
            _ => Write::Alter {
                table: rng.pick(&["zones", "trips", "logins"]).unwrap(),
            },
        };
        match &write {
            // audit keeps no keys here.
            Write::Insert { table, key, .. } => {
                if let Some(k) = keys.get_mut(table) {
                    k.insert(key.clone());
                }
            }
            Write::Update {
                table,
                key,
                new_key,
                ..
            } => {
                let k = keys.get_mut(table).unwrap();
                k.remove(key);
                k.insert(new_key.clone());
            }
            Write::Delete { table, key } => {
                keys.get_mut(table).unwrap().remove(key);
            }
            Write::Truncate { table } => keys.get_mut(table).unwrap().clear(),
            Write::Alter { .. } => {}
        }
        writes.push(write);
    }
    writes
}

fn random_ask(rng: &mut Rng) -> Ask {
    let (fact, measures, keys, times): (_, &[&str], &[&str], &[&str]) = if rng.chance(70) {
        (
            "trips",
            &["trips", "fare", "fare_max", "riders"],
            &["kind", "zone", "zone.borough", "zone.id"],
            &["bucket", "hour", "day"],
        )
    } else {
        (
            "logins",
            &["logins"],
            &["zone", "zone.borough"],
            &["bucket", "day"],
        )
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
        by.push(rng.pick(times).unwrap());
    }
    let mut ask = ask(fact, &chosen, &by);
    if rng.chance(30) {
        let values = BOROUGHS
            .iter()
            .filter(|_| rng.chance(50))
            .map(|b| Literal::Text(b.to_string()))
            .collect::<Vec<_>>();
        if !values.is_empty() {
            ask.filter.push(Cond {
                path: path("zone.borough"),
                values,
            });
        }
    }
    // Sometimes one day of the range.
    if rng.chance(30) {
        ask.from = DAY * rng.int(0, 3);
        ask.to = ask.from + DAY;
    }
    ask
}

#[derive(Debug, Default)]
struct Seen {
    answered: usize,
    waited: usize,
    failed: usize,
    reloads: usize,
    /// Asks answered while a table loaded.
    aside: usize,
}

/// Checks, in the middle of a load, that the table loading doesn't answer
/// and every table that is ready answers as a fresh load at the app's
/// position would.
fn check_aside(app: &App, db: &Database, loading: &str) -> usize {
    let status = app.status();
    let ready = |name: &str| {
        (status.facts.iter().chain(&status.dims)).any(|t| t.name.as_str() == name && t.ready)
    };
    assert!(!ready(loading), "{loading} is loading");
    let mut answered = 0;
    for fact in ["trips", "logins"] {
        let ask = ask(fact, &[fact], &["zone"]);
        match aside(app, ask.clone()) {
            Ok(got) => {
                assert!(
                    ready(fact) && ready("zones"),
                    "{fact} while {loading} loads"
                );
                let lsn = status.lsn.unwrap();
                assert_eq!(got.lsn, lsn);
                assert_eq!(got.answer, reference(db, lsn).ask(&ask).unwrap());
                answered += 1;
            }
            Err(AnswerError::NotReady) => {
                assert!(
                    !(ready(fact) && ready("zones")),
                    "{fact} while {loading} loads"
                )
            }
            Err(e) => panic!("{e}"),
        }
    }
    answered
}

/// Asks against a fresh load of the database at `lsn`. Once the app has
/// applied the latest commit, every table must be ready.
fn check(app: &App, db: &Database, lsn: Lsn, rng: &mut Rng, seen: &mut Seen) {
    let reference = reference(db, lsn);
    for _ in 0..4 {
        let ask = random_ask(rng);
        match app.ask(&ask) {
            Ok(answered) => {
                assert_eq!(answered.lsn, lsn);
                assert_eq!(answered.answer, reference.ask(&ask).unwrap(), "{ask:?}");
                seen.answered += 1;
            }
            Err(AnswerError::NotReady) if lsn < db.lsn() => seen.waited += 1,
            Err(e) => panic!("{e} at {lsn} for {ask:?}"),
        }
    }
    assert!(lsn < db.lsn() || app.ready());
}

fn follow(seed: u64, seen: &mut Seen) {
    let mut rng = Rng(seed);
    let db = Database::new(&TABLES).shared();
    let app = App::new(spec());
    let mut fake = Fake::new(&db);
    let aside = Rc::new(RefCell::new(0));
    let (watch, shared, count) = (app.clone(), Rc::clone(&db), Rc::clone(&aside));
    fake.on_load = Some(Box::new(move |table| {
        *count.borrow_mut() += check_aside(&watch, &shared.borrow(), table);
    }));
    let mut writer = Writer::new(app, fake);
    let mut next_key = 0;
    let mut position = None;
    for _ in 0..150 {
        for _ in 0..rng.below(4) {
            let writes = random_writes(&mut rng, &db.borrow(), &mut next_key);
            db.borrow_mut().commit(writes);
        }
        if rng.chance(5) {
            db.borrow_mut().drop_connection = true;
        }
        let latest = db.borrow().lsn();
        let mut steps = 0;
        while position != Some(latest) {
            steps += 1;
            assert!(steps < 100, "seed {seed}: stuck");
            let loads = db.borrow().loads.len();
            position = match writer.step() {
                Ok(Step::Started(lsn)) => Some(lsn),
                Ok(Step::Applied(lsn)) => {
                    if db.borrow().loads.len() > loads {
                        seen.reloads += 1;
                    }
                    Some(lsn)
                }
                Err(_) => {
                    seen.failed += 1;
                    None
                }
            };
            if let Some(lsn) = position {
                check(writer.app(), &db.borrow(), lsn, &mut rng, seen);
            }
        }
        assert!(writer.app().ready(), "seed {seed}");
    }
    seen.aside += *aside.borrow();
}

#[test]
fn follows_the_database() {
    let mut seen = Seen::default();
    for seed in 0..16 {
        follow(seed, &mut seen);
    }
    // The run went through every path it claims to test.
    assert!(seen.answered > 1_000, "{seen:?}");
    assert!(seen.waited > 0, "{seen:?}");
    assert!(seen.failed > 0, "{seen:?}");
    assert!(seen.reloads > 0, "{seen:?}");
    assert!(seen.aside > 0, "{seen:?}");
}
