//! The taxi month end to end: load January's taxi trips, stream changes
//! and compare the whole `taxi.sql` view with PostgreSQL each time.
//! Slow, so ignored by default.

use std::path::Path;
use std::time::Instant;

use straum_app::{App, Step, Writer};

use super::references::*;
use super::support::*;
use crate::source::{Postgres, Replication};
use crate::wire::{Connection, ConnectionKind};

/// The taxi month through straum: load, stream inserts and dim changes, read
/// the fact again after an update, and every time compare the whole
/// `taxi.sql` view with PostgreSQL. `bench/load_taxi_trips.py` puts the month
/// in PostgreSQL, so it needs uv. Slow, so ignored; run
/// `cargo test -p straum-pg --release taxi_month -- --ignored --nocapture`.
#[test]
#[ignore = "slow, and needs uv to load the taxi month"]
fn taxi_month() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let spec_text =
        std::fs::read_to_string(root.join("crates/straum-notation/tests/taxi.sql")).unwrap();
    let pg = taxi_month_postgres(&root);
    let mut sql = Connection::connect(&pg.dsn, ConnectionKind::Sql).unwrap();
    sql.execute(
        "ALTER TABLE yellow_trips REPLICA IDENTITY FULL;
         ALTER TABLE taxi_zones REPLICA IDENTITY FULL;
         CREATE PUBLICATION straum FOR TABLE taxi_zones, yellow_trips;
         ANALYZE;",
    )
    .unwrap();
    let references = taxi_month_references(&spec_text);
    let spec = straum_notation::parse(&spec_text).unwrap();
    let mut writer = Writer::new(
        App::new(spec, LIMITS),
        Postgres::new(
            pg.dsn.clone(),
            "straum",
            ROW_LIMIT,
            MAX_RETAINED_WAL_BYTES,
            Replication::Logical,
        ),
    );
    let clock = Instant::now();
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    eprintln!("loaded in {:.1?}", clock.elapsed());
    check_taxi_month(&mut sql, writer.app(), &references, "after the load");

    // Inserts in 200 transactions of 50, a day later than rows already there.
    let clock = Instant::now();
    for remainder in 0..200 {
        sql.execute(&format!(
            "INSERT INTO yellow_trips (vendor_id, pickup_at, pu_location_id, do_location_id,
                                      payment_type, fare_amount, tip_amount, trip_distance)
             SELECT vendor_id, pickup_at + interval '1 day', pu_location_id, do_location_id,
                    payment_type, fare_amount + 1, tip_amount, trip_distance
               FROM yellow_trips WHERE id % 10007 = {remainder} LIMIT 50"
        ))
        .unwrap();
    }
    for _ in 0..200 {
        applied(&mut writer);
    }
    eprintln!("streamed 200 commits in {:.1?}", clock.elapsed());
    check_taxi_month(&mut sql, writer.app(), &references, "after the stream");

    // An update takes the old row out of its cell.
    sql.execute("UPDATE yellow_trips SET tip_amount = tip_amount + 1 WHERE id = 1")
        .unwrap();
    applied(&mut writer);
    check_taxi_month(&mut sql, writer.app(), &references, "after an update");
    change_taxi_zones(&mut sql, &mut writer, &references);
    assert_eq!(writer.app().status().starts, 1);
}

/// The shared memory PostgreSQL's container gets for the month: 1 GiB,
/// where Docker's default is 64 MiB.
const SHARED_MEMORY_BYTES: u64 = 1 << 30;

/// PostgreSQL 18 with the taxi month, which `bench/load_taxi_trips.py`
/// puts there.
fn taxi_month_postgres(root: &Path) -> PostgresContainer {
    let pg = plain(
        image("18")
            .with_shm_size(SHARED_MEMORY_BYTES)
            .with_cmd(&["-c", "wal_level=logical"]),
    );
    let clock = Instant::now();
    load_taxi_month(root, &pg.container);
    let mut sql = Connection::connect(&pg.dsn, ConnectionKind::Sql).unwrap();
    let trips = sql.rows("SELECT count(*) FROM yellow_trips").unwrap();
    eprintln!("copied {:?} trips in {:.1?}", trips[0][0], clock.elapsed());
    pg
}

