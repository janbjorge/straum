//! What a row must be to go in or come out: as wide as the table, of the
//! types its measures read, and held before it is removed. Also where the
//! columns of a row and the parts of the rollup key lie.

use crate::common::{Rng, id, text};
use crate::random::trip;
use crate::views::spec;
use crate::{AT, DIST, DRO, FARE, HOME, PAY, PU, RIDER, ROW_COLUMNS, SHARED, TIP, TOLLED, VENDOR};
use straum_domain::{ColumnType, Datum, Identifier, KeyPart, RowError, Store};

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
        (RowError::Sealed, "row falls in a sealed bucket"),
    ];
    for (error, message) in cases {
        assert_eq!(error.to_string(), message);
    }
}
