//! Rows a day or a week apart against PostgreSQL's own `date_bin` over
//! the raw rows: trips by day, logins by week, tasks by wall-clock day.
//! The rows span several weeks and reach back before 1970, so the epoch
//! grid shows on both sides of it. Each test loads, compares, then
//! follows changes that move rows across day and week edges.

use straum_app::Step;
use straum_domain::{Ask, AskKey, Width};

use super::references::*;
use super::support::*;

const DAY: Width = Width { seconds: 86_400 };
const WEEK: Width = Width { seconds: 604_800 };
/// Hours of rows from `START`: a little over five weeks.
const SPAN_HOURS: u64 = 24 * 37;

/// `ask` with rows `width` apart, the time key first, from `from` to `to`.
fn by_width(ask: Ask, width: Width, from: Option<i64>, to: Option<i64>) -> Ask {
    let mut keys = vec![AskKey::WiderBucket {
        name: id("bucket"),
        width,
    }];
    keys.extend(ask.keys);
    Ask {
        keys,
        from,
        to,
        ..ask
    }
}

/// Trips by day, kind and borough over every time, with the measures
/// whose state merges least simply: distinct, median and a filter.
fn trips_by_day() -> Reference {
    Reference {
        sql: "SELECT extract(epoch FROM date_bin('1 day', f.at, timestamptz 'epoch'))::int8,
                     f.kind, pickup.borough, count(*), sum(f.fare), max(f.fare),
                     count(DISTINCT f.rider),
                     count(*) FILTER (WHERE pickup.borough = 'Manhattan'),
                     percentile_disc(0.5) WITHIN GROUP (ORDER BY f.fare)
                FROM $S.trips f LEFT JOIN $S.zones pickup ON pickup.id = f.zone_id
               WHERE isfinite(f.at)
               GROUP BY 1, 2, 3"
            .to_owned(),
        ask: by_width(
            ask(
                "trips",
                &[
                    "trips",
                    "fare",
                    "fare_max",
                    "riders",
                    "manhattan",
                    "fare_median",
                ],
                &["kind", "borough"],
            ),
            DAY,
            None,
            None,
        ),
        scales: vec![0, 0, 0, 0, 2, 2, 0, 0, 2],
    }
}

/// Logins by week and toll: over every time, then over the two weeks
/// from the week `START` falls in.
fn logins_by_week() -> Vec<Reference> {
    let from = WEEK.bucket_start(START);
    let to = from + 2 * i64::from(WEEK.seconds);
    let every = Reference {
        sql: "SELECT extract(epoch FROM date_bin('7 days', l.at, timestamptz 'epoch'))::int8,
                     pickup.toll, count(*)
                FROM $S.logins l LEFT JOIN $S.zones pickup ON pickup.id = l.zone_id
               WHERE isfinite(l.at)
               GROUP BY 1, 2"
            .to_owned(),
        ask: by_width(ask("logins", &["logins"], &["toll"]), WEEK, None, None),
        scales: vec![0, 1, 0],
    };
    let two_weeks = Reference {
        sql: format!(
            "SELECT extract(epoch FROM date_bin('7 days', l.at, timestamptz 'epoch'))::int8,
                    count(*)
               FROM $S.logins l
              WHERE l.at >= to_timestamp({from}) AND l.at < to_timestamp({to})
              GROUP BY 1"
        ),
        ask: by_width(ask("logins", &["logins"], &[]), WEEK, Some(from), Some(to)),
        scales: vec![0, 0],
    };
    vec![every, two_weeks]
}

/// Tasks by wall-clock day and category: a `timestamp` has no zone, so
/// its day is the UTC day of the wall clock.
fn tasks_by_day() -> Reference {
    Reference {
        sql: "SELECT extract(epoch FROM date_bin('1 day', f.created, timestamp 'epoch'))::int8,
                     g.category, count(*), sum(f.picks)
                FROM $S.tasks f
                LEFT JOIN $S.taskgroups g
                       ON g.organization_id = f.organization_id
                      AND g.taskgroup_id = f.taskgroup_id
               WHERE isfinite(f.created)
               GROUP BY 1, 2"
            .to_owned(),
        ask: by_width(
            ask("tasks", &["tasks", "picks"], &["category"]),
            DAY,
            None,
            None,
        ),
        scales: vec![0, 0, 0, 0],
    }
}

