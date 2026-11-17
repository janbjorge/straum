//! Dates and uuids as keys, joined and filtered: the load reads them in
//! binary, the logical stream as text and the physical stream off the
//! heap pages, and every answer equals the view body PostgreSQL runs.

use straum_app::{Step, Writer};
use straum_domain::Literal;

use super::references::*;
use super::support::*;
use crate::source::Postgres;

/// Riders keyed by uuid, one held twice and one never; rides by date and
/// rider. A boolean before each date and uuid moves them off their
/// alignment, so the heap's padding is read as PostgreSQL lays it out.
const TABLES: &str = "
CREATE TABLE $S.riders (id uuid, joined date);
CREATE TABLE $S.rides (
    id int PRIMARY KEY,
    at timestamptz,
    paid boolean,
    day date,
    shared boolean,
    rider uuid,
    fare int
);
ALTER TABLE $S.riders REPLICA IDENTITY FULL;
ALTER TABLE $S.rides REPLICA IDENTITY FULL;
CREATE PUBLICATION $S FOR TABLE $S.riders, $S.rides;
";

const SPEC: &str = "
CREATE VIEW rides WITH (buckets = '1 hour, 1 day') AS
SELECT date_bin('1 hour', r.at, timestamptz 'epoch') AS bucket,
       r.day AS day,
       r.rider AS rider,
       p.joined AS joined,
       count(*) AS rides,
       sum(r.fare) AS fare,
       count(DISTINCT r.day) AS days,
       count(DISTINCT r.rider) AS riders,
       count(r.day) AS dated,
       count(*) FILTER (WHERE r.day = '2024-01-15') AS on_the_15th,
       count(*) FILTER (WHERE p.joined IN ('0044-03-15 BC', 'infinity')) AS odd_joins
FROM $S.rides AS r
LEFT OUTER JOIN $S.riders AS p ON p.id = r.rider
GROUP BY 1, 2, 3, 4;
";

const MEASURES: [&str; 7] = [
    "rides",
    "fare",
    "days",
    "riders",
    "dated",
    "on_the_15th",
    "odd_joins",
];

/// The days rides fall on: past four digits of year, before Christ, a
/// leap day and both infinities.
const DAYS: [&str; 8] = [
    "'2024-01-15'",
    "'2024-01-16'",
    "'2000-02-29'",
    "'0044-03-15 BC'",
    "'12345-06-07'",
    "'infinity'",
    "'-infinity'",
    "NULL",
];

/// The riders; the last has no row in `riders`.
const RIDERS: [&str; 5] = [
    "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
    "00000000-0000-0000-0000-000000000000",
    "ffffffff-ffff-ffff-ffff-ffffffffffff",
    "7f000001-0000-4000-8000-00000000beef",
    "deadbeef-dead-beef-dead-beefdeadbeef",
];

/// A ride of `id`, its values picked by the id.
fn ride(id: u64) -> String {
    let hour = id * 7 % 48;
    let day = DAYS[usize::try_from(id % 8).unwrap()];
    let rider = match id % 6 {
        5 => "NULL".to_owned(),
        index => format!("'{}'", RIDERS[usize::try_from(index).unwrap()]),
    };
    format!(
        "INSERT INTO $S.rides VALUES ({id}, to_timestamp({START}) + interval '{hour} hours', \
         {}, {day}, {}, {rider}, {})",
        id.is_multiple_of(2),
        id.is_multiple_of(3),
        id % 13
    )
}

