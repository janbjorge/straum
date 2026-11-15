//! What a cell keeps per measure: percentiles at their rank, a min and max
//! that reveal the next value when one leaves, a count of a column that
//! notices a value its cell lacks, and every kind of cell state refusing a
//! row it never saw.

use crate::common::{Rng, by_key, id, table, text};
use crate::random::trip;
use crate::rows::base_trip_with;
use crate::views::{new_measure, percentile_spec, spec, typed};
use crate::{AT, END, FARE, HOUR, PU, RIDER, START, TIP, TOLLED, WIDTH};
use straum_domain::{
    Aggregate, Answer, Ask, AskKey, Datum, Fact, Origin, RowError, Spec, Store, Time, Value, Width,
};

/// The values of each row of an answer, the keys left out.
fn values_of(answer: Answer) -> Vec<Vec<Value>> {
    let mut values = Vec::with_capacity(answer.rows.len());
    for row in answer.rows {
        values.push(row.values);
    }
    values
}

/// The named measures by pay over every bucket the tests fill.
fn percentiles(store: &Store, measures: &[&str]) -> Vec<Vec<Value>> {
    let mut names = Vec::with_capacity(measures.len());
    for name in measures {
        names.push(id(name));
    }
    let ask = Ask {
        view: id("trips"),
        measures: names,
        keys: vec![AskKey::Key(id("pay"))],
        filter: Vec::new(),
        having: Vec::new(),
        from: START,
        to: END,
    };
    values_of(store.ask(&ask).unwrap())
}

/// A row of the percentile view: its time, its pay and its fare.
fn fare_row(at: i64, pay: i64, fare: i64) -> [Datum; 3] {
    [Datum::Int(at), Datum::Int(pay), Datum::Int(fare)]
}

/// Pay 1 holds 10 and 20 in one bucket, 20, 40 and 90 in the next; pay 2
/// one row of 5. Five values put the median at rank 3 and p99 at 5; a
/// delete moves p99 to the next value down.
#[test]
fn a_percentile_picks_the_value_at_its_rank() {
    let mut store = Store::new(percentile_spec());
    let width = i64::from(WIDTH);
    for (at, fare) in [(0, 10), (0, 20), (width, 20), (width, 40), (width, 90)] {
        store
            .add_row("trips", &fare_row(START + at, 1, fare))
            .unwrap();
    }
    store.add_row("trips", &fare_row(START, 2, 5)).unwrap();
    store
        .add_row("trips", &[Datum::Int(START), Datum::Int(2), Datum::Null])
        .unwrap();
    let asked = ["median", "p99", "lowest", "highest"];
    let int = Value::Int;
    assert_eq!(
        percentiles(&store, &asked),
        [
            vec![int(20), int(90), int(10), int(90)],
            vec![int(5), int(5), int(5), int(5)],
        ]
    );

    store
        .remove_row("trips", &fare_row(START + width, 1, 90))
        .unwrap();
    store.remove_row("trips", &fare_row(START, 2, 5)).unwrap();
    assert_eq!(
        percentiles(&store, &asked),
        [
            vec![int(20), int(40), int(10), int(40)],
            vec![Value::Null, Value::Null, Value::Null, Value::Null],
        ]
    );
}

/// PostgreSQL ranks in float8: 0.07 of 100 is 7.000000000000001 there,
/// so it gives the 8th value where exact math gives the 7th.
#[test]
fn a_percentile_ranks_in_float8_as_postgresql_does() {
    let mut store = Store::new(percentile_spec());
    for fare in 1..=100 {
        store
            .add_row(
                "trips",
                &[Datum::Int(START), Datum::Int(1), Datum::Int(fare)],
            )
            .unwrap();
    }
    assert_eq!(percentiles(&store, &["p07"]), [vec![Value::Int(8)]]);
}

