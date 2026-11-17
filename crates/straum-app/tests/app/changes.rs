//! Commits after the start: fact and dim rows that change in place, and
//! the changes that no longer fit, after which Straum starts over.

use straum_app::{AnswerError, Change, Error, Step};
use straum_domain::{AnswerRow, Datum, Value};

use crate::common::{
    DAY, Harness, ask, by_borough, count, insert, int, login, reference_answer, row, setup, text,
    trip, zone,
};
use crate::fake::{Row, Write, id};

#[test]
fn a_dropped_stream_starts_over() {
    let Harness {
        database,
        mut writer,
    } = setup();
    writer.step().unwrap();
    let first = database.borrow_mut().commit(vec![trip(1, 1, 1, 10)]);
    assert_eq!(writer.step().unwrap(), Step::Applied(first));
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(1)));

    let second = database.borrow_mut().commit(vec![trip(2, 1, 1, 10)]);
    database.borrow_mut().drop_connection = true;
    assert!(writer.step().is_err());
    assert!(!writer.app().status().ready);
    assert_eq!(count(writer.app(), "trips"), Err(AnswerError::NotReady));

    // The new stream starts after the second commit, and the new load counts
    // the first trip once.
    assert_eq!(writer.step().unwrap(), Step::Started(second));
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(2)));
}

#[test]
fn fact_rows_change_through_the_old_row() {
    let Harness {
        database,
        mut writer,
    } = setup();
    database
        .borrow_mut()
        .commit(vec![trip(1, 1, 1, 10), trip(2, 1, 1, 20)]);
    writer.step().unwrap();
    database.borrow_mut().commit(vec![
        Write::Update {
            table: "trips",
            key: int(1),
            new_key: int(1),
            row: vec![int(0), int(2), int(1), int(30), text("bo")],
        },
        Write::Delete {
            table: "trips",
            key: int(2),
        },
    ]);
    writer.step().unwrap();

    assert_eq!(database.borrow().loads.len(), 3, "no reload");
    let by_kind = ask("trips", &["trips", "fare", "riders"], &["kind"]);
    let answered = writer.app().ask(&by_kind).unwrap();
    assert_eq!(
        answered.answer.rows,
        [AnswerRow {
            keys: vec![int(2)],
            values: vec![Value::Int(1), Value::Int(30), Value::Int(1)]
        }]
    );
    assert_eq!(
        answered.answer,
        reference_answer(&database.borrow(), &by_kind)
    );
}

#[test]
fn fact_change_without_the_old_row_starts_over() {
    let Harness {
        database,
        mut writer,
    } = setup();
    database.borrow_mut().commit(vec![login(1, 1), login(2, 1)]);
    writer.step().unwrap();

    database.borrow_mut().commit(vec![Write::Delete {
        table: "logins",
        key: int(1),
    }]);
    assert_eq!(
        writer.step().unwrap_err().to_string(),
        "a change no longer fits logins: the stream no longer carries what Straum reads of it: \
         the table was altered or rewritten, or a change came without its old row"
    );
    assert!(!writer.app().status().ready);
    let lsn = database.borrow().lsn();
    assert_eq!(writer.step().unwrap(), Step::Started(lsn));
    assert_eq!(database.borrow().loads[3..], ["zones", "trips", "logins"]);
    assert_eq!(count(writer.app(), "logins"), Ok(Value::Int(1)));

    database.borrow_mut().commit(vec![Write::Update {
        table: "logins",
        key: int(2),
        new_key: int(2),
        row: vec![int(DAY), int(2)],
    }]);
    assert!(writer.step().is_err());
    writer.step().unwrap();
    let by_bucket = ask("logins", &["logins"], &["bucket", "borough"]);
    assert_eq!(
        writer.app().ask(&by_bucket).unwrap().answer,
        reference_answer(&database.borrow(), &by_bucket)
    );
}

