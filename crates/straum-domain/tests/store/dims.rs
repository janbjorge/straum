//! Dims as an ask reads them: through the cell's join columns, as the dim
//! is now. A dim row relabelled, removed or added moves the answer at once;
//! a key of several columns matches on all; a key with a null joins
//! nothing.

use crate::common::{Rng, attribute, by_key, condition, id, lit, table, text};
use crate::random::trip;
use crate::ranges::first_value;
use crate::views::{new_measure, spec, typed};
use crate::{AT, DRO, END, FARE, PU, SHARED, START, VENDOR, WIDTH};
use straum_domain::{
    Aggregate, Ask, AskKey, ColumnType, Datum, Dim, Fact, Origin, Ref, RefKind, RowError, Spec,
    Store, Time, Value, Via, Width,
};

fn trip_key_type(store: &Store, name: &str) -> Option<ColumnType> {
    store.key_type("trips", &id(name))
}

/// A key has the type the load set: a fact column's, a boolean for a
/// null-ness key, the dim's for a dim column. A measure is no key.
#[test]
fn a_key_has_the_type_the_load_set() {
    let store = typed(&spec());
    assert_eq!(trip_key_type(&store, "pay"), Some(ColumnType::Int));
    assert_eq!(trip_key_type(&store, "shared"), Some(ColumnType::Bool));
    assert_eq!(trip_key_type(&store, "known"), Some(ColumnType::Bool));
    assert_eq!(
        trip_key_type(&store, "pickup_borough"),
        Some(ColumnType::Text)
    );
    assert_eq!(trip_key_type(&store, "combo_label"), Some(ColumnType::Text));
    assert_eq!(trip_key_type(&store, "trips"), None);
    assert_eq!(trip_key_type(&store, "bucket"), None);
    assert_eq!(store.key_type("zones", &id("pay")), None);
}

/// Two rows in one cell, and a row with no time, still leave after the
/// dims they join have changed.
#[test]
fn a_fact_row_still_comes_out_after_its_dim_changes() {
    let spec = spec();
    let mut store = typed(&spec);
    store
        .add_row("zones", &[Datum::Int(1), text("m"), text("a")])
        .unwrap();
    store
        .add_row("vendors", &[Datum::Int(1), text("v1")])
        .unwrap();
    store
        .add_row("combos", &[Datum::Int(1), Datum::Int(1), text("x")])
        .unwrap();
    let mut row = trip(&mut Rng(1));
    row[AT] = Datum::Int(START);
    row[PU] = Datum::Int(1);
    row[DRO] = Datum::Int(1);
    row[VENDOR] = Datum::Int(1);
    row[SHARED] = Datum::Null;
    let mut other = row.clone();
    other[FARE] = Datum::Int(99);
    let mut untimed = row.clone();
    untimed[AT] = Datum::Null;
    store.add_row("trips", &row).unwrap();
    store.add_row("trips", &other).unwrap();
    store.add_row("trips", &untimed).unwrap();
    store
        .update_row(
            "zones",
            &[Datum::Int(1), text("m"), text("a")],
            &[Datum::Int(1), text("q"), text("b")],
        )
        .unwrap();
    store.remove_row("trips", &row).unwrap();
    store.remove_row("trips", &other).unwrap();
    store.remove_row("trips", &untimed).unwrap();
}

/// The first key of each row of the answer.
fn first_keys(store: &Store, ask: &Ask) -> Vec<Datum> {
    let mut keys = Vec::new();
    for row in store.ask(ask).unwrap().rows {
        keys.push(row.keys[0].clone());
    }
    keys
}

