//! Commits placed at one exact point of a start: the writer reports each
//! table it loads, and on the report of the last dim the test writes on
//! another connection, before any view loads. Those commits land after
//! the load's snapshot, so the stream must bring each of them once, on
//! either stream.

use std::cell::RefCell;

use straum_app::Writer;

use super::references::*;
use super::streams::*;
use super::support::*;
use crate::source::Postgres;

thread_local! {
    /// The connection the writer's report writes on, until it has.
    static BETWEEN_LOADS: RefCell<Option<Db>> = const { RefCell::new(None) };
}

/// On the logical stream, inserts, an update and a delete on the fact
/// table and a change to a dim row, committed after the dims load and
/// before the views do, each reach the views once.
#[test]
fn applies_writes_between_loads_once() {
    follow_writes_between_loads(&postgres(), Db::writer);
}

/// As [`applies_writes_between_loads_once`], on the physical stream.
#[test]
fn applies_writes_between_loads_once_physically() {
    follow_writes_between_loads(&postgres_at_replica(&tag()), Db::physical_writer);
}

/// Starts the writer `writer_for` makes over the taxi tables on `pg`,
/// with writes run between its loads, then checks its views equal SQL's.
fn follow_writes_between_loads(
    pg: &PostgresContainer,
    writer_for: fn(&Db, &str) -> Writer<Postgres>,
) {
    let mut db = Db::new(pg, "between", TABLES);
    let mut rng = Rng::new(51);
    insert_zones(&mut db, &mut rng);
    for id in 0..40 {
        db.run(&rng.trip(id));
    }
    db.run(&format!(
        "INSERT INTO $S.logins VALUES (1, to_timestamp({START}), 1), (2, to_timestamp({START}), 3)"
    ));
    BETWEEN_LOADS.set(Some(db.another_connection()));
    let mut writer = writer_for(&db, SPEC).reporting(write_after_dims);
    started(&mut writer);
    let written = BETWEEN_LOADS.with_borrow(Option::is_none);
    assert!(written, "no write ran between the loads");
    let mut next = 10_000;
    catch_up(&mut db, &mut writer, &mut rng, &mut next);
}

/// On the report that the dims have loaded, runs the writes on the
/// connection [`BETWEEN_LOADS`] holds, once.
fn write_after_dims(line: &str) {
    if line.starts_with("loaded dim zones ") {
        BETWEEN_LOADS.with_borrow_mut(write_during_load);
    }
}

/// Inserts trips, alone and two in one commit, moves one trip to another
/// zone, deletes one, changes a zone the trips join to, and adds a login;
/// takes the connection, so the writes run once.
fn write_during_load(connection: &mut Option<Db>) {
    let Some(mut db) = connection.take() else {
        return;
    };
    let at = START + 600;
    db.run(&format!(
        "INSERT INTO $S.trips VALUES (1000, to_timestamp({at}), 'cab', 1, 12.50, 'ann', 3)"
    ));
    db.run(&format!(
        "BEGIN;
         INSERT INTO $S.trips VALUES (1001, to_timestamp({at}), 'pool', 2, 7.25, 'bo', 1);
         INSERT INTO $S.trips VALUES (1002, to_timestamp({at}), 'van', 3, 30.00, NULL, 0);
         COMMIT"
    ));
    db.run("UPDATE $S.trips SET fare = 99.99, zone_id = 2 WHERE id = 1");
    db.run("DELETE FROM $S.trips WHERE id = 2");
    db.run("UPDATE $S.zones SET borough = 'Manhattan', toll = 9.5 WHERE id = 3");
    db.run(&format!(
        "INSERT INTO $S.logins VALUES (3, to_timestamp({at}), 3)"
    ));
}
