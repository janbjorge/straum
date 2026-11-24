//! A view that keeps its hours for a time, loaded from PostgreSQL: the
//! load's snapshot time sets the hours' horizon, so trips older than two
//! days back from it answer only by the day. The trips run ten days back
//! from the server's `now()`, so the horizon falls among them. Allowed
//! asks equal the view body run in PostgreSQL; an ask by the hour from
//! before the horizon is refused, naming it. Both streams read the
//! snapshot's time, so the test runs over each.

use straum_app::{AnswerError, App, Writer};
use straum_domain::{Ask, AskError, AskKey, Width};

use super::references::*;
use super::support::*;
use crate::Postgres;

/// Trips by kind, hours kept two days, days for good. `$S` is the
/// test's schema.
const KEEP_SPEC: &str = "
CREATE VIEW trips AS
SELECT buckets(f.at, keep('1 hour', '2 days'), '1 day') AS bucket,
       f.kind AS kind,
       count(*) AS trips,
       sum(f.fare) AS fare
FROM $S.trips AS f
GROUP BY 1, 2;
";

const KEEP_TABLES: &str = "
CREATE TABLE $S.trips (id bigint PRIMARY KEY, at timestamptz, kind text, fare numeric(10, 2));
ALTER TABLE $S.trips REPLICA IDENTITY FULL;
CREATE PUBLICATION $S FOR TABLE $S.trips;
";

/// 400 trips, one every 37 minutes back from `now()`: about ten days.
const TRIPS: &str = "
INSERT INTO $S.trips
SELECT g, now() - g * interval '37 minutes',
       CASE WHEN g % 3 = 0 THEN 'pool' ELSE 'cab' END,
       (g % 50) + 0.25
  FROM generate_series(1, 400) g;
";

const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;

/// The server's clock, in whole Unix seconds.
fn server_now(db: &mut Db) -> i64 {
    let rows = db.rows("SELECT floor(extract(epoch FROM now()))::int8");
    rows[0][0].as_deref().unwrap().parse().unwrap()
}

/// The hours' horizon the app names when asked by the hour from `from`;
/// any other outcome fails the test.
fn hour_horizon(app: &App, from: i64, to: i64) -> i64 {
    let mut old = ask("trips", &["trips"], &["bucket"]);
    old.from = Some(from);
    old.to = Some(to);
    let errors = match app.ask(&old) {
        Err(AnswerError::Invalid(errors)) => errors,
        other => panic!("expected a refusal, got {other:?}"),
    };
    let [
        AskError::PastRetention {
            width: Width { seconds: 3_600 },
            kept_from,
        },
    ] = errors[..]
    else {
        panic!("refused with {errors:?}")
    };
    kept_from
}

/// `ask` with both ends open, or from `from` on.
fn from_on(ask: Ask, from: Option<i64>) -> Ask {
    Ask {
        from,
        to: None,
        ..ask
    }
}

#[test]
fn a_kept_width_loads_with_its_horizon_at_the_snapshot() {
    let pg = postgres();
    loads_with_its_horizon(&pg, "retention", Db::writer);
}

/// The physical stream takes its snapshot after the stream is consistent,
/// on a server that has no logical decoding.
#[test]
fn a_kept_width_loads_with_its_horizon_at_the_snapshot_on_the_physical_wal() {
    let pg = postgres_at_replica(&tag());
    loads_with_its_horizon(&pg, "retention_physical", Db::physical_writer);
}

/// Loads the trips through the writer `writer_of` makes, then checks the
/// hours' horizon lies two days back from the server's clock as the load
/// ran, and that the asks either side of it answer or are refused.
fn loads_with_its_horizon(
    pg: &PostgresContainer,
    name: &str,
    writer_of: fn(&Db, &str) -> Writer<Postgres>,
) {
    let mut db = Db::new(pg, name, KEEP_TABLES);
    db.run(TRIPS);
    let before = server_now(&mut db);
    let mut writer = writer_of(&db, KEEP_SPEC);
    started(&mut writer);
    let after = server_now(&mut db) + 1;
    let app = writer.app();
    let first_hour = (before - 5 * DAY).div_euclid(HOUR) * HOUR;
    let kept_from = hour_horizon(app, first_hour, first_hour + DAY);
    let earliest = (before - 2 * DAY).div_euclid(DAY) * DAY;
    let latest = (after - 2 * DAY).div_euclid(DAY) * DAY;
    assert!(
        (earliest..=latest).contains(&kept_from),
        "{kept_from} outside {earliest}..={latest}"
    );
    let body = body(KEEP_SPEC, "trips");
    let by_kind = Reference {
        sql: format!("SELECT kind, sum(trips)::int8, sum(fare) FROM ({body}) v GROUP BY 1"),
        ask: from_on(ask("trips", &["trips", "fare"], &["kind"]), None),
        scales: vec![0, 0, 2],
    };
    let recent_hours = Reference {
        sql: format!(
            "SELECT extract(epoch FROM bucket)::int8, kind, trips, fare FROM ({body}) v
              WHERE bucket >= to_timestamp({kept_from})"
        ),
        ask: from_on(
            ask("trips", &["trips", "fare"], &["bucket", "kind"]),
            Some(kept_from),
        ),
        scales: vec![0, 0, 0, 2],
    };
    let by_day = Reference {
        sql: "SELECT extract(epoch FROM date_bin('1 day', f.at, 'epoch'))::int8, count(*)
                FROM $S.trips f GROUP BY 1"
            .to_owned(),
        ask: from_on(
            Ask {
                keys: vec![AskKey::WiderBucket {
                    name: id("bucket"),
                    width: Width { seconds: 86_400 },
                }],
                ..ask("trips", &["trips"], &[])
            },
            None,
        ),
        scales: vec![0, 0],
    };
    compare_after(&mut db, app, &[by_kind, recent_hours, by_day], "the load");
    assert_eq!(
        hour_horizon(app, kept_from - HOUR, kept_from + HOUR),
        kept_from
    );
}