/// A cell is keyed by the join column, not the dim's value, so a dim row
/// relabelled, removed or added moves the answer at once and moves no
/// cell. A key with no dim row reads null, as the join gives.
#[test]
fn a_dim_change_moves_the_answer_at_once() {
    let spec = spec();
    let mut store = typed(&spec);
    store
        .add_row("vendors", &[Datum::Int(1), text("v1")])
        .unwrap();
    let mut row = trip(&mut Rng(3));
    row[AT] = Datum::Int(START);
    row[VENDOR] = Datum::Int(1);
    store.add_row("trips", &row).unwrap();
    let by_label = Ask {
        view: id("trips"),
        measures: vec![id("trips")],
        keys: vec![AskKey::Key(id("vend_label"))],
        filter: Vec::new(),
        having: Vec::new(),
        from: Some(START),
        to: Some(END),
    };
    assert_eq!(first_keys(&store, &by_label), [text("v1")]);
    let cells = store.cells("trips");
    assert_eq!(store.bound_cells("trips"), cells);

    store
        .remove_row("vendors", &[Datum::Int(1), text("v1")])
        .unwrap();
    assert_eq!(store.bound_cells("trips"), cells);
    assert_eq!(first_keys(&store, &by_label), [Datum::Null]);
    store
        .add_row("vendors", &[Datum::Int(1), text("v2")])
        .unwrap();
    assert_eq!(first_keys(&store, &by_label), [text("v2")]);
    assert_eq!(store.cells("trips"), cells);
    // Another vendor's row changes nothing here.
    store
        .add_row("vendors", &[Datum::Int(2), text("v9")])
        .unwrap();
    assert_eq!(first_keys(&store, &by_label), [text("v2")]);
    assert_eq!(store.bound_cells("trips"), cells);
    store.clear("vendors");
    assert_eq!(store.bound_cells("trips"), cells);
    assert_eq!(first_keys(&store, &by_label), [Datum::Null]);
    assert_eq!(store.dim_rows("vendors"), Some(0));
    // The fact row comes out as it went in, whatever the dim did.
    store.remove_row("trips", &row).unwrap();
    assert_eq!(store.cells("trips"), Some(0));
}

/// An update of a dim row that reads the same changed no column the
/// views read, so the bound cells stay. One that reads otherwise moves
/// them, and later fact rows bind through the new row.
#[test]
fn a_dim_update_moves_the_bound_cells_when_it_reads_otherwise() {
    let spec = spec();
    let mut store = typed(&spec);
    let labelled_v1 = [Datum::Int(1), text("v1")];
    let labelled_v2 = [Datum::Int(1), text("v2")];
    store.add_row("vendors", &labelled_v1).unwrap();
    let mut row = trip(&mut Rng(5));
    row[AT] = Datum::Int(START);
    row[VENDOR] = Datum::Int(1);
    store.add_row("trips", &row).unwrap();
    assert_eq!(store.bound_cells("trips"), Some(1));

    store
        .update_row("vendors", &labelled_v1, &labelled_v1)
        .unwrap();
    assert_eq!(store.bound_cells("trips"), Some(1));
    assert_eq!(
        store.update_row("vendors", &labelled_v1[..1], &labelled_v1[..1]),
        Err(RowError::Arity {
            expected: 2,
            found: 1
        })
    );
    store
        .update_row("vendors", &labelled_v1, &labelled_v2)
        .unwrap();
    assert_eq!(store.bound_cells("trips"), Some(1));

    let mut other = trip(&mut Rng(6));
    other[AT] = Datum::Int(START + i64::from(WIDTH));
    store.update_row("trips", &row, &other).unwrap();
    assert_eq!(store.cells("trips"), Some(1));
    let flat: Vec<Datum> = [row.clone(), other.clone()].concat();
    store.add_rows("trips", &flat).unwrap();
    assert_eq!(store.cells("trips"), Some(2));
    assert_eq!(store.bound_cells("trips"), Some(2));
}

/// Combos of pickup zone 1 with vendors 1 and 2, labelled x and y, laid
/// end to end.
fn two_combos() -> Vec<Datum> {
    vec![
        Datum::Int(1),
        Datum::Int(1),
        text("x"),
        Datum::Int(1),
        Datum::Int(2),
        text("y"),
    ]
}

/// A random trip at `START` from pickup zone `pu` with `vendor`.
fn trip_at_start_from(rng: &mut Rng, pu: Datum, vendor: Datum) -> Vec<Datum> {
    let mut row = trip(rng);
    row[AT] = Datum::Int(START);
    row[PU] = pu;
    row[VENDOR] = vendor;
    row
}

/// The trips by combo label over every bucket the tests fill.
fn by_combo_label() -> Ask {
    Ask {
        view: id("trips"),
        measures: vec![id("trips")],
        keys: vec![AskKey::Key(id("combo_label"))],
        filter: Vec::new(),
        having: Vec::new(),
        from: Some(START),
        to: Some(END),
    }
}

