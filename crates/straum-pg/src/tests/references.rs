//! The specs and tables the tests follow, and the references they are
//! checked against: for each ask, the SQL PostgreSQL runs over the same
//! rows. `compare` asks Straum, runs the SQL and asserts both print the
//! same rows.

use straum_app::{AnswerError, App};
use straum_domain::{Answer, AnswerRow, Ask, Comparison, Literal};

use super::support::*;

/// The spec the taxi-shaped tests follow. `$S` is the test's schema.
pub(super) const SPEC: &str = "
CREATE VIEW trips AS
SELECT buckets(f.at, '1 hour', '1 day') AS bucket,
       f.kind AS kind,
       f.zone_id AS zone_id,
       pickup.borough AS borough,
       count(*) AS trips,
       sum(f.fare) AS fare,
       max(f.fare) AS fare_max,
       avg(f.fare) AS fare_avg,
       count(DISTINCT f.rider) AS riders,
       count(f.rider) AS named,
       sum(f.tip) AS tips,
       count(*) FILTER (WHERE pickup.borough = 'Manhattan') AS manhattan,
       count(*) FILTER (WHERE f.zone_id IN (1, 2)) AS near,
       percentile_disc(0.5) WITHIN GROUP (ORDER BY f.fare) AS fare_median,
       percentile_disc(0.9) WITHIN GROUP (ORDER BY f.tip) AS tip_p90,
       var_samp(f.fare) AS fare_var,
       stddev_pop(f.fare) AS fare_sd_pop,
       var_pop(f.tip) AS tip_var_pop,
       stddev_samp(f.tip) AS tip_sd,
       mode() WITHIN GROUP (ORDER BY f.fare) AS fare_mode,
       mode() WITHIN GROUP (ORDER BY f.tip) AS tip_mode,
       sum(DISTINCT f.tip) AS distinct_tips,
       avg(DISTINCT f.fare) AS distinct_fare_avg
FROM $S.trips AS f
LEFT OUTER JOIN $S.zones AS pickup ON pickup.id = f.zone_id
GROUP BY 1, 2, 3, 4;

CREATE VIEW logins AS
SELECT buckets(l.at, '1 day', '7 days') AS bucket,
       l.zone_id AS zone_id,
       pickup.toll AS toll,
       count(*) AS logins
FROM $S.logins AS l
LEFT OUTER JOIN $S.zones AS pickup ON pickup.id = l.zone_id
GROUP BY 1, 2, 3;
";

/// The taxi tables: zones, trips and logins, published whole.
pub(super) const TABLES: &str = "
CREATE TABLE $S.zones (id int PRIMARY KEY, borough text, toll numeric(4, 1));
CREATE TABLE $S.trips (
    id bigint PRIMARY KEY,
    at timestamptz,
    kind text,
    zone_id int,
    fare numeric(10, 2),
    rider text,
    tip int
);
ALTER TABLE $S.trips REPLICA IDENTITY FULL;
CREATE TABLE $S.logins (id bigint PRIMARY KEY, at timestamptz, zone_id int);
ALTER TABLE $S.logins REPLICA IDENTITY FULL;
ALTER TABLE $S.zones REPLICA IDENTITY FULL;
CREATE PUBLICATION $S FOR TABLE $S.zones, $S.trips, $S.logins;
";

/// A warehouse-shaped spec: tasks per hour, organization and task group
/// over a `timestamp` column, which has no zone, with the task groups a
/// dim keyed by two columns, one of them grouped by too. `$S` is the
/// test's schema.
pub(super) const TASKS_SPEC: &str = "
CREATE VIEW tasks AS
SELECT buckets(f.created, '1 hour', '1 day') AS bucket,
       f.organization_id AS organization_id,
       f.taskgroup_id AS taskgroup_id,
       (f.completed IS NOT NULL) AS done,
       g.category::text AS category,
       f.kind::text AS kind,
       count(*) AS tasks,
       sum(f.picks) AS picks,
       max(f.picks) AS picks_max,
       count(*) FILTER (WHERE g.category::text = 'pick') AS pick_tasks,
       count(*) FILTER (WHERE f.completed IS NOT NULL) AS completed_tasks,
       count(*) FILTER (WHERE f.kind::text = 'shelf') AS shelf_tasks
