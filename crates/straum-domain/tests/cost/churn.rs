//! What a store keeps as rows come and go: a truncate, a dim change or
//! values never seen again leave what a store that never held them
//! holds, and an ask through a join remembers only the ids it read.
//! Each test reads the heap through the counting allocator of `main.rs`.

use super::{
    BOROUGHS, Datum, HOUR, PEAK, RefKind, Spec, Store, ZONES, Zones, ask, held, live, load_zones,
    spec, spec_of, trip_value,
};

/// A truncate gives back the codes of the values the view held, not only
/// its cells: the store holds what a new one does.
#[test]
fn a_cleared_view_holds_no_codes() {
    const PICKUPS: i64 = 20_000;
    let fresh = held(|| {
        let mut store = Store::new(spec_of(&[], Zones::Apart));
        store.add_row("trips", &trip_row(&store, 0, 0)).unwrap();
        store.clear("trips");
        store
    });
    let cleared = held(|| {
        let mut store = Store::new(spec_of(&[], Zones::Apart));
        for pickup in 0..PICKUPS {
            store
                .add_row("trips", &trip_row(&store, pickup, 0))
                .unwrap();
        }
        store.clear("trips");
        store
    });
    eprintln!("{fresh} bytes cleared after one row, {cleared} after {PICKUPS}");
    assert!(cleared - fresh < 4 << 10, "{cleared} against {fresh}");
}

/// A trip of `store`'s view with pickup zone `pickup` and the tip
/// offset `tip`.
fn trip_row(store: &Store, pickup: i64, tip: i64) -> Vec<Datum> {
    let columns = store.spec().fact("trips").unwrap().columns();
    let mut row = Vec::with_capacity(columns.len());
    for column in columns {
        let mut value = trip_value(column.as_str(), 0, tip);
        if column.as_str() == "pu" {
            value = Datum::Int(pickup);
        }
        row.push(value);
    }
    row
}

/// A store of `ids` trips, each in an hour of its own from a zone id of
/// its own: the zones name a few of the ids, as a dim of accounts names
/// the ones still open.
fn store_of_ids(ids: i64) -> Store {
    let mut store = Store::new(spec(&[]));
    load_zones(&mut store);
    for id in 0..ids {
        let row = [
            Datum::Int(id * i64::from(HOUR)),
            Datum::Int(0),
            Datum::Int(id),
            Datum::Int(id),
            Datum::Int(0),
        ];
        store.add_row("trips", &row).unwrap();
    }
    store
}

/// The heap an ask by borough over the first `hours` reaches at its most.
fn peak_by_borough(store: &Store, hours: i64) -> isize {
    let ask = ask(&["borough"], 0, hours);
    let before = live();
    PEAK.set(before);
    drop(store.ask(&ask).unwrap());
    PEAK.get() - before
}

/// An ask that reads a dim value through a join works it out once per
/// zone id it meets, in a memo of the ids it read, not of every id the
/// trips ever named: 100 times the ids cost an ask of a few cells nothing,
/// where a memo of every id would be 4 bytes an id.
#[test]
fn an_ask_of_a_dim_value_holds_what_its_cells_name() {
    let few = store_of_ids(1_000);
    let many = store_of_ids(100_000);
    let (few, many) = (peak_by_borough(&few, 10), peak_by_borough(&many, 10));
    eprintln!("10 hours by borough: {few} bytes, {many} with 100 times the ids");
    assert_eq!(few, many, "the memo grows with the ids");
}

/// When [`emptied`] takes the zones away: before the trips come, or
/// after, which drops the trips' bound cells.
#[derive(Clone, Copy)]
enum ZonesGo {
    Before,
    After,
}

/// The heap a store with bound cells over an inner join holds once
/// `trips` trips, an hour each, came and every zone went, as `zones_go`
/// says. With no zone, the join keeps no trip, so no bound cell is left.
fn emptied(trips: i64, zones_go: ZonesGo) -> isize {
    held(|| {
        let mut store = Store::new(inner(&spec_of(&[], Zones::Bound)));
        load_zones(&mut store);
        if let ZonesGo::Before = zones_go {
            remove_zones(&mut store);
        }
        for trip in 0..trips {
            let row = [
                Datum::Int(trip * i64::from(HOUR)),
                Datum::Int(0),
                Datum::Int(trip % ZONES),
                Datum::Int(1),
                Datum::Int(1),
            ];
            store.add_row("trips", &row).unwrap();
        }
        if let ZonesGo::After = zones_go {
            remove_zones(&mut store);
        }
        assert_eq!(store.bound_cells("trips"), Some(0));
        store
    })
}