/// Each ask's SQL over the view body or the rides themselves: every key
/// and measure, then a day, a rider and a join date asked for by their
/// text, as a client sends them.
fn oracles() -> Vec<Reference> {
    let all = Reference {
        sql: format!(
            "SELECT extract(epoch FROM bucket)::int8, day, rider, joined, rides, fare, days,
                    riders, dated, on_the_15th, odd_joins
               FROM ({}) v",
            body(SPEC, "rides")
        ),
        ask: ask("rides", &MEASURES, &["bucket", "day", "rider", "joined"]),
        scales: vec![0; 11],
    };
    let by_day = Reference {
        // Over the view body: the rider held twice counts twice.
        sql: format!(
            "SELECT day, sum(rides)::int8 FROM ({}) v WHERE day = '2024-01-15' GROUP BY 1",
            body(SPEC, "rides")
        ),
        ask: where_is(
            ask("rides", &["rides"], &["day"]),
            "day",
            Literal::Text("2024-01-15".into()),
        ),
        scales: vec![0; 2],
    };
    let by_rider = Reference {
        sql: format!(
            "SELECT r.rider, count(DISTINCT r.day) FROM $S.rides r
              WHERE r.rider = '{}' GROUP BY 1",
            RIDERS[0]
        ),
        ask: where_is(
            ask("rides", &["days"], &["rider"]),
            "rider",
            Literal::Text(RIDERS[0].into()),
        ),
        scales: vec![0; 2],
    };
    let by_joined = Reference {
        sql: "SELECT p.joined, count(*) FROM $S.rides r JOIN $S.riders p ON p.id = r.rider
               WHERE p.joined = 'infinity' GROUP BY 1"
            .into(),
        ask: where_is(
            ask("rides", &["rides"], &["joined"]),
            "joined",
            Literal::Text("infinity".into()),
        ),
        scales: vec![0; 2],
    };
    vec![all, by_day, by_rider, by_joined]
}

/// Commits that move rides across days and riders, and riders across
/// join dates.
fn changes() -> Vec<String> {
    let mut changes = Vec::new();
    for id in 200..220 {
        changes.push(ride(id));
    }
    let statements = [
        "UPDATE $S.rides SET day = '2024-01-15' WHERE id % 5 = 0",
        "UPDATE $S.rides SET day = 'infinity', paid = NOT paid WHERE id % 7 = 0",
        "UPDATE $S.rides SET rider = 'deadbeef-dead-beef-dead-beefdeadbeef' WHERE id % 4 = 1",
        "UPDATE $S.rides SET rider = NULL, day = NULL WHERE id % 9 = 0",
        "DELETE FROM $S.rides WHERE id % 11 = 0",
        "INSERT INTO $S.riders VALUES ('deadbeef-dead-beef-dead-beefdeadbeef', '2024-01-16')",
        "UPDATE $S.riders SET joined = 'infinity' WHERE joined = '2000-02-29'",
        "DELETE FROM $S.riders WHERE id = '00000000-0000-0000-0000-000000000000' \
         AND joined IS NULL",
    ];
    for statement in statements {
        changes.push(statement.to_owned());
    }
    changes
}

/// PostgreSQL's layout of the two types, which the heap reader follows:
/// a date four bytes by value aligned to four, a uuid sixteen by
/// reference aligned to one.
fn assert_layouts(db: &mut Db) {
    let layouts = db.rows(
        "SELECT typname, typlen, typbyval, typalign FROM pg_type
          WHERE oid IN ('date'::regtype, 'uuid'::regtype) ORDER BY typname",
    );
    let expected = [["date", "4", "t", "i"], ["uuid", "16", "f", "c"]];
    let mut found = Vec::new();
    for row in layouts {
        let mut fields = Vec::new();
        for field in row {
            fields.push(field.unwrap());
        }
        found.push(fields);
    }
    assert_eq!(found, expected);
}

/// Dates and uuids answer as PostgreSQL's view does, logical and
/// physical alike, through rides and riders that come, move and go.
#[test]
fn dates_and_uuids_match_postgresql() {
    let pg = postgres();
    let mut db = Db::new(&pg, "typed", TABLES);
    assert_layouts(&mut db);
    db.run(&format!(
        "INSERT INTO $S.riders VALUES ('{}', '2024-01-15'), ('{}', '0044-03-15 BC'),
                                      ('{}', NULL), ('{}', '2000-02-29'), ('{}', 'infinity')",
        RIDERS[0], RIDERS[1], RIDERS[1], RIDERS[2], RIDERS[3]
    ));
    for id in 0..200 {
        db.run(&ride(id));
    }
    let mut writers: [Writer<Postgres>; 2] = [db.writer(SPEC), db.physical_writer(SPEC)];
    for writer in &mut writers {
        assert!(matches!(writer.step(), Ok(Step::Started(_))));
    }
    let references = oracles();
    for writer in &writers {
        compare(&mut db, writer.app(), &references).expect("ready");
    }
    for change in changes() {
        db.run(&change);
        for writer in &mut writers {
            applied(writer);
            compare_after(&mut db, writer.app(), &references, &change);
        }
    }
    for writer in &writers {
        assert_eq!(writer.app().status().starts, 1);
    }
}