FROM $S.tasks AS f
LEFT OUTER JOIN $S.taskgroups AS g
       ON g.organization_id = f.organization_id AND g.taskgroup_id = f.taskgroup_id
GROUP BY 1, 2, 3, 4, 5, 6;
";

/// The task tables keyed by the group's category alone: the group itself
/// is not selected, so the cells are keyed by the category's few values.
pub(super) const PICKS_SPEC: &str = "
CREATE VIEW picks AS
SELECT buckets(f.created, '1 hour', '1 day') AS bucket,
       f.organization_id AS organization_id,
       g.category::text AS category,
       (f.completed IS NOT NULL) AS done,
       count(*) AS tasks,
       sum(f.picks) AS picks,
       count(*) FILTER (WHERE g.category::text = 'pick') AS pick_tasks
FROM $S.tasks AS f
LEFT OUTER JOIN $S.taskgroups AS g
       ON g.organization_id = f.organization_id AND g.taskgroup_id = f.taskgroup_id
GROUP BY 1, 2, 3, 4;
";

/// The warehouse tables: task groups and tasks, published whole.
pub(super) const TASKS: &str = "
CREATE TYPE $S.category AS ENUM ('pick', 'pack', 'count', 'other');
CREATE TYPE $S.kind AS ENUM ('grid', 'shelf');
CREATE TABLE $S.taskgroups (
    organization_id int,
    taskgroup_id int,
    category $S.category,
    PRIMARY KEY (organization_id, taskgroup_id)
);
CREATE TABLE $S.tasks (
    task_id int PRIMARY KEY,
    organization_id int,
    taskgroup_id int,
    created timestamp NOT NULL,
    completed timestamp,
    picks int,
    kind $S.kind NOT NULL DEFAULT 'grid'
);
ALTER TABLE $S.tasks REPLICA IDENTITY FULL;
ALTER TABLE $S.taskgroups REPLICA IDENTITY FULL;
CREATE PUBLICATION $S FOR TABLE $S.taskgroups, $S.tasks;
";

/// What an ask must equal in PostgreSQL: the SQL, the ask, and the scale
/// each printed column is compared at.
pub(super) struct Reference {
    pub(super) sql: String,
    pub(super) ask: Ask,
    pub(super) scales: Vec<u32>,
}

/// The taxi spec's references: trips by bucket, kind, zone and borough; logins
/// by day, zone and toll; then trips without the bucket from 5 hours before
/// `START` to 29 after, which reads the day bucket `START` begins and the
/// hours at either end, against the raw rows. `fare_scale` follows `fare`'s
/// type.
pub(super) fn taxi_oracles(fare_scale: u32) -> Vec<Reference> {
    let spreads = SpreadScales::of(fare_scale);
    let trips = Reference {
        sql: format!(
            "SELECT extract(epoch FROM bucket)::int8, kind, zone_id, borough, trips, fare,
                    fare_max, round(fare_avg, 6), riders, named, tips, manhattan, near,
                    fare_median, tip_p90, {}, fare_mode, tip_mode, distinct_tips,
                    round(distinct_fare_avg, 6)
               FROM ({}) v",
            spreads.sql(["fare_var", "fare_sd_pop", "tip_var_pop", "tip_sd"]),
            body(SPEC, "trips")
        ),
        ask: ask(
            "trips",
            &TAXI_MEASURES,
            &["bucket", "kind", "zone_id", "borough"],
        ),
        scales: [
            &[
                0, 0, 0, 0, 0, fare_scale, fare_scale, fare_scale, 0, 0, 0, 0, 0, fare_scale, 0,
            ][..],
            &spreads.scales(),
            &[fare_scale, 0, 0, fare_scale],
        ]
        .concat(),
    };
    let logins = Reference {
        sql: format!(
            "SELECT extract(epoch FROM bucket)::int8, zone_id, toll, logins FROM ({}) v",
            body(SPEC, "logins")
        ),
        ask: ask("logins", &["logins"], &["bucket", "zone_id", "toll"]),
        scales: vec![0, 0, 1, 0],
    };
    vec![
        trips,
        logins,
        window(&spreads, fare_scale),
        kept_by_having(fare_scale),
    ]
}

