//! What a row must be to go in or come out: as wide as the table, of the
//! types its measures read, and held before it is removed. Also where the
//! columns of a row and the parts of the rollup key lie.

use crate::common::{Rng, by_key, id, table, text};
use crate::random::trip;
use crate::views::{new_measure, spec};
use crate::{
    AT, DIST, DRO, FARE, HOME, PAY, PU, RIDER, ROW_COLUMNS, SHARED, START, TIP, TOLLED, VENDOR,
    WIDTH,
};
use straum_domain::{
    Aggregate, ColumnType, Datum, Fact, Identifier, KeyPart, RowError, Spec, Store, Time, Width,
};

#[test]
fn rows_added_together_add_none_on_error() {
    let mut rng = Rng(5);
    let mut store = Store::new(spec());
    let good = [trip(&mut rng), trip(&mut rng)].concat();
    assert_eq!(
        store.add_rows("trips", &good[1..]),
        Err(RowError::Arity {
            expected: ROW_COLUMNS,
            found: ROW_COLUMNS - 1
        })
    );
    let mut bad = good.clone();
    bad[ROW_COLUMNS + AT] = text("noon");
    assert_eq!(
        store.add_rows("trips", &bad),
        Err(RowError::NotInt { column: id("at") })
    );
    assert_eq!(store.cells("trips"), Some(0));
    store.add_rows("trips", &[]).unwrap();
    store.add_rows("trips", &good).unwrap();
    assert!(store.cells("trips").unwrap() > 0);
}

#[test]
fn columns_put_measure_inputs_after_the_key() {
    let spec = spec();
    let columns: Vec<_> = spec
        .fact("trips")
        .unwrap()
        .columns()
        .iter()
        .map(Identifier::to_string)
        .collect();
    let expected = [
        (AT, "at"),
        (PAY, "pay"),
        (VENDOR, "vendor"),
        (SHARED, "shared"),
        (PU, "pu"),
        (DRO, "dro"),
        (HOME, "home"),
        (FARE, "fare"),
        (TIP, "tip"),
        (DIST, "dist"),
        (RIDER, "rider"),
        (TOLLED, "tolled"),
    ];
    assert_eq!(columns.len(), expected.len());
    for (position, name) in expected {
        assert_eq!(columns[position], name);
    }
}

/// The refs' via columns follow the by keys in the rollup key, each slot
/// once: here `home` is the one via that is no by column.
#[test]
fn rollup_key_puts_via_columns_after_the_by_keys_each_once() {
    let spec = spec();
    let mut key = Vec::new();
    for part in spec.fact("trips").unwrap().rollup_key() {
        key.push(match part {
            KeyPart::Column(column) => column.to_string(),
            KeyPart::IsNull {
                column,
                negated: true,
            } => format!("{column} is not null"),
            KeyPart::IsNull {
                column,
                negated: false,
            } => format!("{column} is null"),
            KeyPart::Bucket => "bucket".to_owned(),
        });
    }
    assert_eq!(
        key,
        [
            "pay",
            "vendor",
            "shared",
            "pu",
            "dro",
            "shared is not null",
            "pay is null",
            "home",
            "bucket",
        ]
    );
}

#[test]
fn clear_drops_every_row() {
    let mut store = Store::new(spec());
    let trip = trip(&mut Rng(5));
    store.add_row("trips", &trip).unwrap();
    store.clear("trips");
    assert_eq!(store.cells("trips"), Some(0));
    assert_eq!(store.remove_row("trips", &trip), Err(RowError::Missing));
    store.add_row("trips", &trip).unwrap();
    assert_eq!(store.cells("trips"), Some(1));
}

/// The trip of seed 3, one column changed.
pub(crate) fn base_trip_with(column: usize, datum: Datum) -> Vec<Datum> {
    let mut row = trip(&mut Rng(3));
    row[column] = datum;
    row
}