/// A dim keyed by several columns matches on all of them; a via that is
/// null joins nothing, even when a dim row has a null there.
#[test]
fn a_composite_key_matches_every_column() {
    let spec = spec();
    let mut rng = Rng(4);
    for (pu, vendor, label) in [
        (Datum::Int(1), Datum::Int(1), text("x")),
        (Datum::Int(1), Datum::Int(2), text("y")),
        (Datum::Int(2), Datum::Int(1), Datum::Null),
        (Datum::Null, Datum::Int(2), Datum::Null),
    ] {
        let mut store = typed(&spec);
        store.add_rows("combos", &two_combos()).unwrap();
        let row = trip_at_start_from(&mut rng, pu, vendor);
        store.add_row("trips", &row).unwrap();
        let answer = store.ask(&by_combo_label()).unwrap();
        assert_eq!(answer.rows[0].keys, [label]);
    }
}

#[test]
fn a_filter_on_a_composite_key_attribute_reads_the_dim() {
    let mut store = typed(&spec());
    let unkeyed = vec![Datum::Null, Datum::Int(2), text("z")];
    store
        .add_rows("combos", &[two_combos(), unkeyed].concat())
        .unwrap();
    // A row with a null in its key is never held: nothing could join it.
    assert_eq!(store.dim_rows("combos"), Some(2));
    // Filtered on the attribute too.
    let mut by_label = by_combo_label();
    by_label.filter = vec![condition("combo_label", vec![lit("x")])];
    by_label.keys.clear();
    let row = trip_at_start_from(&mut Rng(4), Datum::Int(1), Datum::Int(1));
    store.add_row("trips", &row).unwrap();
    assert_eq!(
        store.ask(&by_label).unwrap().rows[0].values,
        [Value::Int(1)]
    );
    store
        .remove_row("combos", &[Datum::Int(1), Datum::Int(1), text("x")])
        .unwrap();
    assert_eq!(
        store.ask(&by_label).unwrap().rows[0].values,
        [Value::Int(0)]
    );
}

/// A measure's FILTER on a dim column reads the dim as it is now too.
#[test]
fn a_filter_on_an_attribute_reads_the_dim() {
    let spec = spec();
    let mut store = typed(&spec);
    store
        .add_rows("zones", &[Datum::Int(2), text("m"), text("a")])
        .unwrap();
    let mut row = trip(&mut Rng(8));
    row[AT] = Datum::Int(START);
    row[PU] = Datum::Int(2);
    row[VENDOR] = Datum::Int(2);
    row[FARE] = Datum::Int(100);
    store.add_row("trips", &row).unwrap();
    let mq_fare = Ask {
        view: id("trips"),
        measures: vec![id("mq_fare")],
        keys: Vec::new(),
        filter: Vec::new(),
        having: Vec::new(),
        from: Some(START),
        to: Some(END),
    };
    assert_eq!(first_value(&store, &mq_fare), Value::Int(100));
    store
        .remove_row("zones", &[Datum::Int(2), text("m"), text("a")])
        .unwrap();
    store
        .add_row("zones", &[Datum::Int(2), text("b"), text("a")])
        .unwrap();
    assert_eq!(first_value(&store, &mq_fare), Value::Null);
}

#[test]
fn dim_row_errors() {
    let mut store = typed(&spec());
    let row = [Datum::Int(1), text("m"), text("a")];
    let short = Err(RowError::Arity {
        expected: 3,
        found: 2,
    });
    assert_eq!(store.add_row("zones", &row[1..]), short);
    assert_eq!(store.add_rows("zones", &row[1..]), short);
    assert_eq!(store.set_types("zones", &[ColumnType::Int; 2]), short);
    store.add_row("zones", &row).unwrap();
    // The same row again is a second row under the key, as a join finds.
    store.add_row("zones", &row).unwrap();
    assert_eq!(store.dim_rows("zones"), Some(2));
    let other = [Datum::Int(1), text("q"), text("a")];
    assert_eq!(store.remove_row("zones", &other), Err(RowError::Missing));
    store.remove_row("zones", &row).unwrap();
    store.remove_row("zones", &row).unwrap();
    assert_eq!(store.remove_row("zones", &row), Err(RowError::Missing));
    // A batch is all or nothing.
    let bad = [row.to_vec(), vec![Datum::Int(2)]].concat();
    assert_eq!(
        store.add_rows("zones", &bad),
        Err(RowError::Arity {
            expected: 3,
            found: 1
        })
    );
    assert_eq!(store.dim_rows("zones"), Some(0));
    assert_eq!(store.dim_rows("trips"), None);
}

