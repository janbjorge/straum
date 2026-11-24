//! What the app reports about itself: ready or not, how far behind, the
//! WAL the slot holds, how far a load is; and how its values print.

use std::cell::RefCell;
use std::rc::Rc;
use std::thread;
use std::time::{Duration, SystemTime};

use straum_app::{AnswerError, App, Error, Limits, Lsn, Status, Step, Wal, Writer};
use straum_domain::{AskError, Instead, Value};

use crate::common::{
    BYTES_PER_GIB, Harness, LIMITS, again, ask, count, database, login, setup, spec, trip, wal,
    watched, watching, zone,
};
use crate::fake::{Database, Fake, id, time};

#[test]
fn ready_only_within_the_lag_bound() {
    // Lag counts from the last reading caught up, so a bound of 0 would
    // never be met: the load takes longer than this one, a moment after
    // it doesn't.
    let limits = Limits {
        max_lag: Duration::from_millis(100),
        ..LIMITS
    };
    let app = App::new(spec(), limits);
    // Loading takes time, so WAL written meanwhile has an age.
    let Harness {
        database,
        mut writer,
    } = watched(app, |_, _| thread::sleep(Duration::from_millis(60)));
    assert_eq!(writer.app().status().lag, None);

    // WAL written during the load that Straum hasn't applied.
    database.borrow_mut().wal = Some(wal(200, 100));
    assert_eq!(writer.step().unwrap(), Step::Started(Lsn(100)));
    assert!(writer.app().status().lag.unwrap() >= Duration::from_millis(2));
    assert!(!writer.app().status().ready);
    // Answers are as of before the load, not the reading after it.
    let behind = writer.app().ask(&ask("trips", &["trips"], &[])).unwrap();
    assert!(behind.as_of + Duration::from_millis(2) <= SystemTime::now());
    // Asks still answer, at the LSN they reflect.
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(0)));

    // While it waits a second for a commit, Straum reads the WAL again and
    // finds the slot caught up.
    database.borrow_mut().wal = Some(wal(200, 200));
    database.borrow_mut().waits = 1;
    let lsn = database.borrow_mut().commit(vec![trip(1, 1, 1, 10)]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    // Caught up at the last reading, a moment ago.
    assert!(writer.app().status().lag.unwrap() < Duration::from_secs(1));
    assert!(writer.app().status().ready);
    let caught_up = writer.app().ask(&ask("trips", &["trips"], &[])).unwrap();
    assert!(caught_up.as_of >= behind.as_of + Duration::from_secs(1));

    // A failure forgets the lag along with the tables.
    database.borrow_mut().drop_connection = true;
    database.borrow_mut().commit(vec![trip(2, 1, 1, 10)]);
    writer.step().unwrap_err();
    assert_eq!(writer.app().status().lag, None);
}

#[test]
fn status_reports_what_straum_did() {
    let Harness {
        database,
        mut writer,
    } = setup();
    status_before_the_first_step(writer.app());
    let start = status_after_a_start(&database, &mut writer);
    status_while_waiting_for_a_commit(&database, &mut writer, start);
    status_after_a_failure(&database, &mut writer);
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
            loading: None,
            limits: LIMITS,
        }
    );
}

fn status_after_a_start(database: &RefCell<Database>, writer: &mut Writer<Fake>) -> Lsn {
    let start = database.borrow_mut().commit(vec![trip(1, 1, 1, 10)]);
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
    assert!(status.lag.unwrap() < Duration::from_secs(1));
    let caught_up = Wal {
        written: start,
        applied: start,
        restart: start,
    };
    assert_eq!(status.wal, Some(caught_up));
    start
}

// A reading while it waits for the next commit.
fn status_while_waiting_for_a_commit(
    database: &RefCell<Database>,
    writer: &mut Writer<Fake>,
    start: Lsn,
) {
    let held = Wal {
        written: Lsn(start.0 + 50),
        applied: start,
        restart: Lsn(start.0 - 20),
    };
    database.borrow_mut().wal = Some(held);
    database.borrow_mut().waits = 1;
    let lsn = database.borrow_mut().commit(vec![trip(2, 1, 1, 10)]);
    writer.step().unwrap();
    let status = writer.app().status();
    assert_eq!(
        (status.applied, status.committed),
        (Some(lsn), Some(time(lsn)))
    );
    assert_eq!(status.wal, Some(held));
}