/// `spec` with its one ref an inner join.
fn inner(spec: &Spec) -> Spec {
    let mut facts = spec.facts().to_vec();
    facts[0].refs[0].kind = RefKind::Inner;
    Spec::new(spec.dims().to_vec(), facts).unwrap()
}

/// Removes every zone [`load_zones`] added, one row at a time.
fn remove_zones(store: &mut Store) {
    for zone in 0..ZONES {
        let borough = BOROUGHS[usize::try_from(zone).unwrap() % BOROUGHS.len()];
        store
            .remove_row("zones", &[Datum::Int(zone), Datum::Text(borough.into())])
            .unwrap();
    }
}

/// A bound bucket whose last cell a dim change drops goes with it: a
/// thousand hours of trips whose zones all went leave what they leave
/// when the zones went first. A thousand empty buckets kept would be
/// hundreds of KiB.
#[test]
fn a_bound_bucket_a_dim_change_empties_goes() {
    const TRIPS: i64 = 1_000;
    let before = emptied(TRIPS, ZonesGo::Before);
    let after = emptied(TRIPS, ZonesGo::After);
    eprintln!("{before} bytes with the zones gone first, {after} after the trips");
    assert!(after - before < 4 << 10, "{after} bytes against {before}");
}

/// The heap a store of the trips without the zones holds after `rounds`
/// rounds of `trips` trips, each with a zone id never seen before, added
/// and all removed again.
fn churned(rounds: i64, trips: i64) -> isize {
    held(|| {
        let mut store = Store::new(spec_of(&[], Zones::Apart));
        for round in 0..rounds {
            let mut rows = Vec::with_capacity(usize::try_from(trips).unwrap());
            for trip in 0..trips {
                rows.push([
                    Datum::Int(0),
                    Datum::Int(0),
                    Datum::Int(round * trips + trip),
                    Datum::Int(1),
                    Datum::Int(1),
                ]);
            }
            for row in &rows {
                store.add_row("trips", row).unwrap();
            }
            for row in &rows {
                store.remove_row("trips", row).unwrap();
            }
        }
        assert_eq!(store.cells("trips"), Some(0));
        store
    })
}

/// Codes go once most of a column's are dead, so memory follows the
/// cells held, not every value seen: five rounds of 20,000 new zone ids,
/// all gone, hold what one round holds. Codes kept would be 20,000 more
/// a round, hundreds of KiB each.
#[test]
fn codes_no_cell_holds_go() {
    let one = churned(1, 20_000);
    let five = churned(5, 20_000);
    eprintln!("{one} bytes after one round, {five} after five");
    assert!(five - one < 64 << 10, "{five} bytes against {one}");
}

/// The heap a store holds after zone 1's borough was renamed `renames`
/// times, each to a name never seen before, and back.
fn renamed(renames: usize) -> isize {
    held(|| {
        let mut store = Store::new(spec_of(&[], Zones::Bound));
        load_zones(&mut store);
        let mut name = Datum::Text(BOROUGHS[1].into());
        for rename in 0..renames {
            let next = Datum::Text(format!("borough {rename}").into());
            store
                .update_row(
                    "zones",
                    &[Datum::Int(1), name],
                    &[Datum::Int(1), next.clone()],
                )
                .unwrap();
            name = next;
        }
        let back = Datum::Text(BOROUGHS[1].into());
        store
            .update_row("zones", &[Datum::Int(1), name], &[Datum::Int(1), back])
            .unwrap();
        store
    })
}

/// A dim row renamed over and over holds a bounded number of names, not
/// every name it had: a column keeps up to a few thousand dead codes
/// before it drops them, under 1 MiB here, where 100,000 names kept would
/// be many MiB.
#[test]
fn a_dim_row_renamed_holds_a_bounded_number_of_names() {
    let few = renamed(100);
    let many = renamed(100_000);
    eprintln!("{few} bytes after 100 renames, {many} after 100,000");
    assert!(many - few < 1 << 20, "{many} bytes against {few}");
}