/// A dim row with a null in its key joins nothing, so it is never held:
/// its insert and its delete are nothing, and neither is an error. An
/// update that fills the key in adds the row; one that nulls it takes
/// the row out.
#[test]
fn a_dim_row_with_a_null_key_is_never_held() {
    let mut store = typed(&spec());
    let unkeyed = [Datum::Null, Datum::Int(2), text("z")];
    let keyed = [Datum::Int(1), Datum::Int(2), text("z")];
    store.add_row("combos", &unkeyed).unwrap();
    assert_eq!(store.dim_rows("combos"), Some(0));
    store.update_row("combos", &unkeyed, &keyed).unwrap();
    assert_eq!(store.dim_rows("combos"), Some(1));
    store.update_row("combos", &keyed, &unkeyed).unwrap();
    assert_eq!(store.dim_rows("combos"), Some(0));
    store.remove_row("combos", &unkeyed).unwrap();
    // A row with its key whole is missed when it is not held.
    assert_eq!(store.remove_row("combos", &keyed), Err(RowError::Missing));
}

/// Trips by pay, pickup and vendor, joined to the combos on both: every
/// via column is a by key, so asks read the dim through the projection,
/// not through bound cells.
fn combos_on_by_keys_spec() -> Spec {
    let combos = Dim {
        as_text: Vec::new(),
        name: id("combos"),
        table: table("combos"),
        key: vec![id("pu_id"), id("vendor_id")],
        attributes: vec![id("label")],
    };
    let trips = Fact {
        as_text: Vec::new(),
        name: id("trips"),
        table: table("trips"),
        time: Time {
            column: id("at"),
            width: Width { seconds: WIDTH },
            wider: Vec::new(),
            origin: Origin::Timestamptz,
        },
        refs: vec![Ref {
            alias: id("combo"),
            dim: id("combos"),
            via: vec![Via::Fact(id("pu")), Via::Fact(id("vendor"))],
            kind: RefKind::Left,
            reads: vec![attribute("combo_label", "label")],
        }],
        by_keys: vec![by_key("pay"), by_key("pu"), by_key("vendor")],
        filter: vec![],
        measures: vec![new_measure("trips", Aggregate::Count, vec![])],
    };
    Spec::new(vec![combos], vec![trips]).unwrap()
}

/// A dim key of two columns read through the projection matches on
/// both: two combos share a pickup and differ in vendor, and each trip
/// finds its own.
#[test]
fn a_key_of_two_columns_read_through_the_projection_matches_on_both() {
    let spec = combos_on_by_keys_spec();
    let mut store = Store::new(spec.clone());
    assert_eq!(store.bound_cells("trips"), None);
    store
        .set_types(
            "combos",
            &[ColumnType::Int, ColumnType::Int, ColumnType::Text],
        )
        .unwrap();
    for (vendor, label) in [(1, "a"), (2, "b")] {
        let combo = [Datum::Int(7), Datum::Int(vendor), text(label)];
        store.add_row("combos", &combo).unwrap();
    }
    let columns = spec.fact("trips").unwrap().columns().clone();
    for vendor in [1, 2, 2] {
        let mut row = Vec::new();
        for column in &columns {
            row.push(match column.as_str() {
                "at" => Datum::Int(START),
                "pu" => Datum::Int(7),
                "vendor" => Datum::Int(vendor),
                _ => Datum::Int(1),
            });
        }
        store.add_row("trips", &row).unwrap();
    }
    let ask = Ask {
        view: id("trips"),
        measures: vec![id("trips")],
        keys: vec![AskKey::Key(id("combo_label"))],
        filter: Vec::new(),
        having: Vec::new(),
        from: Some(START),
        to: Some(END),
    };
    let mut counted = Vec::new();
    for row in store.ask(&ask).unwrap().rows {
        counted.push((row.keys[0].clone(), row.values[0].clone()));
    }
    assert_eq!(
        counted,
        [(text("a"), Value::Int(1)), (text("b"), Value::Int(2))]
    );
}