// A failed writer's app says nothing of the stream it had.
fn status_after_a_failure(database: &RefCell<Database>, writer: &mut Writer<Fake>) {
    database.borrow_mut().drop_connection = true;
    database.borrow_mut().commit(vec![trip(3, 1, 1, 10)]);
    assert_eq!(writer.step().unwrap_err().to_string(), "connection dropped");
    let status = writer.app().status();
    assert!(!status.ready);
    assert_eq!((status.applied, status.lag, status.wal), (None, None, None));
    assert_eq!(status.as_of, None);
}

#[test]
fn too_much_wal_stops_the_writer() {
    let Harness {
        database,
        mut writer,
    } = setup();
    let limit = LIMITS.max_retained_wal;
    let message = "PostgreSQL holds 5120.0 MiB of WAL for Straum's slot, \
                   past the limit of 4096.0 MiB";

    database.borrow_mut().wal = Some(wal(100 + 5 * BYTES_PER_GIB, 100));
    let error = writer.step().unwrap_err();
    assert!(
        matches!(error, Error::Retained { retained, limit } if retained == 5 * BYTES_PER_GIB && limit == 4 * BYTES_PER_GIB)
    );
    assert_eq!(error.to_string(), message);
    assert!(!writer.app().status().ready);

    // Up to the limit is fine, for the writer Straum starts again with.
    database.borrow_mut().wal = Some(wal(100 + limit, 100));
    let mut writer = again(&database);
    assert_eq!(writer.step().unwrap(), Step::Started(Lsn(100)));
    assert!(writer.app().status().ready);

    // Past it while streaming, at the next reading.
    database.borrow_mut().wal = Some(wal(100 + 5 * BYTES_PER_GIB, 100));
    database.borrow_mut().waits = 1;
    database.borrow_mut().commit(vec![trip(1, 1, 1, 10)]);
    assert_eq!(writer.step().unwrap_err().to_string(), message);
    assert!(!writer.app().status().ready);
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
        AskError::UnknownMeasure {
            name: id("nope"),
            instead: Instead::Every(vec![]),
        },
        AskError::NoMeasures,
    ]);
    // Each error's words name Straum SQL, so straum-notation says them.
    assert_eq!(invalid.to_string(), "errors in the ask: 2");
}

/// The reason each table gives in the middle of its load, as `/readyz`
/// says it, over `database`.
fn reasons_while_loading(database: &Rc<RefCell<Database>>) -> Vec<String> {
    let reasons = Rc::new(RefCell::new(Vec::new()));
    let seen = Rc::clone(&reasons);
    let mut writer = watching(App::new(spec(), LIMITS), database, move |app, _| {
        let loading = app.status().loading.expect("a table loads");
        seen.borrow_mut().push(loading.to_string());
    });
    assert_eq!(writer.app().status().loading, None);
    writer.step().unwrap();
    assert_eq!(writer.app().status().loading, None);
    reasons.take()
}

/// Two zones, three trips and a login, which the fake estimates exactly.
fn some_of_each(database: &Rc<RefCell<Database>>) {
    database.borrow_mut().commit(vec![
        zone(1, "Bronx"),
        zone(2, "Queens"),
        trip(1, 1, 1, 10),
        trip(2, 1, 1, 10),
        trip(3, 1, 2, 10),
        login(1, 1),
    ]);
}

/// While a table loads, the status says which, its place among the
/// tables, how much of it is read and what the stream holds meanwhile.
/// Before the load and after it, it says none of that.
#[test]
fn a_load_says_how_far_it_is() {
    let database = database();
    some_of_each(&database);
    let held = "commits held 0.0 MiB of 1024.0 MiB";
    assert_eq!(
        reasons_while_loading(&database),
        [
            format!("loading public.zones, table 1 of 3, 50% of rows; {held}"),
            format!("loading public.trips, table 2 of 3, 33% of rows; {held}"),
            format!("loading public.logins, table 3 of 3, 0% of rows; {held}"),
        ]
    );
}

/// A table of unknown size gives no percent, never one of 0 or -1 rows.
#[test]
fn a_load_of_unknown_size_gives_no_percent() {
    let database = database();
    some_of_each(&database);
    database.borrow_mut().unanalysed = true;
    let reasons = reasons_while_loading(&database);
    assert_eq!(reasons.len(), 3);
    for reason in reasons {
        assert!(!reason.contains('%'), "{reason}");
        assert!(reason.contains(", table "), "{reason}");
    }
}
