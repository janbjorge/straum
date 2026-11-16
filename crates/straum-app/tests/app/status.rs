//! What the app reports about itself: ready or not, how far behind, the
//! WAL the slot holds, the last failure; and how its values print.

use std::cell::RefCell;
use std::thread;
use std::time::{Duration, SystemTime};

use straum_app::{AnswerError, App, Error, Limits, Lsn, Status, Step, Wal, Writer};
use straum_domain::{AskError, Instead, Value};

use crate::common::{BYTES_PER_GIB, Harness, LIMITS, ask, count, setup, spec, trip, wal, watched};
use crate::fake::{Database, Fake, id, time};

#[test]
fn ready_only_within_the_lag_bound() {
    let limits = Limits {
        max_lag: Duration::ZERO,
        ..LIMITS
    };
    let app = App::new(spec(), limits);
    // Loading takes time, so WAL written meanwhile has an age.
    let Harness {
        database,
        mut writer,
    } = watched(app, |_, _| thread::sleep(Duration::from_millis(1)));
    assert_eq!(writer.app().status().lag, None);

    // WAL written during the load that the slot hasn't confirmed.
    database.borrow_mut().wal = Some(wal(200, 100));
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
    database.borrow_mut().wal = Some(wal(200, 200));
    database.borrow_mut().waits = 1;
    let lsn = database.borrow_mut().commit(vec![trip(1, 1, 1, 10)]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    assert_eq!(writer.app().status().lag, Some(Duration::ZERO));
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
    let failed = status_after_a_failure(&database, &mut writer);
    status_after_the_next_start(&database, &mut writer, &failed);
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
    database: &RefCell<Database>,
    writer: &mut Writer<Fake>,
    start: Lsn,
) {
    let held = Wal {
        written: Lsn(start.0 + 50),
        confirmed: start,
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

fn status_after_a_failure(database: &RefCell<Database>, writer: &mut Writer<Fake>) -> Status {
    database.borrow_mut().drop_connection = true;
    database.borrow_mut().commit(vec![trip(3, 1, 1, 10)]);
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
fn status_after_the_next_start(
    database: &RefCell<Database>,
    writer: &mut Writer<Fake>,
    failed: &Status,
) {
    database.borrow_mut().wal = None;
    writer.step().unwrap();
    let after = writer.app().status();
    assert!(after.ready);
    assert_eq!(after.last_failure, failed.last_failure);
    assert_eq!((after.starts, after.failures), (2, 1));
}

#[test]
fn too_much_wal_starts_over() {
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
    assert_eq!(writer.app().status().last_failure.unwrap().error, message);

    // Up to the limit is fine.
    database.borrow_mut().wal = Some(wal(100 + limit, 100));
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
    assert_eq!(
        invalid.to_string(),
        "unknown measure nope\nno measures asked"
    );
}
