//! A writer with a spill: old buckets go out to it, a change brings them
//! back, each start empties it, and a spill that fails starts Straum over.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use straum_app::{AnswerError, App, Error, Spilling, Step, Writer};
use straum_domain::{Ask, Sealed, SealedId, Value};

use crate::common::{
    DAY, Harness, LIMITS, RANGE, ask, by_borough, count, harness, int, login, reference_answer,
    spec, trip, watched, zone,
};
use crate::fake::{Database, Fake, Write, id};
use crate::spill::MemorySpill;

/// Long enough that no bucket here is old enough to seal.
const NEVER: Duration = Duration::from_secs(1_000 * 365 * DAY as u64);

/// A writer over a fake database, with the spill it seals buckets into.
struct Spilled {
    database: Rc<RefCell<Database>>,
    writer: Writer<Fake>,
    spill: Arc<MemorySpill>,
}

/// A writer that seals buckets once they ended `seal_after` ago.
fn spilling(seal_after: Duration) -> Spilled {
    let spill = Arc::new(MemorySpill::default());
    let spilling = Spilling {
        spill: spill.clone(),
        seal_after,
    };
    let Harness { database, writer } = harness(App::new(spec(), LIMITS, Some(spilling)));
    Spilled {
        database,
        writer,
        spill,
    }
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
fn started(seal_after: Duration) -> Spilled {
    let Spilled {
        database,
        mut writer,
        spill,
    } = spilling(seal_after);
    database
        .borrow_mut()
        .commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10), login(1, 1)]);
    writer.step().unwrap();
    Spilled {
        database,
        writer,
        spill,
    }
}

#[test]
fn old_buckets_are_sealed_and_answer_as_before() {
    let Spilled {
        database,
        writer,
        spill,
    } = started(Duration::ZERO);
    assert_eq!(spill.ids(), [day_of_logins(), hour_of_trips()]);
    for fact in ["trips", "logins"] {
        assert_eq!(
            by_borough(writer.app(), fact),
            reference_answer(&database.borrow(), &ask(fact, &[fact], &["borough"])).rows,
            "{fact}"
        );
    }
    assert_eq!(spill.gets(), 2);
}

#[test]
fn buckets_younger_than_seal_after_stay() {
    let Spilled { writer, spill, .. } = started(NEVER);
    assert_eq!(spill.ids(), []);
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(1)));
    assert_eq!(spill.gets(), 0);
}

#[test]
fn a_change_unseals_the_buckets_it_falls_in() {
    let Spilled {
        database,
        mut writer,
        spill,
    } = started(Duration::ZERO);
    let lsn = database.borrow_mut().commit(vec![trip(2, 1, 1, 5)]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    // Back in memory until the writer next seals.
    assert_eq!(spill.ids(), [day_of_logins()]);
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(2)));
    let lsn = database.borrow_mut().commit(vec![Write::Delete {
        table: "trips",
        key: int(1),
    }]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(1)));
    assert_eq!(count(writer.app(), "logins"), Ok(Value::Int(1)));
}

#[test]
fn each_start_empties_the_spill() {
    let Spilled {
        database,
        mut writer,
        spill,
    } = spilling(NEVER);
    (spill.buckets.lock().unwrap()).insert(hour_of_trips(), stale_bucket());
    writer.step().unwrap();
    assert_eq!(spill.ids(), []);
    database.borrow_mut().drop_connection = true;
    database.borrow_mut().commit(vec![trip(1, 1, 1, 10)]);
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
    let Harness {
        database,
        mut writer,
    } = watched(app, move |app, _| {
        log.borrow_mut().push(app.status().sealed);
    });
    database
        .borrow_mut()
        .commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10), login(1, 1)]);
    writer.step().unwrap();
    assert_eq!(writer.app().status().sealed, Some(2));

    database.borrow_mut().drop_connection = true;
    writer.step().unwrap_err();
    // The failure leaves the store as it was until the next start.
    assert_eq!(writer.app().status().sealed, Some(2));
    seen.borrow_mut().clear();
    writer.step().unwrap();
    assert_eq!(*seen.borrow(), [Some(0); 3]);
    assert_eq!(writer.app().status().sealed, Some(2));
}

/// A bucket no store sealed: a cell keyed by codes it never gave.
fn stale_bucket() -> Sealed {
    Sealed {
        id: hour_of_trips(),
        keys: vec![7, 7],
        counts: vec![1],
        columns: Vec::new(),
    }
}

#[test]
fn a_truncate_removes_its_sealed_buckets_from_the_spill() {
    let Spilled {
        database,
        mut writer,
        spill,
    } = started(Duration::ZERO);
    assert_eq!(writer.app().status().sealed, Some(2));
    database
        .borrow_mut()
        .commit(vec![Write::Truncate { table: "trips" }]);
    writer.step().unwrap();
    assert_eq!(spill.ids(), [day_of_logins()]);
    assert_eq!(writer.app().status().sealed, Some(1));
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(0)));
    assert_eq!(count(writer.app(), "logins"), Ok(Value::Int(1)));
}

#[test]
fn a_truncate_the_spill_cannot_take_starts_over_without_a_leak() {
    let Spilled {
        database,
        mut writer,
        spill,
    } = started(Duration::ZERO);
    // A dim has no buckets: its truncate leaves the spill alone.
    database
        .borrow_mut()
        .commit(vec![Write::Truncate { table: "zones" }]);
    writer.step().unwrap();
    assert_eq!(spill.ids(), [day_of_logins(), hour_of_trips()]);

    database
        .borrow_mut()
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
    let Spilled {
        database,
        mut writer,
        ..
    } = started(Duration::ZERO);
    let status = writer.app().status();
    assert_eq!((status.sealed, status.unseals), (Some(2), 0));
    database
        .borrow_mut()
        .commit(vec![trip(2, 1, 1, 5), login(2, 1)]);
    writer.step().unwrap();
    let status = writer.app().status();
    assert_eq!((status.sealed, status.unseals), (Some(0), 2));
}

#[test]
fn a_spill_that_fails_starts_over() {
    let Spilled {
        database,
        mut writer,
        spill,
    } = spilling(Duration::ZERO);
    database
        .borrow_mut()
        .commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10)]);
    spill.break_it(true);
    let error = writer.step().unwrap_err();
    assert!(matches!(error, Error::Spill(_)), "{error:?}");
    assert_eq!(error.to_string(), "spill: broken");
    assert!(!writer.app().status().ready);
    spill.break_it(false);
    writer.step().unwrap();
    assert_eq!(spill.ids(), [hour_of_trips()]);

    // Unsealing for a change fails too.
    database.borrow_mut().commit(vec![trip(2, 1, 1, 5)]);
    spill.break_it(true);
    assert!(matches!(writer.step(), Err(Error::Spill(_))));
    spill.break_it(false);
    writer.step().unwrap();
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(2)));
}

#[test]
fn an_ask_fails_when_a_sealed_bucket_cannot_be_read() {
    let Spilled {
        database,
        mut writer,
        spill,
    } = spilling(Duration::ZERO);
    database
        .borrow_mut()
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
    let Spilled {
        database,
        mut writer,
        spill,
    } = spilling(Duration::ZERO);
    database
        .borrow_mut()
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
    database.borrow_mut().commit(vec![trip(2, 1, 1, 5)]);
    let error = writer.step().unwrap_err();
    assert!(matches!(error, Error::Sealed(_)), "{error:?}");
    assert_eq!(error.to_string(), reason);
    // The start after holds every row again.
    writer.step().unwrap();
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(2)));
}
