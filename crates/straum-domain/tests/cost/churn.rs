//! What a store keeps as rows come and go: a truncate, a dim change or
//! values never seen again leave what a store that never held them
//! holds, a bucket thinned out gives back its room, and an ask through
//! a join or a filter remembers only the ids it read. A count of
//! distinct values gives back the room of the values that went. Each
//! test reads the heap through the counting allocator of `main.rs`.

use super::{
    Condition, Datum, HOUR, Literal, RefKind, Spec, Store, ZONES, Zones, allocations_of, ask,
    borough, held, id, load_zones, peak, spec, spec_measuring, spec_of, tip_distinct, trip_row,
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
    peak(|| drop(store.ask(&ask).unwrap()))
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

/// The heap an ask over the first `hours`, filtered on one pickup zone,
/// reaches at its most.
fn peak_of_one_pickup(store: &Store, hours: i64) -> isize {
    let mut ask = ask(&[], 0, hours);
    ask.filter = vec![Condition {
        key: id("pu"),
        values: vec![Literal::Int(1)],
    }];
    peak(|| drop(store.ask(&ask).unwrap()))
}

/// A condition is worked out once per code of its key the ask meets, in
/// a memo of the codes it read, not of every code the key ever had: 100
/// times the ids cost an ask of a few cells nothing, where a memo of
/// every id would be a byte an id.
#[test]
fn a_filtered_ask_holds_what_its_cells_name() {
    let few = store_of_ids(1_000);
    let many = store_of_ids(100_000);
    let (few, many) = (peak_of_one_pickup(&few, 10), peak_of_one_pickup(&many, 10));
    eprintln!("10 hours of one pickup: {few} bytes, {many} with 100 times the ids");
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
        store
            .remove_row("zones", &[Datum::Int(zone), borough(zone)])
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
        let mut name = borough(1);
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
        let back = borough(1);
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

/// Where [`thinned`] puts its trips: all in the first hour, so in one
/// bucket at each width, or each in an hour of its own.
#[derive(Clone, Copy)]
enum Hours {
    One,
    Each,
}

/// The trip of `store`'s view from zone `pickup`, in the hour `hours`
/// gives it.
fn pickup_row(store: &Store, pickup: i64, hours: Hours) -> Vec<Datum> {
    let mut row = trip_row(store, pickup, 0);
    let Hours::Each = hours else {
        return row;
    };
    let columns = store.spec().fact("trips").unwrap().columns();
    for (value, column) in row.iter_mut().zip(columns) {
        if column.as_str() == "at" {
            *value = Datum::Int(pickup * i64::from(HOUR));
        }
    }
    row
}

/// A store of the trips with day buckets that got a trip from each of
/// `pickups` pickup zones, in the hours `hours` says, and then lost all
/// but `kept`.
fn thinned(pickups: i64, kept: i64, hours: Hours) -> Store {
    let mut store = Store::new(spec_of(&[24], Zones::Apart));
    for pickup in 0..pickups {
        let row = pickup_row(&store, pickup, hours);
        store.add_row("trips", &row).unwrap();
    }
    for pickup in kept..pickups {
        let row = pickup_row(&store, pickup, hours);
        store.remove_row("trips", &row).unwrap();
    }
    assert_eq!(store.cells("trips"), Some(usize::try_from(kept).unwrap()));
    store
}

/// A bucket gives back its room as its cells go, at the hour and the
/// day: 20,000 cells in one bucket, down to one, hold what 20,000 cells
/// in buckets of their own, all gone but one, hold; the zone ids' codes
/// are alike in both. Kept, the index and columns of 20,000 cells would
/// be MiB.
#[test]
fn a_bucket_thinned_to_one_cell_holds_what_one_cell_holds() {
    const PICKUPS: i64 = 20_000;
    let spread = held(|| thinned(PICKUPS, 1, Hours::Each));
    let thinned = held(|| thinned(PICKUPS, 1, Hours::One));
    eprintln!("{spread} bytes over {PICKUPS} hours, {thinned} in one, thinned to a cell");
    assert!(
        thinned - spread < 4 << 10,
        "{thinned} bytes against {spread}"
    );
}

/// The allocations of a thousand rounds of the last of `kept` cells,
/// left of 20,000 in one bucket, going and coming back.
fn come_and_go(kept: i64) -> usize {
    const ROUNDS: usize = 1_000;
    let mut store = thinned(20_000, kept, Hours::One);
    let row = pickup_row(&store, kept - 1, Hours::One);
    allocations_of(|| {
        for _ in 0..ROUNDS {
            store.remove_row("trips", &row).unwrap();
            store.add_row("trips", &row).unwrap();
        }
    })
}

/// A bucket gives back its room only once most of it lies empty, so a
/// cell that comes and goes at that edge doesn't make it shrink and grow
/// each time: a thousand rounds there allocate about what they do away
/// from it, not a new index and columns a round.
#[test]
fn a_cell_coming_and_going_at_the_edge_moves_no_room() {
    // 20,000 cells take 32,768 index slots, which shrink at an eighth.
    const EDGE: i64 = 32_768 / 8;
    let away = come_and_go(EDGE + 1_000);
    let edge = come_and_go(EDGE + 1);
    eprintln!("{edge} allocations at the edge, {away} away from it");
    assert!(edge < away + 64, "{edge} allocations against {away}");
}

/// A store of the trips with a count of distinct tips and no zones.
fn distinct_tips() -> Store {
    Store::new(spec_measuring(&[], Zones::Apart, vec![tip_distinct()]))
}

/// A trip of `store`'s view in the first cell, with the tip `tip`.
fn tip_row(store: &Store, tip: &Datum) -> Vec<Datum> {
    let mut row = trip_row(store, 0, 0);
    let columns = store.spec().fact("trips").unwrap().columns();
    for (value, column) in row.iter_mut().zip(columns) {
        if column.as_str() == "tip" {
            *value = tip.clone();
        }
    }
    row
}

/// The trips of the first cell, each with a text tip of its own.
fn text_tips(store: &Store, tips: i64) -> Vec<Vec<Datum>> {
    let mut rows = Vec::new();
    for tip in 0..tips {
        rows.push(tip_row(store, &Datum::Text(format!("tip {tip}").into())));
    }
    rows
}

/// The heap a store holds after `tips` distinct text tips came into one
/// cell and all but the first went.
fn kept_one_of(tips: i64) -> isize {
    held(|| {
        let mut store = distinct_tips();
        let rows = text_tips(&store, tips);
        for row in &rows {
            store.add_row("trips", row).unwrap();
        }
        for row in &rows[1..] {
            store.remove_row("trips", row).unwrap();
        }
        store
    })
}

/// A count of distinct values gives back the room of the values that
/// went: 100,000 tips, all gone but one, hold about what one tip holds.
/// Kept, the room of 100,000 values would be MiB.
#[test]
fn a_distinct_map_gives_back_the_room_of_its_values() {
    let one = kept_one_of(1);
    let many = kept_one_of(100_000);
    eprintln!("{one} bytes after one tip, {many} after 100,000, all but one gone");
    assert!(many - one < 8 << 10, "{many} bytes against {one}");
}

/// A count of distinct values that loses a fifth of its values keeps
/// its room. Each tip is held twice. The removes of the first copies
/// change only the counts, and the removes of the second copies take
/// the values out with no allocation more. The 56,000 values left fit
/// the room of a smaller map, which a map that shrinks too soon takes.
#[test]
fn a_distinct_map_keeps_its_room_when_a_fifth_goes() {
    let mut store = distinct_tips();
    let rows = text_tips(&store, 70_000);
    for row in rows.iter().chain(&rows) {
        store.add_row("trips", row).unwrap();
    }
    let mut remove_a_fifth = || {
        allocations_of(|| {
            for row in &rows[..14_000] {
                store.remove_row("trips", row).unwrap();
            }
        })
    };
    let counted = remove_a_fifth();
    let gone = remove_a_fifth();
    eprintln!("{counted} allocations to count down, {gone} to take a fifth out");
    assert_eq!(gone, counted, "a fifth of the values gone");
}

/// The allocations of ten rounds of the tips over 1,000 going and coming
/// back, in a store that holds 2,000 number tips `copies` times after
/// one text tip. The text tip keeps a map of values, and a number makes
/// no allocation of its own.
fn fall_and_rise(copies: usize) -> usize {
    let mut store = distinct_tips();
    let mut rows = Vec::new();
    for tip in 0..2_000 {
        rows.push(tip_row(&store, &Datum::Int(tip)));
    }
    let first = tip_row(&store, &Datum::Text("first".into()));
    store.add_row("trips", &first).unwrap();
    for _ in 0..copies {
        for row in &rows {
            store.add_row("trips", row).unwrap();
        }
    }
    allocations_of(|| {
        for _ in 0..10 {
            for row in &rows[1_000..] {
                store.remove_row("trips", row).unwrap();
            }
            for row in &rows[1_000..] {
                store.add_row("trips", row).unwrap();
            }
        }
    })
}

/// A count of distinct values that moves between 1,000 and 2,000 values
/// keeps the room of 2,000: its rounds allocate what rounds that change
/// only the counts allocate.
#[test]
fn a_distinct_map_moving_between_sizes_moves_no_room() {
    let counted = fall_and_rise(2);
    let moved = fall_and_rise(1);
    eprintln!("{moved} allocations over ten rounds, {counted} to count only");
    assert_eq!(moved, counted, "ten falls and rises");
}
