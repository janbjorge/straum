//! Bound cells: the second rollup of a fact, keyed by the dim columns its
//! joins read, which answer an ask with fewer cells than the keyed cells.
//! A dim change moves them.

use crate::common::{id, text};
use crate::views::{Widths, zones_spec};
use crate::{END, START};
use straum_domain::{Ask, AskKey, Datum, RefKind, Store, Value};

/// Zones 1 to 3 in borough m, 4 to 6 in q, and a trip from each at
/// `START`.
fn zoned_store() -> Store {
    let mut store = Store::new(zones_spec(Some(RefKind::Left), Widths::Quarter));
    for zone in 1..7 {
        let borough = if zone < 4 { "m" } else { "q" };
        store
            .add_row("zones", &[Datum::Int(zone), text(borough)])
            .unwrap();
    }
    for zone in 1..7 {
        store
            .add_row(
                "trips",
                &[Datum::Int(START), Datum::Int(1), Datum::Int(zone)],
            )
            .unwrap();
    }
    store
}

/// The trips by borough over every bucket the tests fill.
fn by_borough() -> Ask {
    by_borough_over(START, END)
}

/// The trips by borough over `from..to`.
fn by_borough_over(from: i64, to: i64) -> Ask {
    Ask {
        view: id("trips"),
        measures: vec![id("trips")],
        keys: vec![AskKey::Key(id("borough"))],
        filter: Vec::new(),
        having: Vec::new(),
        from,
        to,
    }
}

/// The bound cells are keyed by the dim's column, so the zones' boroughs
/// make fewer of them than the zone ids make keyed cells, and an ask
/// reads the fewer. A zone change moves those cells and the ask still
/// reads them.
#[test]
fn bound_cells_follow_the_dim_column() {
    let mut store = zoned_store();
    let by_borough = by_borough();
    let answer = store.ask(&by_borough).unwrap();
    assert_eq!(store.cells("trips"), Some(6));
    assert_eq!(store.bound_cells("trips"), Some(2));
    assert_eq!(store.cells_read(&by_borough), Ok(2));

    store
        .remove_row("zones", &[Datum::Int(6), text("q")])
        .unwrap();
    store.add_row("zones", &[Datum::Int(6), text("m")]).unwrap();
    assert_eq!(store.bound_cells("trips"), Some(2));
    assert_eq!(store.cells_read(&by_borough), Ok(2));
    let moved = store.ask(&by_borough).unwrap();
    assert_eq!(answer.rows[0].values, [Value::Int(3)]);
    assert_eq!(moved.rows[0].values, [Value::Int(4)]);
    assert_eq!(moved.columns, answer.columns);
}

/// A fact without refs has nothing to bind: its keyed cells are its
/// answer, however its rows come.
#[test]
fn a_fact_without_refs_keeps_no_bound_cells() {
    let mut store = Store::new(zones_spec(None, Widths::Quarter));
    // Without the join, a row carries no zone.
    store
        .add_row("trips", &[Datum::Int(START), Datum::Int(1)])
        .unwrap();
    store
        .add_rows("trips", &[Datum::Int(START), Datum::Int(2)])
        .unwrap();
    assert_eq!(store.cells("trips"), Some(2));
    assert_eq!(store.bound_cells("trips"), None);
}

/// A dim change under a fact with no rows leaves nothing bound wrong, so
/// the bound cells stay and bind the rows that come after it.
#[test]
fn a_dim_change_under_no_rows_keeps_the_bound_cells() {
    let mut store = Store::new(zones_spec(Some(RefKind::Left), Widths::Quarter));
    store.add_row("zones", &[Datum::Int(1), text("m")]).unwrap();
    store
        .remove_row("zones", &[Datum::Int(1), text("m")])
        .unwrap();
    store.add_row("zones", &[Datum::Int(1), text("q")]).unwrap();
    assert_eq!(store.bound_cells("trips"), Some(0));
    store
        .add_row("trips", &[Datum::Int(START), Datum::Int(1), Datum::Int(1)])
        .unwrap();
    assert_eq!(store.bound_cells("trips"), Some(1));
    let answer = store.ask(&by_borough()).unwrap();
    assert_eq!(answer.rows[0].keys, [text("q")]);
}
