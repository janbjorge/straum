//! The logical stream end to end: Straum loads the tables, follows the
//! changes pgoutput (PostgreSQL's logical decoding plugin) sends, and
//! after each commit every view equals its SQL. Also how it stops: past
//! the transaction limit, on a value it can't read, on an altered table;
//! a new writer then loads again, as Straum started again does.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, SystemTime};

use straum_app::{AnswerError, App, Source, Writer};
use straum_domain::{AnswerRow, Ask, AskError, ColumnType, Datum, Literal, Value};

use super::references::*;
use super::support::*;
use crate::source::{Postgres, Replication, parse_lsn};

#[test]
fn follows_the_database() {
    let pg = postgres();
    let mut db = Db::new(&pg, "follows", TABLES);
    // Six hours of trips, so a cell holds several rows.
    let mut rng = Rng::within_hours(7, 6);
    let mut trips: Vec<u64> = Vec::new();
    let mut next = 1;
    insert_zones(&mut db, &mut rng);
    for _ in 0..400 {
        db.run(&rng.trip(next));
        trips.push(next);
        next += 1;
    }

    let mut writer = db.writer(SPEC);
    started(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready after the load");

    let mut compared = 0;
    for round in 0..120 {
        // Every transaction inserts a trip, so each is one commit.
        let mut sql = vec!["BEGIN;".to_owned(), rng.trip(next)];
        trips.push(next);
        next += 1;
        // The zones stand still here; `a_dim_change_shows_at_once` and
        // `follows_the_task_tables` move a dim.
        for _ in 0..rng.below(4) {
            let Some(statement) = random_trip_change(&mut rng, &mut trips, next) else {
                continue;
            };
            next += 1;
            sql.push(statement);
        }
        // Every login moves zone at once; truncates empty a table in place.
        match round {
            30 | 70 => sql.push("UPDATE $S.logins SET zone_id = zone_id + 1;".into()),
            50 => sql.push("TRUNCATE $S.logins;".into()),
            90 => {
                sql.push("TRUNCATE $S.trips;".into());
                trips.clear();
                sql.push(rng.trip(next));
                trips.push(next);
                next += 1;
            }
            _ => {}
        }
        sql.push("COMMIT;".into());
        db.run(&sql.join("\n"));
        applied_or_started_again(&db, SPEC, &mut writer);
        if compare(&mut db, writer.app(), &taxi_oracles(2)).is_some() {
            compared += 1;
        }
    }
    assert!(compared > 100, "compared only {compared} times");
}

/// A statement that changes one of `trips` at random, or logs in as
/// `next`; `None` one time in eight.
fn random_trip_change(rng: &mut Rng, trips: &mut Vec<u64>, next: u64) -> Option<String> {
    let pick = trips[rng.index(trips.len())];
    let statement = match rng.below(8) {
        0 | 1 => format!(
            "UPDATE $S.trips SET fare = fare + 1, kind = '{}' WHERE id = {pick};",
            rng.pick(&KINDS)
        ),
        2 => format!("UPDATE $S.trips SET at = at + interval '5 hours' WHERE id = {pick};"),
        3 => {
            trips.retain(|trip| *trip != pick);
            format!("DELETE FROM $S.trips WHERE id = {pick};")
        }
        // Into zones 5 and 6 too, which have no row.
        4 | 5 => format!(
            "UPDATE $S.trips SET zone_id = {} WHERE id = {pick};",
            1 + rng.below(6)
        ),
        6 => format!(
            "INSERT INTO $S.logins VALUES ({next}, to_timestamp({}), {});",
            START + i64::try_from(rng.below(3 * 86_400)).unwrap(),
            1 + rng.below(6)
        ),
        _ => return None,
    };
    Some(statement)
}

/// Naive times and a dim keyed by two columns, one of them a by column.
#[test]
fn follows_the_task_tables() {
    let pg = postgres();
    let mut db = Db::new(&pg, "task_tables", TASKS);
    // The comparison session is far from UTC, so a value that went through
    // a zone on either side would show.
    db.run("SET TIME ZONE 'Europe/Oslo'");
    let mut rng = Rng::new(17);
    insert_task_tables(&mut db, &mut rng, 200);
    // On a bucket edge, a microsecond before it, and at the infinities,
    // which no bucket holds.
    db.run(&format!(
        "INSERT INTO $S.tasks VALUES
           (1000, 1, 1, to_timestamp({START}) AT TIME ZONE 'UTC', NULL, 1),
           (1001, 1, 1, to_timestamp({START}) AT TIME ZONE 'UTC' - interval '1 microsecond', NULL, 1),
           (1002, 1, 1, 'infinity', NULL, 1),
           (1003, 2, 2, '-infinity', NULL, 2)"
    ));
    let spec = db.spec(TASKS_SPEC);
    assert!(crate::plan(&spec, "p", "r", None, Replication::Logical).contains(
        "-- dim taskgroups: Straum holds its key and the columns the views read, and follows it."
    ));
    let mut writer = db.writer(TASKS_SPEC);
    started(&mut writer);
    compare(&mut db, writer.app(), &task_oracles()).expect("ready after the load");
    assert_edge_buckets(writer.app());

    for id in 200..230 {
        db.run(&rng.task(id));
        applied(&mut writer);
    }
    for change in task_changes() {
        db.run(&change);
        applied(&mut writer);
        compare_after(&mut db, writer.app(), &task_oracles(), &change);
    }
    // The key is a boolean, whatever its column is.
    assert!(writer.app().ask(&done_is(Literal::Bool(true))).is_ok());
    assert_eq!(
        refused(writer.app(), &done_is(Literal::Int(1))),
        ["done is a boolean, so it can't be the integer 1"]
    );
}

/// The hour before `START` holds the row a microsecond before it alone.
fn assert_edge_buckets(app: &App) {
    let edges = Ask {
        from: Some(START - 3_600),
        to: Some(START + 3_600),
        ..ask("tasks", &["tasks"], &["bucket"])
    };
    let answered = app.ask(&edges).unwrap();
    assert_eq!(answered.answer.rows.len(), 2);
    assert_eq!(
        answered.answer.rows[0],
        AnswerRow {
            keys: vec![Datum::Int(START - 3_600)],
            values: vec![Value::Int(1)]
        }
    );
    assert_eq!(answered.answer.rows[1].keys, [Datum::Int(START)]);
}

/// The tasks whose `done` key is `value`.
fn done_is(value: Literal) -> Ask {
    where_is(ask("tasks", &["tasks"], &[]), "done", value)
}

/// Each change is one commit, compared after it: facts moving to
/// another dim row or to a group with no row, times moved, rows gone.
fn task_changes() -> [String; 15] {
    [
        // Null-ness flips both ways.
        "UPDATE $S.tasks SET completed = created + interval '1 hour' WHERE task_id % 3 = 0"
            .to_owned(),
        "UPDATE $S.tasks SET completed = NULL WHERE task_id % 6 = 0".to_owned(),
        "UPDATE $S.tasks SET taskgroup_id = 2 WHERE task_id % 5 = 0".to_owned(),
        "UPDATE $S.tasks SET taskgroup_id = 4 WHERE task_id % 17 = 0".to_owned(),
        // The enum key moves, in the fact and through the dim.
        "UPDATE $S.tasks SET kind = 'shelf' WHERE task_id % 4 = 0".to_owned(),
        "UPDATE $S.tasks SET organization_id = 3 WHERE task_id % 13 = 0".to_owned(),
        "UPDATE $S.tasks SET created = created + interval '90 minutes' WHERE task_id % 7 = 0"
            .to_owned(),
        // An infinite time becomes a real one and lands in its bucket; a
        // real one becomes infinite and leaves its bucket; one goes.
        format!(
            "UPDATE $S.tasks SET created = to_timestamp({START}) AT TIME ZONE 'UTC' \
             + interval '2 hours' WHERE task_id = 1002"
        ),
        "UPDATE $S.tasks SET created = 'infinity' WHERE task_id = 1000".to_owned(),
        "DELETE FROM $S.tasks WHERE task_id = 1003".to_owned(),
        "DELETE FROM $S.tasks WHERE task_id % 11 = 0".to_owned(),
        // The dim moves: a group recategorised, one moved to a new key,
        // one gone, one back.
        "UPDATE $S.taskgroups SET category = 'other' WHERE organization_id = 1".to_owned(),
        "UPDATE $S.taskgroups SET taskgroup_id = 3 WHERE organization_id = 2 AND taskgroup_id = 1"
            .to_owned(),
        "DELETE FROM $S.taskgroups WHERE organization_id = 3 AND taskgroup_id = 2".to_owned(),
        "INSERT INTO $S.taskgroups VALUES (3, 2, 'count')".to_owned(),
    ]
}

/// `--max-transaction-rows` for the capped writer.
const CAP: usize = 50;

/// A transaction at the limit applies; one past it stops the writer, and
/// the next one's load holds its rows.
#[test]
fn a_transaction_past_the_limit_stops_the_writer() {
    let pg = postgres();
    let mut db = Db::new(&pg, "capped", TABLES);
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5), (2, 'Bronx', NULL)");
    let mut rng = Rng::new(23);
    for id in 0..20 {
        db.run(&rng.trip(id));
    }
    let mut writer = capped_writer(&db);
    started(&mut writer);
    db.run(&batch(&mut rng, 100, CAP));
    applied(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");

    db.run(&batch(&mut rng, 200, CAP + 1));
    let error = writer.step().unwrap_err().to_string();
    let lsn = db.rows("SELECT pg_current_wal_lsn()")[0][0]
        .clone()
        .unwrap();
    assert_eq!(
        error,
        format!(
            "transaction {} changes more than {CAP} rows Straum reads, the --max-transaction-rows \
             limit",
            error.split(' ').nth(1).unwrap_or_default()
        ),
        "{error}"
    );
    // The transaction's commit record ends at or before the server's
    // position now.
    let named = error.split(' ').nth(1).unwrap();
    assert!(
        parse_lsn(named).unwrap() <= parse_lsn(&lsn).unwrap(),
        "transaction {named} is past the server at {lsn}"
    );
    assert!(!writer.app().status().ready);
    let mut writer = capped_writer(&db);
    started(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready after the new load");
    db.run(&rng.trip(300));
    applied(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");
}

/// A writer that refuses a transaction of more than `CAP` rows.
fn capped_writer(db: &Db) -> Writer<Postgres> {
    let source = Postgres::new(
        db.dsn.clone(),
        &db.name,
        CAP,
        MAX_RETAINED_WAL_BYTES,
        Replication::Logical,
    );
    Writer::new(App::new(db.spec(SPEC), LIMITS), source)
}

/// One transaction inserting `rows` trips from id `from`.
fn batch(rng: &mut Rng, from: u64, rows: usize) -> String {
    let to = from + u64::try_from(rows).unwrap();
    let mut inserts: Vec<String> = Vec::new();
    for id in from..to {
        inserts.push(rng.trip(id));
    }
    format!("BEGIN;\n{}\nCOMMIT;", inserts.join("\n"))
}

/// The same tables under two views: a dim change is a change to the dim's
/// projection, and every ask reads the dim as it is now, as PostgreSQL's
/// view does. Nothing loads again.
#[test]
fn a_dim_change_shows_at_once() {
    let pg = postgres();
    let mut db = Db::new(&pg, "ingest", TASKS);
    db.run("SET TIME ZONE 'Europe/Oslo'");
    let mut rng = Rng::new(29);
    insert_task_tables(&mut db, &mut rng, 200);
    let mut picks = db.writer(PICKS_SPEC);
    let mut tasks = db.writer(TASKS_SPEC);
    started(&mut picks);
    started(&mut tasks);
    compare(&mut db, picks.app(), &picks_oracles()).expect("picks ready");
    compare(&mut db, tasks.app(), &task_oracles()).expect("tasks ready");

    for change in dim_changes(&mut rng) {
        db.run(&change);
        applied(&mut picks);
        applied(&mut tasks);
        compare_after(&mut db, picks.app(), &picks_oracles(), &change);
        compare_after(&mut db, tasks.app(), &task_oracles(), &change);
    }
    tasks_of_a_gone_group_read_null(picks.app());
}

/// Fact rows come, move between groups, flip and go; tasks in groups 3
/// and 4 have no dim row and read null. Then the dim moves: a group
/// recategorised with a new task in the same transaction, a key moved and
/// a row gone each with a task pointing at them, the table emptied and
/// filled again.
fn dim_changes(rng: &mut Rng) -> Vec<String> {
    let mut changes: Vec<String> = Vec::new();
    for id in 200..210 {
        changes.push(rng.task(id));
    }
    changes.extend([
        "UPDATE $S.tasks SET taskgroup_id = 2 WHERE task_id % 5 = 0".to_owned(),
        "UPDATE $S.tasks SET completed = NULL WHERE task_id % 4 = 0".to_owned(),
        "DELETE FROM $S.tasks WHERE task_id % 11 = 0".to_owned(),
        format!(
            "BEGIN;
             UPDATE $S.taskgroups SET category = 'other' WHERE organization_id = 1;
             INSERT INTO $S.tasks VALUES (300, 1, 1, to_timestamp({START}) AT TIME ZONE 'UTC', NULL, 5);
             COMMIT;"
        ),
        "UPDATE $S.tasks SET picks = coalesce(picks, 0) + 1
          WHERE task_id = (SELECT min(task_id) FROM $S.tasks
                            WHERE organization_id = 1 AND taskgroup_id = 1 AND task_id < 300)"
            .to_owned(),
        format!(
            "BEGIN;
             UPDATE $S.taskgroups SET taskgroup_id = 3 WHERE organization_id = 2 AND taskgroup_id = 1;
             INSERT INTO $S.tasks VALUES (401, 2, 1, to_timestamp({START}) AT TIME ZONE 'UTC', NULL, 1);
             COMMIT;"
        ),
        format!(
            "BEGIN;
             DELETE FROM $S.taskgroups WHERE organization_id = 2 AND taskgroup_id = 3;
             INSERT INTO $S.tasks VALUES (402, 2, 1, to_timestamp({START}) AT TIME ZONE 'UTC', NULL, 1);
             COMMIT;"
        ),
        "TRUNCATE $S.taskgroups".to_owned(),
        "INSERT INTO $S.taskgroups VALUES (1, 1, 'pick'), (2, 2, 'pack'), (3, 1, 'count')"
            .to_owned(),
    ]);
    changes
}

/// Tasks whose group is gone read null, as the join gives.
fn tasks_of_a_gone_group_read_null(app: &App) {
    let organization_2 = where_is(
        ask("picks", &["tasks"], &["category"]),
        "organization_id",
        Literal::Int(2),
    );
    let null_tasks = Ask {
        from: Some(START),
        to: Some(START + 3_600),
        ..organization_2
    };
    let answered = app.ask(&null_tasks).unwrap();
    let mut nulls = None;
    for row in &answered.answer.rows {
        if row.keys[0] == Datum::Null {
            nulls = Some(row.values[0].clone());
            break;
        }
    }
    assert!(
        matches!(nulls, Some(Value::Int(count)) if count >= 2),
        "{nulls:?}"
    );
}

/// A value Straum can't read stops the load, or the stream, naming the
/// table and column it was in, so the row can be found and fixed.
#[test]
fn a_poison_value_names_its_column() {
    let pg = postgres();
    let mut db = Db::new(&pg, "poison", TABLES);
    db.run(&format!(
        "INSERT INTO $S.trips VALUES (1, to_timestamp({START}), 'cab', 1, 'NaN', 'ann', 2)"
    ));
    let mut writer = db.writer(SPEC);
    let error = writer.step().unwrap_err().to_string();
    assert!(
        error.starts_with("can't read \"\\\\x")
            && error.ends_with("as a numeric of scale 2 in column fare of poison.trips"),
        "{error}"
    );
    assert!(!writer.app().status().ready);
    db.run("UPDATE $S.trips SET fare = 1.50 WHERE id = 1");
    writer = db.writer(SPEC);
    started(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");

    // Streamed, the value comes as text.
    db.run("UPDATE $S.trips SET fare = 'NaN' WHERE id = 1");
    let error = writer.step().unwrap_err().to_string();
    assert_eq!(
        error,
        "can't read \"NaN\" as a numeric of scale 2 in column fare of poison.trips"
    );
    db.run("UPDATE $S.trips SET fare = 2.50 WHERE id = 1");
    writer = db.writer(SPEC);
    started(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready after the fix");
}

#[test]
fn the_snapshot_and_the_stream_split_exactly() {
    let pg = postgres();
    let mut db = Db::new(&pg, "split", TABLES);
    for round in 0..3 {
        db.run("TRUNCATE $S.trips");
        let written = Arc::new(AtomicUsize::new(0));
        let done = Arc::new(AtomicBool::new(false));
        let inserter = insert_trips_in_turn(db.another_connection(), &written, &done);
        while written.load(Ordering::Acquire) < 50 {
            thread::sleep(Duration::from_millis(1));
        }
        let mut writer = db.writer(SPEC);
        started(&mut writer);
        assert!(
            !done.load(Ordering::Acquire),
            "round {round}: the writer finished before the load"
        );
        let loaded = writer.app().ask(&ask("trips", &["trips"], &[])).unwrap();
        assert_ne!(
            loaded.answer.rows[0].values[0],
            Value::Int(0),
            "round {round}"
        );
        inserter.join().unwrap();
        db.run("INSERT INTO $S.trips VALUES (-1, now(), 'marker', 1, 1.00, 'ann', 2)");
        let marker = ask("trips", &["trips"], &["kind"]);
        let mut streamed = 0;
        loop {
            applied(&mut writer);
            streamed += 1;
            let answered = writer.app().ask(&marker).unwrap();
            if holds_the_marker(&answered.answer.rows) {
                break;
            }
        }
        assert!(streamed > 1, "round {round}: nothing was streamed");
        // The marker's commit time, by the server's clock, is now.
        let time = writer.app().status().committed.unwrap();
        let apart = match SystemTime::now().duration_since(time) {
            Ok(behind) => behind,
            Err(ahead) => ahead.duration(),
        };
        assert!(apart < Duration::from_secs(60), "round {round}: {apart:?}");
        compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");
    }
}

/// A thread inserting 2000 trips into `db`'s schema, one commit each,
/// counting them in `written` and setting `done` at the end.
fn insert_trips_in_turn(
    mut db: Db,
    written: &Arc<AtomicUsize>,
    done: &Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    let (written, done) = (Arc::clone(written), Arc::clone(done));
    thread::spawn(move || {
        for id in 0..2_000 {
            db.run(&format!(
                "INSERT INTO $S.trips VALUES ({id}, to_timestamp({}), 'cab', 1, 1.00, 'ann', 2)",
                START + id
            ));
            written.fetch_add(1, Ordering::Release);
        }
        done.store(true, Ordering::Release);
    })
}

/// Whether one of `rows` is of kind `marker`.
fn holds_the_marker(rows: &[AnswerRow]) -> bool {
    for row in rows {
        if row.keys[0] == Datum::Text("marker".into()) {
            return true;
        }
    }
    false
}

/// What the writer says when a change to `table` no longer fits it.
fn no_longer_fits(table: &str) -> String {
    format!(
        "a change no longer fits {table}: \
         the stream no longer carries what Straum reads of it: \
         the table was altered or rewritten, or a change came without its old row"
    )
}

#[test]
fn an_unfit_change_stops_the_writer() {
    let pg = postgres();
    let mut db = Db::new(&pg, "unfit", TABLES);
    db.run(&format!(
        "INSERT INTO $S.logins VALUES (1, to_timestamp({START}), 1)"
    ));
    let mut writer = db.writer(SPEC);
    writer.step().unwrap();
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");

    // A fact that stops sending old rows stops the writer, and the setup
    // of the next one says what to fix.
    db.run("ALTER TABLE $S.logins REPLICA IDENTITY DEFAULT");
    db.run("UPDATE $S.logins SET zone_id = 2");
    let error = writer.step().unwrap_err().to_string();
    assert_eq!(error, no_longer_fits("logins"));
    assert_eq!(
        writer.app().ask(&ask("trips", &["trips"], &[])),
        Err(AnswerError::NotReady)
    );
    writer = db.writer(SPEC);
    let error = writer.step().unwrap_err().to_string();
    assert!(
        error.contains("fact logins: Straum needs the whole old row of every update and delete"),
        "{error}"
    );
    db.run("ALTER TABLE $S.logins REPLICA IDENTITY FULL");
    writer = db.writer(SPEC);
    started(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready after the new load");
    db.run(&format!(
        "INSERT INTO $S.logins VALUES (2, to_timestamp({START}), 3)"
    ));
    applied(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");

    // A dim is held too, so it owes the same.
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5)");
    applied(&mut writer);
    db.run("ALTER TABLE $S.zones REPLICA IDENTITY DEFAULT");
    db.run("UPDATE $S.zones SET borough = 'Staten Island' WHERE id = 1");
    let error = writer.step().unwrap_err().to_string();
    assert_eq!(error, no_longer_fits("zones"));
    writer = db.writer(SPEC);
    let error = writer.step().unwrap_err().to_string();
    assert!(
        error.contains("dim zones: Straum needs the whole old row of every update and delete"),
        "{error}"
    );
    db.run("ALTER TABLE $S.zones REPLICA IDENTITY FULL");
    writer = db.writer(SPEC);
    started(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready after the dim's new load");
}

#[test]
fn asks_take_values_of_the_types_the_load_read() {
    let pg = postgres();
    let db = Db::new(&pg, "types", TABLES);
    let mut writer = db.writer(SPEC);
    started(&mut writer);
    // text, the column a ref joins on (int), a dim's text; then
    // numeric(4, 1).
    let cases = [
        TypeCase {
            key: "kind",
            right: text_literal("cab"),
            wrong: Literal::Int(1),
            column: ColumnType::Text,
        },
        TypeCase {
            key: "zone_id",
            right: Literal::Int(1),
            wrong: text_literal("1"),
            column: ColumnType::Int,
        },
        TypeCase {
            key: "borough",
            right: text_literal("Bronx"),
            wrong: Literal::Bool(true),
            column: ColumnType::Text,
        },
    ];
    for case in cases {
        takes_only_its_type(writer.app(), case);
    }
    // A numeric of scale 1 takes no integer: 1 would read as 0.1.
    let toll = filtered("logins", "logins", "toll", Literal::Int(1));
    assert_eq!(
        refused(writer.app(), &toll),
        ["toll is a numeric of scale 1, so it can't be the integer 1"]
    );
}

/// A key of the trips view, a value of its column's type, one of another
/// type, and the column's type.
struct TypeCase {
    // `'static`: every key is a literal written here.
    key: &'static str,
    right: Literal,
    wrong: Literal,
    column: ColumnType,
}

/// The trips where `case.key` is a value of its type answer; where it is
/// one of another type, the ask is refused naming the column's type.
fn takes_only_its_type(app: &App, case: TypeCase) {
    let TypeCase {
        key,
        right,
        wrong,
        column,
    } = case;
    let answered = app.ask(&filtered("trips", "trips", key, right));
    assert!(answered.is_ok(), "{key}: {answered:?}");
    let refused = app.ask(&filtered("trips", "trips", key, wrong.clone()));
    let Err(AnswerError::Invalid(errors)) = refused else {
        panic!("{key}: {refused:?}");
    };
    assert_eq!(errors.len(), 1, "{key}");
    assert!(
        matches!(&errors[0], AskError::WrongType { column: got, value, .. } if *got == column && *value == wrong),
        "{key}: {errors:?}"
    );
}

/// Why `app` refuses `asked`, each error as a client reads it.
fn refused(app: &App, asked: &Ask) -> Vec<String> {
    let Err(AnswerError::Invalid(errors)) = app.ask(asked) else {
        panic!("not refused as invalid: {asked:?}");
    };
    let mut lines = Vec::with_capacity(errors.len());
    for error in &errors {
        lines.push(straum_notation::describe_ask_error(error));
    }
    lines
}

/// `fact`'s `measure` where `key` is `value`.
fn filtered(fact: &str, measure: &str, key: &str, value: Literal) -> Ask {
    where_is(ask(fact, &[measure], &[]), key, value)
}

/// `text` as a text literal.
fn text_literal(text: &str) -> Literal {
    Literal::Text(text.into())
}

#[test]
fn follows_at_replica_from_19() {
    let pg = postgres_at_replica(NINETEEN);
    let mut db = Db::new(&pg, "replica", TABLES);
    let mut rng = Rng::new(13);
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5), (2, 'Bronx', NULL)");
    for id in 0..20 {
        db.run(&rng.trip(id));
    }
    let mut writer = db.writer(SPEC);
    started(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready after the load");
    for id in 20..30 {
        db.run(&rng.trip(id));
        applied(&mut writer);
    }
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");
}

#[test]
fn a_changed_column_type_stops_the_writer() {
    let pg = postgres();
    let mut db = Db::new(&pg, "altered", TABLES);
    db.run(&Rng::new(1).trip(1));
    let mut writer = db.writer(SPEC);
    writer.step().unwrap();
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");

    db.run("ALTER TABLE $S.trips ALTER COLUMN fare TYPE numeric(12, 3)");
    db.run(&format!(
        "INSERT INTO $S.trips VALUES (2, to_timestamp({START}), 'cab', 1, 2.125, 'bo', 3)"
    ));
    let error = writer.step().unwrap_err().to_string();
    assert!(
        error == no_longer_fits("trips") || trips_altered(&db, &error),
        "{error}"
    );
    writer = db.writer(SPEC);
    writer.step().unwrap();
    db.run(&Rng::new(2).trip(3));
    applied(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(3)).expect("ready");

    // A column Straum reads is gone: Straum stops and the setup check says why.
    db.run("ALTER TABLE $S.trips DROP COLUMN rider");
    db.run(&format!(
        "INSERT INTO $S.trips (id, at, kind, zone_id, fare, tip)
         VALUES (4, to_timestamp({START}), 'cab', 1, 1.000, 1)"
    ));
    let error = writer.step().unwrap_err().to_string();
    assert!(
        error.contains("column rider of") || trips_altered(&db, &error),
        "{error}"
    );
    writer = db.writer(SPEC);
    let error = writer.step().unwrap_err().to_string();
    assert!(error.contains("has no column rider"), "{error}");
}

/// Whether `error` is the WAL reading's word that trips was altered. The
/// writer reads the WAL before it asks the stream when a reading is due,
/// so either may say first why Straum stops.
fn trips_altered(db: &Db, error: &str) -> bool {
    error == db.sql("trips: a column Straum reads of $S.trips was altered")
}

/// Too big and too random to stay inline: PostgreSQL TOASTs the rider, and
/// an update that leaves it alone doesn't send it. The old row, whole
/// under `REPLICA IDENTITY FULL`, supplies it.
#[test]
fn a_value_left_out_comes_from_the_old_row() {
    let pg = postgres();
    let mut db = Db::new(&pg, "toast", TABLES);
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5)");
    db.run(&format!(
        "INSERT INTO $S.trips
         SELECT 1, to_timestamp({START}), 'cab', 1, 1.00,
                string_agg(md5(random()::text), ''), 2
           FROM generate_series(1, 2000)"
    ));
    let mut writer = db.writer(SPEC);
    writer.step().unwrap();
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");

    db.run("UPDATE $S.trips SET tip = 3 WHERE id = 1");
    applied(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready after the update");
    db.run("DELETE FROM $S.trips WHERE id = 1");
    applied(&mut writer);
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready after the delete");
}

#[test]
fn the_stream_outlasts_a_busy_writer() {
    let pg = postgres();
    let mut db = Db::new(&pg, "busy", TABLES);
    let mut writer = db.writer(SPEC);
    writer.step().unwrap();
    let mut rng = Rng::new(5);
    db.run(&rng.trip(0));
    applied(&mut writer);
    // More commits than the queue holds, then a pause past
    // wal_sender_timeout (4 s): the thread keeps answering.
    for id in 1..101 {
        db.run(&rng.trip(id));
    }
    thread::sleep(Duration::from_secs(9));
    for _ in 0..100 {
        applied(&mut writer);
    }
    // Commits made after the pause still arrive: the stream survived.
    for id in 101..111 {
        db.run(&rng.trip(id));
        applied(&mut writer);
    }
    compare(&mut db, writer.app(), &taxi_oracles(2)).expect("ready");
}

/// A row once a walsender of Straum's slot streams: it shows `startup`
/// until `START_REPLICATION`.
const STREAMING: &str = "SELECT 1 FROM pg_replication_slots s
    JOIN pg_stat_replication r ON r.pid = s.active_pid
    WHERE s.slot_name LIKE 'straum%' AND r.state IN ('catchup', 'streaming')";

/// The stream reads from the start on, before any table loads, so the
/// slot is read while they load.
#[test]
fn streams_before_the_tables_load() {
    let pg = postgres();
    let mut db = Db::new(&pg, "beside", TABLES);
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    source.open(&spec).unwrap();
    wait_for(&mut db.connection, STREAMING);
    load_all(&mut source, &spec).unwrap();
}

/// A start the setup check refuses leaves no stream and no slot.
#[test]
fn a_refused_start_streams_nothing() {
    let pg = postgres();
    let mut db = Db::new(&pg, "refusedstart", TABLES);
    db.run("ALTER PUBLICATION $S DROP TABLE $S.logins");
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    let error = source.open(&spec).unwrap_err().to_string();
    assert!(error.contains("logins"), "{error}");
    wait_for(
        &mut db.connection,
        "SELECT 1 WHERE NOT EXISTS (SELECT FROM pg_stat_replication)
           AND NOT EXISTS (SELECT FROM pg_replication_slots)",
    );
}

/// A logical slot is made once every writing transaction open when it
/// began has ended. A start waits for that at most its wait, then names
/// the transactions, and starts once they end.
#[test]
fn a_start_says_which_transactions_hold_it_back() {
    let pg = postgres();
    let db = Db::new(&pg, "heldback", TABLES);
    let spec = db.spec(SPEC);
    let mut holder = db.another_connection();
    holder.run("CREATE TABLE $S.other (id int)");
    holder.run("BEGIN; INSERT INTO $S.other VALUES (1)");
    let xid = holder.rows("SELECT pg_current_xact_id()::text")[0][0]
        .clone()
        .unwrap();
    let mut source = db.pg();
    source.settings.consistent_wait = Duration::from_secs(2);
    let error = source.open(&spec).unwrap_err().to_string();
    assert_eq!(
        error,
        format!(
            "the start waited 2 s for transactions open on the server to end before its slot \
             could be made: xid {xid}; a long or prepared one (pg_prepared_xacts) holds the \
             start back"
        )
    );
    holder.run("COMMIT");
    assert!(source.open(&spec).is_ok());
}