#[test]
fn rows_collapse_into_cells() {
    let mut store = Store::new(spec());
    let mut row = trip(&mut Rng(7));
    row[AT] = Datum::Int(START + 1);
    store.add_row("trips", &row).unwrap();
    row[AT] = Datum::Int(START + i64::from(WIDTH) - 1);
    store.add_row("trips", &row).unwrap();
    assert_eq!(store.cells("trips"), Some(1));
    row[AT] = Datum::Int(START + i64::from(WIDTH));
    store.add_row("trips", &row).unwrap();
    assert_eq!(store.cells("trips"), Some(2));
    // A join column is a key: another value is another cell.
    row[PU] = Datum::Int(9);
    store.add_row("trips", &row).unwrap();
    assert_eq!(store.cells("trips"), Some(3));
    // A dim row is none.
    store
        .add_row("zones", &[Datum::Int(9), text("m"), text("a")])
        .unwrap();
    assert_eq!(store.cells("trips"), Some(3));
}

fn first_values(store: &Store, ask: &Ask) -> Vec<Value> {
    store.ask(ask).unwrap().rows[0].values.clone()
}

#[test]
fn removing_an_extreme_reveals_the_next() {
    let mut store = Store::new(spec());
    let ask = Ask {
        view: id("trips"),
        measures: vec![id("tip_min"), id("tip_max")],
        keys: Vec::new(),
        filter: Vec::new(),
        having: Vec::new(),
        from: START,
        to: END,
    };
    let mut row = trip(&mut Rng(11));
    row[AT] = Datum::Int(START);
    let mut rows = Vec::new();
    for tip in [5, 9, 9, 2, 7] {
        row[TIP] = Datum::Int(tip);
        rows.push(row.clone());
    }
    for row in &rows {
        store.add_row("trips", row).unwrap();
    }
    assert_eq!(store.cells("trips"), Some(1));
    assert_eq!(first_values(&store, &ask), [Value::Int(2), Value::Int(9)]);

    // One of two 9s out: the max stays.
    store.remove_row("trips", &rows[1]).unwrap();
    assert_eq!(first_values(&store, &ask), [Value::Int(2), Value::Int(9)]);
    store.remove_row("trips", &rows[2]).unwrap();
    assert_eq!(first_values(&store, &ask), [Value::Int(2), Value::Int(7)]);
    store.remove_row("trips", &rows[3]).unwrap();
    assert_eq!(first_values(&store, &ask), [Value::Int(5), Value::Int(7)]);
    // A value past the old extremes after they went.
    row[TIP] = Datum::Int(1);
    store.add_row("trips", &row).unwrap();
    assert_eq!(first_values(&store, &ask), [Value::Int(1), Value::Int(7)]);
}

/// A row of the view of tips: its time, its pay and its tip.
fn tip_row(pay: i64, tip: Datum) -> Vec<Datum> {
    vec![Datum::Int(START), Datum::Int(pay), tip]
}

/// The trips and those tipped, by `keys`, over the first hour.
fn trips_and_tipped(store: &Store, keys: Vec<AskKey>) -> Vec<Vec<Value>> {
    let ask = Ask {
        view: id("trips"),
        measures: vec![id("trips"), id("tipped")],
        keys,
        filter: Vec::new(),
        having: Vec::new(),
        from: START,
        to: START + i64::from(HOUR),
    };
    values_of(store.ask(&ask).unwrap())
}

/// A row of [`trips_and_tipped`].
fn trips_and_tipped_pair(trips: i128, tipped: i128) -> Vec<Value> {
    vec![Value::Int(trips), Value::Int(tipped)]
}

