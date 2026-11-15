//! Bound cells: the second rollup of a fact, keyed by the dim columns its
//! joins read, which answer an ask with fewer cells than the keyed cells.
//! A dim change moves them, and a sealed bucket leaves them until it is
//! unsealed, whatever happened to the dim meanwhile.

use crate::common::{id, text};
use crate::views::{Widths, zones_spec};
use crate::{END, HOUR, START, WIDTH};
use straum_domain::{Ask, AskKey, Datum, RefKind, Store, Value};

/// Zones 1 to 3 in borough m, 4 to 6 in q, and a trip from each at
/// `START`.
pub(crate) fn zoned_store() -> Store {
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

pub(crate) fn by_borough() -> Ask {
    by_borough_over(START, END)
}

pub(crate) fn by_borough_over(from: i64, to: i64) -> Ask {
    Ask {
        view: id("trips"),
        measures: vec![id("trips")],
        keys: vec![AskKey::Key(id("borough"))],
        filter: Vec::new(),
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

/// Sealing a bucket takes its bound cells with it, so an ask over the
/// bucket reads the keyed cells, 6 for 2. Once the keyed cells are back
/// they are bound again, and a row of the bucket goes into both.
#[test]
fn a_sealed_bucket_is_read_through_its_keyed_cells() {
    let mut store = zoned_store();
    let by_borough = by_borough();
    let id = store.sealable(START + i64::from(WIDTH)).remove(0);
    let copy = store.sealed_copy(&id).unwrap();
    store.seal(&id).unwrap();
    assert_eq!(store.sealed_read(&by_borough), Ok(vec![id.clone()]));
    assert_eq!(store.cells_read(&by_borough), Ok(6));
    assert_eq!(store.bound_cells("trips"), Some(0));
    let thawed = store.thaw(copy).unwrap();
    store.unseal(thawed).unwrap();
    assert_eq!(store.sealed_read(&by_borough), Ok(vec![]));

    store
        .add_row("trips", &[Datum::Int(START), Datum::Int(1), Datum::Int(1)])
        .unwrap();
    assert_eq!(store.cells_read(&by_borough), Ok(2));
    assert_eq!(store.bound_cells("trips"), Some(2));
    let answer = store.ask(&by_borough).unwrap();
    assert_eq!(answer.rows[0].values, [Value::Int(4)]);
    assert_eq!(answer.rows[1].values, [Value::Int(3)]);
    store
        .remove_row("trips", &[Datum::Int(START), Datum::Int(1), Datum::Int(1)])
        .unwrap();
    assert_eq!(
        store.ask(&by_borough).unwrap().rows[0].values,
        [Value::Int(3)]
    );
}

pub(crate) fn load_three_zones_and_trips(store: &mut Store) {
    store.add_row("zones", &[Datum::Int(1), text("m")]).unwrap();
    store.add_row("zones", &[Datum::Int(2), text("m")]).unwrap();
    store.add_row("zones", &[Datum::Int(3), text("m")]).unwrap();
    store
        .add_row("trips", &[Datum::Int(START), Datum::Int(1), Datum::Int(1)])
        .unwrap();
    let later = START + i64::from(WIDTH);
    store
        .add_row("trips", &[Datum::Int(later), Datum::Int(1), Datum::Int(2)])
        .unwrap();
    store
        .add_row("trips", &[Datum::Int(later), Datum::Int(1), Datum::Int(3)])
        .unwrap();
}

/// A sealed quarter stays on the keyed cells, including the hour that
/// contains it. The next quarter, which the seal does not cover, keeps
/// its bound cells after a dim change.
#[test]
fn a_hot_quarter_keeps_bound_cells_beside_a_sealed_one() {
    let spec = zones_spec(Some(RefKind::Left), Widths::QuarterAndHour);
    let mut sealed = Store::new(spec.clone());
    let mut reference = Store::new(spec);
    load_three_zones_and_trips(&mut sealed);
    load_three_zones_and_trips(&mut reference);
    let sealed_id = sealed.sealable(START + i64::from(WIDTH)).remove(0);
    let copy = sealed.sealed_copy(&sealed_id).unwrap();
    sealed.seal(&sealed_id).unwrap();
    let old = [Datum::Int(1), text("m")];
    let new = [Datum::Int(1), text("q")];
    sealed.update_row("zones", &old, &new).unwrap();
    reference.update_row("zones", &old, &new).unwrap();

    let hot = by_borough_over(START + i64::from(WIDTH), START + 2 * i64::from(WIDTH));
    let hour = by_borough_over(START, START + i64::from(HOUR));
    assert_eq!(sealed.sealed_read(&hot).unwrap(), []);
    // Two keyed cells, one borough: the bound cells answer.
    assert_eq!(sealed.cells_read(&hot).unwrap(), 1);
    assert_eq!(sealed.ask(&hot).unwrap(), reference.ask(&hot).unwrap());
    // The hour also covers the sealed quarter. Its answer matches the
    // reference, which has zone 1's new borough.
    let thawed = sealed.thaw(copy).unwrap();
    assert_eq!(
        sealed.ask_with(&hour, &[thawed]).unwrap(),
        reference.ask(&hour).unwrap()
    );
}

/// Seals the bucket at `START`, hands it to `between`, puts it back and
/// adds one more trip from zone 1 to it.
pub(crate) fn through_a_seal(store: &mut Store, between: impl Fn(&mut Store)) {
    let id = store.sealable(START + i64::from(WIDTH)).remove(0);
    let copy = store.sealed_copy(&id).unwrap();
    store.seal(&id).unwrap();
    between(store);
    let thawed = store.thaw(copy).unwrap();
    store.unseal(thawed).unwrap();
    store
        .add_row("trips", &[Datum::Int(START), Datum::Int(1), Datum::Int(1)])
        .unwrap();
}

/// The same changes with the bucket hot throughout.
pub(crate) fn without_a_seal(store: &mut Store, between: impl Fn(&mut Store)) {
    between(store);
    store
        .add_row("trips", &[Datum::Int(START), Datum::Int(1), Datum::Int(1)])
        .unwrap();
}

pub(crate) fn move_zone_six_to_m(store: &mut Store) {
    let old = [Datum::Int(6), text("q")];
    let new = [Datum::Int(6), text("m")];
    store.update_row("zones", &old, &new).unwrap();
}

pub(crate) fn truncate_zones(store: &mut Store) {
    store.clear("zones");
}

pub(crate) fn reload_zones_into_b(store: &mut Store) {
    store.clear("zones");
    let rows = [Datum::Int(1), text("b"), Datum::Int(2), text("b")];
    store.add_rows("zones", &rows).unwrap();
}

pub(crate) fn insert_zone_one(store: &mut Store) {
    store.add_row("zones", &[Datum::Int(1), text("m")]).unwrap();
}

pub(crate) fn inner_zoned_store_with(trip: &[Datum]) -> Store {
    let mut store = Store::new(zones_spec(Some(RefKind::Inner), Widths::Quarter));
    store.add_row("trips", trip).unwrap();
    assert_eq!(store.bound_cells("trips"), Some(0));
    store
}

/// A dim row that changes while a bucket is sealed reaches the bucket's
/// trips once it is back: the bound cells answer, as they would had it
/// never been sealed, and every trip still comes out.
#[test]
fn a_dim_change_under_a_sealed_bucket_reaches_it_once_unsealed() {
    let (mut sealed, mut hot) = (zoned_store(), zoned_store());
    through_a_seal(&mut sealed, move_zone_six_to_m);
    without_a_seal(&mut hot, move_zone_six_to_m);
    let by_borough = by_borough();
    assert_eq!(sealed.cells_read(&by_borough), Ok(2));
    let answer = sealed.ask(&by_borough).unwrap();
    assert_eq!(answer, hot.ask(&by_borough).unwrap());
    assert_eq!(answer.rows[0].values, [Value::Int(5)]);
    assert_eq!(answer.rows[1].values, [Value::Int(2)]);
    for zone in 1..7 {
        let row = [Datum::Int(START), Datum::Int(1), Datum::Int(zone)];
        sealed.remove_row("trips", &row).unwrap();
    }
    assert_eq!(sealed.cells("trips"), Some(1));
    assert_eq!(sealed.bound_cells("trips"), Some(1));
}

/// A dim truncated while a bucket is sealed rebuilds the bound cells
/// without the bucket. Once it is back its trips are bound again, not
/// only the one that came after.
#[test]
fn a_dim_truncate_under_a_sealed_bucket_keeps_its_trips() {
    let (mut sealed, mut hot) = (zoned_store(), zoned_store());
    through_a_seal(&mut sealed, truncate_zones);
    without_a_seal(&mut hot, truncate_zones);
    let by_borough = by_borough();
    let answer = sealed.ask(&by_borough).unwrap();
    assert_eq!(answer, hot.ask(&by_borough).unwrap());
    assert_eq!(answer.rows[0].keys, [Datum::Null]);
    assert_eq!(answer.rows[0].values, [Value::Int(7)]);
}

/// As the truncate, for a dim loaded while the bucket is sealed.
#[test]
fn a_dim_load_under_a_sealed_bucket_keeps_its_trips() {
    let (mut sealed, mut hot) = (zoned_store(), zoned_store());
    through_a_seal(&mut sealed, reload_zones_into_b);
    without_a_seal(&mut hot, reload_zones_into_b);
    let by_borough = by_borough();
    let answer = sealed.ask(&by_borough).unwrap();
    assert_eq!(answer, hot.ask(&by_borough).unwrap());
    assert_eq!(answer.rows[0].keys, [Datum::Null]);
    assert_eq!(answer.rows[0].values, [Value::Int(4)]);
    assert_eq!(answer.rows[1].values, [Value::Int(3)]);
}

/// An inner join that keeps no trip of a bucket leaves it no bound cells
/// to seal. A zone that comes while it is sealed binds its trips once it
/// is back, and both still come out.
#[test]
fn a_sealed_bucket_without_bound_cells_gains_them_from_a_dim_insert() {
    let first = [Datum::Int(START), Datum::Int(2), Datum::Int(1)];
    let (mut sealed, mut hot) = (
        inner_zoned_store_with(&first),
        inner_zoned_store_with(&first),
    );
    through_a_seal(&mut sealed, insert_zone_one);
    without_a_seal(&mut hot, insert_zone_one);
    let by_borough = by_borough();
    let answer = sealed.ask(&by_borough).unwrap();
    assert_eq!(answer, hot.ask(&by_borough).unwrap());
    assert_eq!(answer.rows[0].values, [Value::Int(2)]);
    sealed.remove_row("trips", &first).unwrap();
    let second = [Datum::Int(START), Datum::Int(1), Datum::Int(1)];
    sealed.remove_row("trips", &second).unwrap();
    assert_eq!(sealed.bound_cells("trips"), Some(0));
}