#[test]
fn row_errors() {
    let base = trip(&mut Rng(3));
    let mut store = Store::new(spec());
    assert_eq!(
        store.add_row("trips", &base[1..]),
        Err(RowError::Arity {
            expected: ROW_COLUMNS,
            found: ROW_COLUMNS - 1
        })
    );
    assert_eq!(
        store.add_row("trips", &base_trip_with(AT, text("noon"))),
        Err(RowError::NotInt { column: id("at") })
    );
    assert_eq!(
        store.remove_row("trips", &base_trip_with(TIP, Datum::Bool(true))),
        Err(RowError::NotInt { column: id("tip") })
    );
    assert_eq!(
        store.add_row("trips", &base_trip_with(TOLLED, Datum::Int(1))),
        Err(RowError::NotBool {
            column: id("tolled")
        })
    );
    // Count distinct takes any value.
    store
        .add_row("trips", &base_trip_with(RIDER, Datum::Bool(true)))
        .unwrap();
    assert_eq!(store.remove_row("trips", &base), Err(RowError::Missing));

    let mut store = Store::new(spec());
    assert_eq!(
        store.set_types("trips", &[ColumnType::Int; ROW_COLUMNS + 1]),
        Err(RowError::Arity {
            expected: ROW_COLUMNS,
            found: ROW_COLUMNS + 1
        })
    );
}

/// Trips by pay with the sum of their fares, joined to nothing: no bound
/// cell or other state of the fare stands in for a check the keyed cells
/// make.
fn fare_sum_spec() -> Spec {
    let trips = Fact {
        as_text: Vec::new(),
        name: id("trips"),
        table: table("trips"),
        time: Time {
            column: id("at"),
            width: Width { seconds: WIDTH },
            wider: Vec::new(),
            name: id("bucket"),
        },
        refs: Vec::new(),
        by_keys: vec![by_key("pay")],
        filter: vec![],
        measures: vec![new_measure("fare", Aggregate::Sum(id("fare")), vec![])],
    };
    Spec::new(Vec::new(), vec![trips]).unwrap()
}

/// A trip of [`fare_sum_spec`].
fn fare_trip(spec: &Spec, pay: i64, fare: &Datum) -> Vec<Datum> {
    fare_trip_at(spec, START, pay, fare)
}

/// As [`fare_trip`], at `at`.
fn fare_trip_at(spec: &Spec, at: i64, pay: i64, fare: &Datum) -> Vec<Datum> {
    let mut row = Vec::new();
    for column in spec.fact("trips").unwrap().columns() {
        row.push(match column.as_str() {
            "at" => Datum::Int(at),
            "pay" => Datum::Int(pay),
            _ => fare.clone(),
        });
    }
    row
}

/// A row never added is missed, and leaves the store as it was: one
/// whose bucket holds other cells but not its own, and one whose cell
/// holds only nulls where it has a value.
#[test]
fn a_row_never_added_is_missed_beside_rows_that_were() {
    let spec = fare_sum_spec();
    let mut store = Store::new(spec.clone());
    store
        .add_row("trips", &fare_trip(&spec, 1, &Datum::Int(500)))
        .unwrap();
    // Pay 2 is known, in the next bucket, but has no cell in this one.
    let later = START + i64::from(WIDTH);
    store
        .add_row("trips", &fare_trip_at(&spec, later, 2, &Datum::Int(500)))
        .unwrap();
    let other_pay = fare_trip(&spec, 2, &Datum::Int(500));
    assert_eq!(
        store.remove_row("trips", &other_pay),
        Err(RowError::Missing)
    );
    assert_eq!(store.cells("trips"), Some(2));

    let mut store = Store::new(spec.clone());
    let unpriced = fare_trip(&spec, 1, &Datum::Null);
    store.add_row("trips", &unpriced).unwrap();
    let priced = fare_trip(&spec, 1, &Datum::Int(500));
    assert_eq!(store.remove_row("trips", &priced), Err(RowError::Missing));
    store.remove_row("trips", &unpriced).unwrap();
    assert_eq!(store.cells("trips"), Some(0));
}

#[test]
fn row_error_display() {
    let cases = [
        (
            RowError::Arity {
                expected: 3,
                found: 1,
            },
            "row has 1 columns, expected 3",
        ),
        (
            RowError::NotInt { column: id("tip") },
            "column tip is not an integer",
        ),
        (
            RowError::NotBool {
                column: id("tolled"),
            },
            "column tolled is not a boolean",
        ),
        (RowError::Missing, "removed row was never added"),
    ];
    for (error, message) in cases {
        assert_eq!(error.to_string(), message);
    }
}