/// Trips and logins across five weeks and before 1970, then inserts,
/// moves across day and week edges, and deletes, each compared.
#[test]
fn rows_a_day_or_a_week_apart_match_postgresql() {
    let pg = postgres();
    let mut db = Db::new(&pg, "wide_rows", TABLES);
    let mut rng = Rng::within_hours(41, SPAN_HOURS);
    for zone in 1..=4 {
        db.run(&format!(
            "INSERT INTO $S.zones VALUES ({zone}, '{}', {})",
            rng.pick(&BOROUGHS),
            rng.toll()
        ));
    }
    for id in 1..=500 {
        db.run(&rng.trip(id));
    }
    // A second before the epoch, on the week edge before it and a second
    // before that edge; and the infinities, which no ask reads.
    db.run(&format!(
        "INSERT INTO $S.trips VALUES
           (900, to_timestamp(-1), 'cab', 1, 5.00, 'ann', 1),
           (901, to_timestamp(-604800), 'cab', 2, 6.00, 'bo', 2),
           (902, to_timestamp(-604801), 'van', 3, 7.00, 'ann', 3),
           (903, 'infinity', 'van', 1, 8.00, 'bo', 4);
         INSERT INTO $S.logins
           SELECT g, to_timestamp({START} - 864000 + g * 7919), 1 + g % 5
             FROM generate_series(1, 400) g;
         INSERT INTO $S.logins VALUES
           (900, to_timestamp(-1), 1),
           (901, to_timestamp(-604800), 2),
           (902, to_timestamp(-604801), 3),
           (903, '-infinity', 1);"
    ));
    let mut references = vec![trips_by_day()];
    references.extend(logins_by_week());

    let mut writer = db.writer(SPEC);
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    compare(&mut db, writer.app(), &references).expect("ready after the load");

    let changes = [
        "INSERT INTO $S.trips VALUES (1000, to_timestamp(-86401), 'cab', 4, 9.00, 'cy', 5)",
        "UPDATE $S.trips SET at = at + interval '3 days 5 hours' WHERE id % 7 = 0",
        "UPDATE $S.logins SET at = at - interval '4 days' WHERE id % 3 = 0",
        "UPDATE $S.trips SET at = to_timestamp(-604800) - interval '1 microsecond' WHERE id = 901",
        "DELETE FROM $S.trips WHERE id % 11 = 0",
        "DELETE FROM $S.logins WHERE id % 5 = 1",
    ];
    for change in changes {
        db.run(change);
        applied(&mut writer);
        compare_after(&mut db, writer.app(), &references, change);
    }
}

/// Tasks over a `timestamp` column by wall-clock day, compared from a
/// session far from UTC, so a day taken in a zone on either side would
/// show.
#[test]
fn wall_clock_days_match_postgresql() {
    let pg = postgres();
    let mut db = Db::new(&pg, "wide_wall_clock", TASKS);
    db.run("SET TIME ZONE 'Europe/Oslo'");
    let mut rng = Rng::within_hours(43, SPAN_HOURS);
    for organization in 1..=3 {
        for taskgroup in 1..=2 {
            db.run(&format!(
                "INSERT INTO $S.taskgroups VALUES ({organization}, {taskgroup}, '{}')",
                rng.pick(&["pick", "pack", "count"])
            ));
        }
    }
    for id in 0..300 {
        db.run(&rng.task(id));
    }
    // Either side of a midnight before 1970 and after, and an infinity.
    db.run(
        "INSERT INTO $S.tasks (task_id, organization_id, taskgroup_id, created, picks) VALUES
           (1000, 1, 1, '1969-12-31 23:59:59', 1),
           (1001, 1, 2, '1970-01-01 00:00:00', 2),
           (1002, 2, 1, '2024-01-20 23:59:59.999999', 3),
           (1003, 2, 2, '2024-01-21 00:00:00', 4),
           (1004, 3, 1, 'infinity', 5)",
    );
    let references = [tasks_by_day()];

    let mut writer = db.writer(TASKS_SPEC);
    assert!(matches!(writer.step(), Ok(Step::Started(_))));
    compare(&mut db, writer.app(), &references).expect("ready after the load");

    let changes = [
        "UPDATE $S.tasks SET created = created + interval '1 microsecond' WHERE task_id = 1002",
        "UPDATE $S.tasks SET created = created - interval '30 hours' WHERE task_id % 4 = 0",
        "UPDATE $S.taskgroups SET category = 'other' WHERE taskgroup_id = 1",
        "DELETE FROM $S.tasks WHERE task_id % 9 = 0",
    ];
    for change in changes {
        db.run(change);
        applied(&mut writer);
        compare_after(&mut db, writer.app(), &references, change);
    }
}
