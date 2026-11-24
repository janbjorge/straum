//! Retention through the writer: the store is advanced to the snapshot's
//! time before any table loads, and to each commit's time before its
//! changes, so a row older than the horizon never makes an hour cell and
//! asks past it are refused. The fake's clock is set ten days on, so the
//! horizons fall among the rows.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use straum_app::{AnswerError, App, Step, Writer};
use straum_domain::{AnswerRow, Ask, AskError, AskKey, Spec, Store, Width};

use crate::common::{DAY, LIMITS, database, insert, int, row};
use crate::fake::{Fake, Write, id};

/// Seconds in an hour.
const HOUR: i64 = 3_600;

/// Trips by kind, hours kept a day, days for good.
const SPEC: &str = "
CREATE VIEW trips AS
SELECT buckets(t.at, keep('1 hour', '1 day'), '1 day') AS bucket,
       t.kind AS kind,
       count(*) AS trips
FROM public.trips AS t
GROUP BY 1, 2;
";

fn spec() -> Spec {
    straum_notation::parse(SPEC).unwrap()
}

/// A trip of kind 1 at `at`, `[at, kind]`.
fn trip_at(key: i64, at: i64) -> Write {
    insert("trips", key, vec![int(at), int(1)])
}

/// Days from a clock's start: the fake's clock runs that far past its
/// LSN's seconds.
fn days(count: u64) -> Duration {
    Duration::from_secs(count * 86_400)
}

/// The trips by the view's time at `seconds`, over `from..to`.
fn by_width(seconds: u32, from: i64, to: i64) -> Ask {
    let key = if seconds == 3_600 {
        AskKey::Key(id("bucket"))
    } else {
        AskKey::WiderBucket {
            name: id("bucket"),
            width: Width { seconds },
        }
    };
    Ask {
        view: id("trips"),
        measures: vec![id("trips")],
        keys: vec![key],
        filter: Vec::new(),
        having: Vec::new(),
        from: Some(from),
        to: Some(to),
        bounded: None,
    }
}

fn by_hour(from: i64, to: i64) -> Ask {
    by_width(3_600, from, to)
}

fn by_day(from: i64, to: i64) -> Ask {
    by_width(86_400, from, to)
}

/// The refusal of an hour ask before `kept_from`.
fn past(kept_from: i64) -> Vec<AskError> {
    vec![AskError::PastRetention {
        width: Width { seconds: 3_600 },
        kept_from,
    }]
}

/// What the app refuses `ask` for; an answer fails the test.
fn refusal(app: &App, ask: &Ask) -> Vec<AskError> {
    match app.ask(ask) {
        Err(AnswerError::Invalid(errors)) => errors,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn rows(app: &App, ask: &Ask) -> Vec<AnswerRow> {
    app.ask(ask).unwrap().answer.rows
}

/// Hour cells the store holds.
fn hour_cells(app: &App) -> usize {
    app.held().unwrap().views[0].cells
}

fn writer_of(fake: Fake) -> Writer<Fake> {
    Writer::new(App::new(spec(), LIMITS), fake)
}

/// The snapshot is taken ten days on: hours are kept from day nine. The
/// store each load fills already refuses the hours of day two, and the
/// trip there makes no hour cell, though days count it.
#[test]
fn the_store_is_advanced_to_the_snapshot_before_the_load() {
    let database = database();
    database.borrow_mut().commit(vec![
        trip_at(1, 2 * DAY + 600),
        trip_at(2, 9 * DAY + 2 * HOUR),
    ]);
    database.borrow_mut().clock = days(10);
    let seen = Rc::new(RefCell::new(Vec::new()));
    let mut fake = Fake::new(&database);
    let record = Rc::clone(&seen);
    fake.before_load = Some(Box::new(move |store: &Store| {
        let old = by_hour(2 * DAY, 3 * DAY);
        record.borrow_mut().push(store.ask(&old).err());
    }));
    let mut writer = writer_of(fake);
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    assert_eq!(*seen.borrow(), [Some(past(9 * DAY))]);
    let app = writer.app();
    assert_eq!(refusal(app, &by_hour(2 * DAY, 3 * DAY)), past(9 * DAY));
    assert_eq!(hour_cells(app), 1);
    assert_eq!(
        rows(app, &by_day(0, 10 * DAY)),
        [row(int(2 * DAY), 1), row(int(9 * DAY), 1)]
    );
    assert_eq!(
        rows(app, &by_hour(9 * DAY, 10 * DAY)),
        [row(int(9 * DAY + 2 * HOUR), 1)]
    );
}

/// The snapshot two days on keeps hours from day one; a commit ten days
/// on moves that to day nine before its trips go in, so its trip of day
/// three makes no hour cell. A later commit stamped earlier moves nothing
/// back.
#[test]
fn the_store_is_advanced_to_each_commit_s_time_before_its_changes() {
    let database = database();
    database.borrow_mut().clock = days(2);
    let mut writer = writer_of(Fake::new(&database));
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    let app = writer.app().clone();
    assert_eq!(refusal(&app, &by_hour(0, DAY)), past(DAY));
    assert!(app.ask(&by_hour(DAY, 3 * DAY)).is_ok());
    database.borrow_mut().clock = days(10);
    database.borrow_mut().commit(vec![
        trip_at(1, 3 * DAY + 600),
        trip_at(2, 9 * DAY + 2 * HOUR),
    ]);
    assert!(matches!(writer.step(), Ok(Step::Applied(_))));
    assert_eq!(refusal(&app, &by_hour(3 * DAY, 4 * DAY)), past(9 * DAY));
    assert_eq!(hour_cells(&app), 1);
    assert_eq!(
        rows(&app, &by_day(0, 10 * DAY)),
        [row(int(3 * DAY), 1), row(int(9 * DAY), 1)]
    );
    database.borrow_mut().clock = days(5);
    database
        .borrow_mut()
        .commit(vec![trip_at(3, 8 * DAY + HOUR)]);
    assert!(matches!(writer.step(), Ok(Step::Applied(_))));
    assert_eq!(refusal(&app, &by_hour(8 * DAY, 9 * DAY)), past(9 * DAY));
    assert_eq!(hour_cells(&app), 1);
    assert_eq!(
        rows(&app, &by_day(0, 10 * DAY)),
        [
            row(int(3 * DAY), 1),
            row(int(8 * DAY), 1),
            row(int(9 * DAY), 1)
        ]
    );
}