/// Every measure of the taxi spec's trips view.
const TAXI_MEASURES: [&str; 19] = [
    "trips",
    "fare",
    "fare_max",
    "fare_avg",
    "riders",
    "named",
    "tips",
    "manhattan",
    "near",
    "fare_median",
    "tip_p90",
    "fare_var",
    "fare_sd_pop",
    "tip_var_pop",
    "tip_sd",
    "fare_mode",
    "tip_mode",
    "distinct_tips",
    "distinct_fare_avg",
];

/// Trips without the bucket over the window, against the raw rows.
fn window(spreads: &SpreadScales, fare_scale: u32) -> Reference {
    Reference {
        sql: format!(
            "SELECT f.kind, f.zone_id, pickup.borough, count(*), sum(f.fare), max(f.fare),
                    round(avg(f.fare), 6), count(DISTINCT f.rider), count(f.rider), sum(f.tip),
                    count(*) FILTER (WHERE pickup.borough = 'Manhattan'),
                    count(*) FILTER (WHERE f.zone_id IN (1, 2)),
                    percentile_disc(0.5) WITHIN GROUP (ORDER BY f.fare),
                    percentile_disc(0.9) WITHIN GROUP (ORDER BY f.tip), {},
                    mode() WITHIN GROUP (ORDER BY f.fare), mode() WITHIN GROUP (ORDER BY f.tip),
                    sum(DISTINCT f.tip), round(avg(DISTINCT f.fare), 6)
               FROM $S.trips f LEFT JOIN $S.zones pickup ON pickup.id = f.zone_id
              WHERE f.at >= to_timestamp({WINDOW_FROM}) AND f.at < to_timestamp({WINDOW_TO})
              GROUP BY 1, 2, 3",
            spreads.sql([
                "var_samp(f.fare)",
                "stddev_pop(f.fare)",
                "var_pop(f.tip)",
                "stddev_samp(f.tip)"
            ])
        ),
        ask: in_window(ask(
            "trips",
            &TAXI_MEASURES,
            &["kind", "zone_id", "borough"],
        )),
        scales: [
            &[
                0, 0, 0, 0, fare_scale, fare_scale, fare_scale, 0, 0, 0, 0, 0, fare_scale, 0,
            ][..],
            &spreads.scales(),
            &[fare_scale, 0, 0, fare_scale],
        ]
        .concat(),
    }
}

/// The scales of the taxi spec's variance and deviation of the fare and
/// of the tip, an integer: a variance is in its column's units squared,
/// and both keep four places past.
struct SpreadScales {
    fare_variance: u32,
    fare_deviation: u32,
}

impl SpreadScales {
    fn of(fare_scale: u32) -> SpreadScales {
        SpreadScales {
            fare_variance: 2 * fare_scale + 4,
            fare_deviation: fare_scale + 4,
        }
    }

    /// The fare's variance and deviation, then the tip's.
    fn scales(&self) -> [u32; 4] {
        [self.fare_variance, self.fare_deviation, 4, 4]
    }

    /// The four columns `expressions` names, in that order, each rounded
    /// as Straum rounds it.
    fn sql(&self, expressions: [&str; 4]) -> String {
        let mut columns = Vec::with_capacity(4);
        for (expression, places) in expressions.into_iter().zip(self.scales()) {
            columns.push(even(expression, places));
        }
        columns.join(", ")
    }
}

/// `expression` rounded half to even at `places`, as Straum rounds a
/// variance or deviation. PostgreSQL's `round` takes a half away from
/// zero, so a tie, a value whose last digit past `places` is a 5 with
/// only zeros after, is taken to the even neighbour here. The values are
/// never below zero.
fn even(expression: &str, places: u32) -> String {
    format!(
        "CASE WHEN ({expression}) * 10 ^ {places}::numeric % 1 = 0.5
                   AND trunc(({expression}) * 10 ^ {places}::numeric) % 2 = 0
              THEN trunc({expression}, {places})
              ELSE round({expression}, {places}) END"
    )
}