/// A cell is keyed by the join column, so a dim change is one row in the
/// dim's projection and the next ask reads the dim as it is: no cell
/// moves, nothing starts over.
#[test]
fn dim_changes_show_at_once() {
    let Harness {
        database,
        mut writer,
    } = setup();
    database
        .borrow_mut()
        .commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10), trip(2, 1, 1, 20)]);
    writer.step().unwrap();
    assert_eq!(by_borough(writer.app(), "trips"), [row(text("Bronx"), 2)]);

    let lsn = database.borrow_mut().commit(vec![Write::Update {
        table: "zones",
        key: int(1),
        new_key: int(1),
        row: vec![int(1), text("Queens")],
    }]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    assert_eq!(by_borough(writer.app(), "trips"), [row(text("Queens"), 2)]);
    assert_eq!(
        writer
            .app()
            .ask(&ask("trips", &["trips"], &["borough"]))
            .unwrap()
            .answer,
        reference_answer(&database.borrow(), &ask("trips", &["trips"], &["borough"]))
    );

    // A new row and a changed one join the dim as it is.
    database.borrow_mut().commit(vec![trip(3, 1, 1, 30)]);
    writer.step().unwrap();
    assert_eq!(by_borough(writer.app(), "trips"), [row(text("Queens"), 3)]);
    let lsn = database.borrow_mut().commit(vec![Write::Update {
        table: "trips",
        key: int(1),
        new_key: int(1),
        row: vec![int(0), int(1), int(1), int(11), text("ann")],
    }]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    assert_eq!(by_borough(writer.app(), "trips"), [row(text("Queens"), 3)]);
    assert_eq!(database.borrow().loads.len(), 3, "no reload");

    // A dim key moved: the rows that pointed at the old key read null.
    let lsn = database.borrow_mut().commit(vec![Write::Update {
        table: "zones",
        key: int(1),
        new_key: int(9),
        row: vec![int(9), text("Queens")],
    }]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    assert_eq!(by_borough(writer.app(), "trips"), [row(Datum::Null, 3)]);
}

/// A second dim row under a key Straum holds is a change like any other:
/// the join finds both, so the trips count once under each borough, as
/// PostgreSQL's join would repeat them.
#[test]
fn a_dim_key_held_twice_joins_twice() {
    let Harness {
        database,
        mut writer,
    } = setup();
    database
        .borrow_mut()
        .commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10), trip(2, 1, 1, 20)]);
    writer.step().unwrap();
    assert_eq!(by_borough(writer.app(), "trips"), [row(text("Bronx"), 2)]);

    let lsn = database.borrow_mut().send(vec![Change::Insert {
        table: id("zones"),
        row: vec![int(1), text("Staten Island")],
    }]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    assert_eq!(
        by_borough(writer.app(), "trips"),
        [row(text("Bronx"), 2), row(text("Staten Island"), 2)]
    );
    let lsn = database.borrow_mut().send(vec![Change::Delete {
        table: id("zones"),
        old: vec![int(1), text("Bronx")],
    }]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    assert_eq!(
        by_borough(writer.app(), "trips"),
        [row(text("Staten Island"), 2)]
    );
    assert_eq!(database.borrow().loads.len(), 3, "no reload");
}

/// A row whose dim row does not exist reads null, as the view's `LEFT
/// OUTER JOIN` gives, until the dim row comes; a dim row deleted or
/// truncated leaves null for every row that pointed at it.
#[test]
fn a_missing_dim_row_reads_null_until_it_comes() {
    let Harness {
        database,
        mut writer,
    } = setup();
    database.borrow_mut().commit(vec![trip(1, 1, 2, 10)]);
    writer.step().unwrap();
    assert_eq!(by_borough(writer.app(), "trips"), [row(Datum::Null, 1)]);

    database
        .borrow_mut()
        .commit(vec![zone(2, "Queens"), trip(2, 1, 2, 20)]);
    writer.step().unwrap();
    assert_eq!(by_borough(writer.app(), "trips"), [row(text("Queens"), 2)]);

    database.borrow_mut().commit(vec![
        Write::Delete {
            table: "zones",
            key: int(2),
        },
        trip(3, 1, 2, 30),
    ]);
    writer.step().unwrap();
    assert_eq!(by_borough(writer.app(), "trips"), [row(Datum::Null, 3)]);
    assert_eq!(database.borrow().loads.len(), 3, "no reload");
}

#[test]
fn truncate_empties_the_table() {
    let Harness {
        database,
        mut writer,
    } = setup();
    database
        .borrow_mut()
        .commit(vec![zone(1, "Bronx"), trip(1, 1, 1, 10), trip(2, 1, 1, 10)]);
    writer.step().unwrap();
    database.borrow_mut().commit(vec![
        Write::Truncate { table: "trips" },
        Write::Truncate { table: "zones" },
        trip(3, 1, 1, 10),
    ]);
    writer.step().unwrap();

    assert_eq!(database.borrow().loads.len(), 3, "no reload");
    let by_borough = ask("trips", &["trips"], &["borough"]);
    let answer = writer.app().ask(&by_borough).unwrap().answer;
    assert_eq!(answer.rows, [row(Datum::Null, 1)]);
    assert_eq!(answer, reference_answer(&database.borrow(), &by_borough));
}

#[test]
fn altered_table_starts_over() {
    let Harness {
        database,
        mut writer,
    } = setup();
    database.borrow_mut().commit(vec![trip(1, 1, 1, 10)]);
    writer.step().unwrap();
    database.borrow_mut().commit(vec![
        Write::Alter { table: "trips" },
        trip(2, 1, 1, 10),
        Write::Alter { table: "trips" },
    ]);
    assert!(matches!(writer.step(), Err(Error::Unfit { table, .. }) if table == id("trips")));
    writer.step().unwrap();
    assert_eq!(database.borrow().loads.len(), 6);
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(2)));

    // A dim altered starts over too: Straum holds its rows.
    database
        .borrow_mut()
        .commit(vec![Write::Alter { table: "zones" }]);
    assert!(matches!(writer.step(), Err(Error::Unfit { table, .. }) if table == id("zones")));
    writer.step().unwrap();
    assert_eq!(database.borrow().loads.len(), 9);
}