/// A count of a column that nothing else reads: no sum, min or max is
/// there to notice a value its cell lacks, so the count has to. It counts
/// the nulls among a cell's rows, and tells from them both a null the
/// cell never held and a value where it holds only nulls.
#[test]
fn a_count_alone_notices_a_value_its_cell_lacks() {
    let trips = Fact {
        as_text: Vec::new(),
        name: id("trips"),
        table: table("trips"),
        time: Time {
            column: id("at"),
            width: Width { seconds: WIDTH },
            wider: vec![Width { seconds: HOUR }],
            origin: Origin::Timestamptz,
        },
        refs: Vec::new(),
        by_keys: vec![by_key("pay")],
        filter: vec![],
        measures: vec![
            new_measure("trips", Aggregate::Count, vec![]),
            new_measure("tipped", Aggregate::CountNotNull(id("tip")), vec![]),
        ],
    };
    let spec = Spec::new(Vec::new(), vec![trips]).unwrap();
    // What the count makes of a row that holds each of the two.
    for (held, lacked, of_held, of_lacked) in [
        (Datum::Null, Datum::Int(5), 0, 1),
        (Datum::Int(5), Datum::Null, 1, 0),
    ] {
        let mut store = Store::new(spec.clone());
        // Two cells, so an ask merges them into one group and into two.
        for pay in [1, 1, 2] {
            store.add_row("trips", &tip_row(pay, held.clone())).unwrap();
        }
        store.add_row("trips", &tip_row(2, lacked.clone())).unwrap();
        assert_eq!(
            trips_and_tipped(&store, Vec::new()),
            [trips_and_tipped_pair(4, 3 * of_held + of_lacked)]
        );
        assert_eq!(
            trips_and_tipped(&store, vec![AskKey::Key(id("pay"))]),
            [
                trips_and_tipped_pair(2, 2 * of_held),
                trips_and_tipped_pair(2, of_held + of_lacked)
            ]
        );
        assert_eq!(
            store.remove_row("trips", &tip_row(1, lacked)),
            Err(RowError::Missing),
            "{held:?} held"
        );
        // What a cell holds leaves it. The store above missed a row, so
        // it no longer matches its table.
        let mut store = Store::new(spec.clone());
        store.add_row("trips", &tip_row(1, held.clone())).unwrap();
        assert_eq!(store.remove_row("trips", &tip_row(1, held)), Ok(()));
        assert_eq!(store.cells("trips"), Some(0));
    }
}

/// A row whose tip its cell doesn't hold was never added, whichever form
/// the cell's count per value has: in the cell, a run or several.
#[test]
fn a_tip_its_cell_lacks_is_missing() {
    for values in [1, 5, 300, 3_000] {
        let mut store = typed(&spec());
        let mut row = trip(&mut Rng(7));
        row[AT] = Datum::Int(START);
        for tip in 0..values {
            row[TIP] = Datum::Int(tip);
            store.add_row("trips", &row).unwrap();
        }
        assert_eq!(store.cells("trips"), Some(1));
        row[TIP] = Datum::Int(-1);
        assert_eq!(
            store.remove_row("trips", &row),
            Err(RowError::Missing),
            "{values} values"
        );
    }
}

#[test]
fn each_kind_of_state_notices_a_value_it_never_saw() {
    let seen = base_trip_with(FARE, Datum::Null);
    let untolled = Datum::Bool(seen[TOLLED] != Datum::Bool(true));
    for (position, datum) in [
        (FARE, Datum::Int(1)),
        (TIP, Datum::Int(301)),
        (RIDER, text("zed")),
        (TOLLED, untolled),
    ] {
        let mut store = Store::new(spec());
        store.add_row("trips", &seen).unwrap();
        let mut other = seen.clone();
        other[position] = datum;
        assert_eq!(
            store.remove_row("trips", &other),
            Err(RowError::Missing),
            "{position}"
        );
    }
}

/// A null is a value where a sum or a count reads the column, so one
/// the cell never held is noticed too.
#[test]
fn a_null_the_cell_never_held_is_noticed() {
    let mut full = base_trip_with(FARE, Datum::Int(5));
    (full[TIP], full[RIDER]) = (Datum::Int(3), text("ann"));
    for position in [FARE, TIP, RIDER] {
        let mut store = Store::new(spec());
        store.add_row("trips", &full).unwrap();
        let mut other = full.clone();
        other[position] = Datum::Null;
        assert_eq!(
            store.remove_row("trips", &other),
            Err(RowError::Missing),
            "a null at {position}"
        );
        // One it does hold leaves. A store that missed a row no longer
        // matches its table, so this is another.
        let mut store = Store::new(spec());
        store.add_row("trips", &full).unwrap();
        store.add_row("trips", &other).unwrap();
        assert_eq!(
            store.remove_row("trips", &other),
            Ok(()),
            "a null at {position}"
        );
        assert_eq!(store.remove_row("trips", &full), Ok(()), "{position}");
        assert_eq!(store.cells("trips"), Some(0), "{position}");
    }
}