/// The window's groups PostgreSQL's HAVING keeps: fares run from 0 to
/// 80, so the average splits them, and the integer, the decimal and the
/// `<>` each decide some group.
fn kept_by_having(fare_scale: u32) -> Reference {
    Reference {
        sql: format!(
            "SELECT f.kind, f.zone_id, pickup.borough, count(*), round(avg(f.fare), 6),
                    max(f.fare)
               FROM $S.trips f LEFT JOIN $S.zones pickup ON pickup.id = f.zone_id
              WHERE f.at >= to_timestamp({WINDOW_FROM}) AND f.at < to_timestamp({WINDOW_TO})
              GROUP BY 1, 2, 3
             HAVING count(*) > 1 AND avg(f.fare) >= 39.5 AND max(f.fare) <> 79.99"
        ),
        ask: having(
            in_window(ask(
                "trips",
                &["trips", "fare_avg", "fare_max"],
                &["kind", "zone_id", "borough"],
            )),
            vec![
                compared("trips", Comparison::Greater, Literal::Int(1)),
                compared(
                    "fare_avg",
                    Comparison::GreaterOrEqual,
                    Literal::Decimal {
                        units: 395,
                        scale: 1,
                    },
                ),
                compared(
                    "fare_max",
                    Comparison::NotEqual,
                    Literal::Decimal {
                        units: 7999,
                        scale: 2,
                    },
                ),
            ],
        ),
        scales: vec![0, 0, 0, 0, fare_scale, fare_scale],
    }
}

/// The tasks spec's references: by bucket, organization, task group and
/// category, then by organization and category without the bucket over a
/// window that cuts the day bucket `START` begins. A `timestamp` has no
/// zone, so the window's bounds are wall clock in UTC.
pub(super) fn task_oracles() -> Vec<Reference> {
    const MEASURES: [&str; 6] = [
        "tasks",
        "picks",
        "picks_max",
        "pick_tasks",
        "completed_tasks",
        "shelf_tasks",
    ];
    // The ask covers every finite bucket; a row at an infinite time is in
    // none, and the view in PostgreSQL would answer it on its own.
    let by_bucket = Reference {
        sql: format!(
            "SELECT extract(epoch FROM bucket)::int8, organization_id, taskgroup_id, done,
                    category, kind, tasks, picks, picks_max, pick_tasks, completed_tasks,
                    shelf_tasks
               FROM ({}) v
              WHERE bucket >= to_timestamp(0) AT TIME ZONE 'UTC'
                AND bucket < to_timestamp({END}) AT TIME ZONE 'UTC'",
            body(TASKS_SPEC, "tasks")
        ),
        ask: ask(
            "tasks",
            &MEASURES,
            &[
                "bucket",
                "organization_id",
                "taskgroup_id",
                "done",
                "category",
                "kind",
            ],
        ),
        scales: vec![0; 12],
    };
    let window = Reference {
        sql: format!(
            "SELECT f.organization_id, g.category, count(*), sum(f.picks), max(f.picks),
                    count(*) FILTER (WHERE g.category = 'pick'),
                    count(*) FILTER (WHERE f.completed IS NOT NULL),
                    count(*) FILTER (WHERE f.kind = 'shelf')
               FROM $S.tasks f
               LEFT JOIN $S.taskgroups g
                      ON g.organization_id = f.organization_id
                     AND g.taskgroup_id = f.taskgroup_id
              WHERE f.created >= to_timestamp({WINDOW_FROM}) AT TIME ZONE 'UTC'
                AND f.created < to_timestamp({WINDOW_TO}) AT TIME ZONE 'UTC'
              GROUP BY 1, 2"
        ),
        ask: in_window(ask("tasks", &MEASURES, &["organization_id", "category"])),
        scales: vec![0; 8],
    };
    vec![by_bucket, window, open_tasks()]
}