/// The whole `taxi.sql` view, and a coarse ask, the use case: it reads
/// every cell, as the first does, but answers few groups.
fn taxi_month_references(spec_text: &str) -> [Reference; 2] {
    let start = spec_text.find("SELECT").unwrap();
    let body = &spec_text[start..spec_text.rfind(';').unwrap()];
    let all = Reference {
        sql: format!(
            "SELECT extract(epoch FROM bucket)::int8, payment_type, vendor_id, pu_location_id,
                    do_location_id, borough, zone, service_zone, trips, fare, tip_max,
                    round(distance, 6), card, manhattan_pickups
               FROM ({body}) v"
        ),
        ask: ask(
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
        ),
        scales: vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 2, 2, 0, 0],
    };
    let coarse = Reference {
        sql: format!(
            "SELECT borough,
                    sum(trips), sum(fare), max(tip_max), sum(card), sum(manhattan_pickups)
               FROM ({body}) v
              GROUP BY 1"
        ),
        ask: ask(
            "trips",
            &["trips", "fare", "tip_max", "card", "manhattan_pickups"],
            &["borough"],
        ),
        scales: vec![0, 0, 2, 2, 0, 0],
    };
    [all, coarse]
}

/// Dims change: a zone renamed, another moved to a new key, then a trip
/// in the renamed zone. Each is one commit applied; nothing starts over.
fn change_taxi_zones(
    sql: &mut Connection,
    writer: &mut Writer<Postgres>,
    references: &[Reference],
) {
    let changes = [
        ZoneChange {
            what: "a zone renamed",
            statement: "UPDATE taxi_zones SET zone = zone || ' (renamed)' WHERE location_id = 161",
            when: "after a zone was renamed",
        },
        ZoneChange {
            what: "a zone moved",
            statement: "UPDATE taxi_zones SET location_id = 999 WHERE location_id = 264",
            when: "after a zone moved",
        },
        ZoneChange {
            what: "a trip in the renamed zone",
            statement: "UPDATE yellow_trips SET tip_amount = tip_amount + 1
              WHERE id = (SELECT min(id) FROM yellow_trips WHERE pu_location_id = 161)",
            when: "after the zones changed",
        },
    ];
    for ZoneChange {
        what,
        statement,
        when,
    } in changes
    {
        sql.execute(statement).unwrap();
        let clock = Instant::now();
        applied(writer);
        eprintln!("applied {what} in {:.1?}", clock.elapsed());
        check_taxi_month(sql, writer.app(), references, when);
    }
}

/// A change to the taxi tables: what it is, its SQL, and when the check
/// after it runs.
// `'static`: each is a literal written here.
struct ZoneChange {
    what: &'static str,
    statement: &'static str,
    when: &'static str,
}

/// Compares each of `references` with PostgreSQL, the second by borough.
fn check_taxi_month(sql: &mut Connection, app: &App, references: &[Reference], when: &str) {
    let [all, coarse] = references else {
        panic!("two references");
    };
    compare_timed(sql, app, all, when);
    compare_timed(sql, app, coarse, &format!("{when}, by borough"));
}

/// Compares `reference` with PostgreSQL, timing both. Straum's time is the
/// time `ask` holds the read lock.
fn compare_timed(sql: &mut Connection, app: &App, reference: &Reference, when: &str) {
    let clock = Instant::now();
    let answered = app.ask(&reference.ask).unwrap();
    let asked = clock.elapsed();
    let mut got = printed(&answered.answer, &reference.scales);
    let clock = Instant::now();
    let mut expected = sql.rows(&reference.sql).unwrap();
    let queried = clock.elapsed();
    got.sort();
    expected.sort();
    let mut differ = None;
    for (ours, theirs) in got.iter().zip(&expected) {
        if ours != theirs {
            differ = Some((ours, theirs));
            break;
        }
    }
    let (ours, theirs) = match differ {
        Some((ours, theirs)) => (Some(ours), Some(theirs)),
        None => (None, None),
    };
    assert!(
        got.len() == expected.len() && differ.is_none(),
        "{when}: {} rows from Straum, {} from PostgreSQL; first difference: {ours:?} vs {theirs:?}",
        got.len(),
        expected.len(),
    );
    eprintln!(
        "{when}: {} groups equal; Straum asked in {asked:.1?}, PostgreSQL in {queried:.1?}",
        got.len()
    );
}
