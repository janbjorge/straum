//! Bound cells: the second rollup of a fact, keyed by the dim columns its
//! joins read, which answer an ask with fewer cells than the keyed cells.
//! A dim change moves them.

use crate::common::{id, text};
use crate::views::{Widths, zones_spec};
use crate::{START, trips_ask};
use straum_domain::{Ask, AskKey, Attribute, Datum, RefKind, Spec, Store, Value};

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
    trips_ask(&["trips"], vec![AskKey::Key(id("borough"))])
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

/// An update that keeps the zone's key moves its trips in one step, as
/// one that changes the key moves them out; a zone no trip names moves
/// nothing, and binds the trip that names it later.
#[test]
fn a_zone_update_moves_only_the_trips_that_name_it() {
    let mut store = zoned_store();
    let by_borough = by_borough();
    store
        .update_row(
            "zones",
            &[Datum::Int(6), text("q")],
            &[Datum::Int(6), text("m")],
        )
        .unwrap();
    let moved = store.ask(&by_borough).unwrap();
    assert_eq!(moved.rows[0].keys, [text("m")]);
    assert_eq!(moved.rows[0].values, [Value::Int(4)]);
    assert_eq!(moved.rows[1].values, [Value::Int(2)]);

    store.add_row("zones", &[Datum::Int(7), text("x")]).unwrap();
    assert_eq!(store.ask(&by_borough).unwrap(), moved);
    store
        .add_row("trips", &[Datum::Int(START), Datum::Int(1), Datum::Int(7)])
        .unwrap();
    let named = store.ask(&by_borough).unwrap();
    assert_eq!(named.rows[2].keys, [text("x")]);

    // Zone 6 is now 8, which no trip names: its trip finds no zone.
    store
        .update_row(
            "zones",
            &[Datum::Int(6), text("m")],
            &[Datum::Int(8), text("m")],
        )
        .unwrap();
    let rekeyed = store.ask(&by_borough).unwrap();
    assert_eq!(rekeyed.rows[0].keys, [Datum::Null]);
    assert_eq!(rekeyed.rows[0].values, [Value::Int(1)]);
    assert_eq!(rekeyed.rows[1].values, [Value::Int(3)]);
}

/// A ref may read one dim column under two names, each a key of its own
/// with the same values; the bound cells key both.
#[test]
fn a_dim_column_read_under_two_names_answers_both() {
    let spec = zones_spec(Some(RefKind::Left), Widths::Quarter);
    let mut facts = spec.facts().to_vec();
    facts[0].refs[0].reads.push(Attribute {
        name: id("area"),
        column: id("borough"),
    });
    let spec = Spec::new(spec.dims().to_vec(), facts).unwrap();
    let mut store = Store::new(spec);
    store.add_row("zones", &[Datum::Int(1), text("m")]).unwrap();
    store
        .add_row("trips", &[Datum::Int(START), Datum::Int(1), Datum::Int(1)])
        .unwrap();
    let mut by_area = by_borough();
    by_area.keys = vec![AskKey::Key(id("borough")), AskKey::Key(id("area"))];
    let answer = store.ask(&by_area).unwrap();
    assert_eq!(answer.rows.len(), 1);
    assert_eq!(answer.rows[0].keys, [text("m"), text("m")]);
}

/// Codes that go once most are dead leave every answer as it was: trips
/// from thousands of zones come and go, past the point where the zone ids'
/// codes are given again, and the zones' boroughs are renamed thousands
/// of times; the answers then are the ones the few trips and zones left
/// give, and a dim change still moves the trips that name it.
#[test]
fn answers_hold_across_a_recode() {
    let mut store = zoned_store();
    let mut many = Vec::new();
    for zone in 7..10_007 {
        many.push([Datum::Int(START), Datum::Int(1), Datum::Int(zone)]);
    }
    for row in &many {
        store.add_row("trips", row).unwrap();
    }
    for row in &many {
        store.remove_row("trips", row).unwrap();
    }
    let mut name = text("q");
    for rename in 0..10_000 {
        let next = text(&format!("q{rename}"));
        store
            .update_row(
                "zones",
                &[Datum::Int(4), name],
                &[Datum::Int(4), next.clone()],
            )
            .unwrap();
        name = next;
    }
    store
        .update_row("zones", &[Datum::Int(4), name], &[Datum::Int(4), text("q")])
        .unwrap();
    let answer = store.ask(&by_borough()).unwrap();
    assert_eq!(answer.rows.len(), 2);
    assert_eq!(
        (answer.rows[0].keys.clone(), answer.rows[0].values.clone()),
        (vec![text("m")], vec![Value::Int(3)])
    );
    assert_eq!(
        (answer.rows[1].keys.clone(), answer.rows[1].values.clone()),
        (vec![text("q")], vec![Value::Int(3)])
    );

    // A zone id seen before the recode joins again; a zone removed moves
    // its trip out of its borough.
    store
        .add_row(
            "trips",
            &[Datum::Int(START), Datum::Int(1), Datum::Int(9_000)],
        )
        .unwrap();
    store
        .remove_row("zones", &[Datum::Int(1), text("m")])
        .unwrap();
    let answer = store.ask(&by_borough()).unwrap();
    assert_eq!(answer.rows[0].keys, [Datum::Null]);
    assert_eq!(answer.rows[0].values, [Value::Int(2)]);
    assert_eq!(answer.rows[1].values, [Value::Int(2)]);
}