/// The null-ness key as a filter, against the test it stands for.
pub(super) fn open_tasks() -> Reference {
    Reference {
        sql: format!(
            "SELECT f.organization_id, count(*), sum(f.picks)
               FROM $S.tasks f
              WHERE f.completed IS NULL
                AND f.created >= to_timestamp({WINDOW_FROM}) AT TIME ZONE 'UTC'
                AND f.created < to_timestamp({WINDOW_TO}) AT TIME ZONE 'UTC'
              GROUP BY 1"
        ),
        ask: in_window(where_is(
            ask("tasks", &["tasks", "picks"], &["organization_id"]),
            "done",
            Literal::Bool(false),
        )),
        scales: vec![0; 3],
    }
}

/// The picks view's references: by bucket, organization, category and done,
/// then by organization and category over a window.
pub(super) fn picks_oracles() -> Vec<Reference> {
    const MEASURES: [&str; 3] = ["tasks", "picks", "pick_tasks"];
    let by_bucket = Reference {
        sql: format!(
            "SELECT extract(epoch FROM bucket)::int8, organization_id, category, done,
                    tasks, picks, pick_tasks
               FROM ({}) v
              WHERE bucket >= to_timestamp(0) AT TIME ZONE 'UTC'
                AND bucket < to_timestamp({END}) AT TIME ZONE 'UTC'",
            body(PICKS_SPEC, "picks")
        ),
        ask: ask(
            "picks",
            &MEASURES,
            &["bucket", "organization_id", "category", "done"],
        ),
        scales: vec![0; 7],
    };
    let window = Reference {
        sql: format!(
            "SELECT f.organization_id, g.category, count(*), sum(f.picks),
                    count(*) FILTER (WHERE g.category = 'pick')
               FROM $S.tasks f
               LEFT JOIN $S.taskgroups g
                      ON g.organization_id = f.organization_id
                     AND g.taskgroup_id = f.taskgroup_id
              WHERE f.created >= to_timestamp({WINDOW_FROM}) AT TIME ZONE 'UTC'
                AND f.created < to_timestamp({WINDOW_TO}) AT TIME ZONE 'UTC'
              GROUP BY 1, 2"
        ),
        ask: in_window(ask("picks", &MEASURES, &["organization_id", "category"])),
        scales: vec![0; 5],
    };
    vec![by_bucket, window]
}

/// Asks Straum what each reference's SQL answers in PostgreSQL and compares the
/// rows. `None` when Straum isn't ready.
pub(super) fn compare(db: &mut Db, app: &App, references: &[Reference]) -> Option<()> {
    for Reference { sql, ask, scales } in references {
        let answered = match app.ask(ask) {
            Ok(answered) => answered,
            Err(AnswerError::NotReady) => return None,
            Err(error) => panic!("{error}"),
        };
        let mut got_scales: Vec<u32> = Vec::new();
        for column in &answered.answer.columns {
            got_scales.push(column.scale);
        }
        assert_eq!(got_scales, *scales, "{}", ask.view);
        let mut got = printed(&answered.answer, scales);
        let mut expected = db.rows(sql);
        got.sort();
        expected.sort();
        assert_eq!(got, expected, "{}", ask.view);
    }
    Some(())
}

/// As [`compare`], after `change`; Straum must be ready, and the panic
/// names the change if it isn't.
pub(super) fn compare_after(db: &mut Db, app: &App, references: &[Reference], change: &str) {
    assert!(compare(db, app, references).is_some(), "{change}");
}

/// An answer's rows as PostgreSQL prints them, at `scales`.
pub(super) fn printed(answer: &Answer, scales: &[u32]) -> Vec<Vec<Option<String>>> {
    let mut rows = Vec::new();
    for AnswerRow { keys, values } in &answer.rows {
        let (key_scales, value_scales) = scales.split_at(keys.len());
        let mut row = Vec::new();
        for (key, scale) in keys.iter().zip(key_scales) {
            row.push(datum(key, *scale));
        }
        for (value, scale) in values.iter().zip(value_scales) {
            row.push(text(value, *scale));
        }
        rows.push(row);
    }
    rows
}