/// The insert before the unfit change is in the store, and not in any
/// answer: the stream is cleared before the write lock drops. The next
/// start loads the commit whole, from the snapshot.
#[test]
fn a_commit_that_stops_fitting_is_not_answered() {
    let Harness {
        database,
        mut writer,
    } = setup();
    database.borrow_mut().commit(vec![trip(1, 1, 1, 10)]);
    writer.step().unwrap();
    database
        .borrow_mut()
        .commit(vec![trip(2, 1, 1, 20), Write::Alter { table: "trips" }]);
    let error = writer.step().unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("a change no longer fits trips: the stream no longer carries"),
        "{error}"
    );
    assert_eq!(count(writer.app(), "trips"), Err(AnswerError::NotReady));
    assert_eq!(writer.app().status().applied, None);
    writer.step().unwrap();
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(2)));
    assert!(writer.app().status().ready);
}

#[test]
fn tables_outside_the_spec_are_skipped() {
    let Harness {
        database,
        mut writer,
    } = setup();
    writer.step().unwrap();
    let lsn = database
        .borrow_mut()
        .commit(vec![insert("audit", 1, vec![text("x")])]);
    assert_eq!(writer.step().unwrap(), Step::Applied(lsn));
    assert_eq!(database.borrow().loads.len(), 3, "no reload");
    assert!(writer.app().status().ready);
}

#[test]
fn a_row_that_does_not_fit_stops_straum() {
    let Harness {
        database,
        mut writer,
    } = setup();
    writer.step().unwrap();
    let mut row = vec![int(0), int(1), int(1), text("ten"), text("ann")];
    database
        .borrow_mut()
        .commit(vec![insert("trips", 1, row.clone())]);

    // The insert fails, and so does loading trips again.
    let error = writer.step().unwrap_err();
    assert_eq!(
        error.to_string(),
        "a change no longer fits trips: column fare is not an integer"
    );
    let error = writer.step().unwrap_err();
    assert_eq!(error.to_string(), "column fare is not an integer");
    assert!(!writer.app().status().ready);

    // Once the row is fixed, Straum starts again.
    row[3] = int(10);
    let lsn = database.borrow_mut().commit(vec![Write::Update {
        table: "trips",
        key: int(1),
        new_key: int(1),
        row,
    }]);
    assert_eq!(writer.step().unwrap(), Step::Started(lsn));
    assert_eq!(count(writer.app(), "trips"), Ok(Value::Int(1)));
}

fn trip_row() -> Row {
    vec![int(0), int(1), int(1), int(10), text("ann")]
}

fn never_added() -> Row {
    vec![int(0), int(9), int(9), int(9), text("zed")]
}

#[test]
fn a_change_that_does_not_fit_starts_over() {
    let cases = [
        (
            "fact update from a row Straum never saw",
            Change::Update {
                table: id("trips"),
                old: never_added(),
                new: trip_row(),
            },
        ),
        // The old row is already out of its cell when the new one fails.
        (
            "fact update to a row of the wrong width",
            Change::Update {
                table: id("trips"),
                old: trip_row(),
                new: vec![int(0)],
            },
        ),
        (
            "fact delete of a row Straum never saw",
            Change::Delete {
                table: id("trips"),
                old: never_added(),
            },
        ),
        (
            "fact insert of the wrong width",
            Change::Insert {
                table: id("trips"),
                row: vec![int(0)],
            },
        ),
        (
            "dim delete of a row Straum never held",
            Change::Delete {
                table: id("zones"),
                old: vec![int(7), text("Staten Island")],
            },
        ),
        (
            "dim insert of the wrong width",
            Change::Insert {
                table: id("zones"),
                row: vec![int(3)],
            },
        ),
    ];
    for (name, change) in cases {
        starts_over_after(name, change);
    }
}

fn starts_over_after(name: &str, change: Change) {
    let Harness {
        database,
        mut writer,
    } = setup();
    database
        .borrow_mut()
        .commit(vec![zone(1, "Bronx"), zone(2, "Queens"), trip(1, 1, 1, 10)]);
    writer.step().unwrap();
    let table = change.table().clone();
    let lsn = database.borrow_mut().send(vec![change]);

    let error = writer.step().unwrap_err();
    assert!(
        matches!(&error, Error::Unfit { table: unfit, .. } if *unfit == table),
        "{name}"
    );
    // The store's own words say why, after the table.
    let said = error.to_string();
    let named = format!("a change no longer fits {table}: ");
    assert!(
        said.len() > named.len() && said.starts_with(&named),
        "{name}: {said}"
    );
    assert!(!writer.app().status().ready, "{name}");
    assert_eq!(writer.step().unwrap(), Step::Started(lsn), "{name}");
    let by_borough = ask("trips", &["trips", "fare"], &["borough"]);
    assert_eq!(
        writer.app().ask(&by_borough).unwrap().answer,
        reference_answer(&database.borrow(), &by_borough),
        "{name}"
    );
}
