//! An ask allocates per group, never per cell: the allocations it makes are
//! the same over 1,000 cells as over 8,000.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use straum_domain::{
    Agg, Ask, AskKey, Datum, Dim, Fact, Grain, Ident, KeyPath, Measure, Ref, Spec, Store, Table,
    Time,
};

thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

/// Counts the allocations of the thread that makes them, so tests running
/// beside each other don't add to each other's counts.
struct Counting;

// SAFETY: hands every call to `System` unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.with(|n| n.set(n.get() + 1));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.with(|n| n.set(n.get() + 1));
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

const HOUR: u32 = 3600;
const ZONES: i64 = 40;
const BOROUGHS: [&str; 4] = ["m", "q", "b", "x"];

fn id(s: &str) -> Ident {
    Ident::new(s).unwrap()
}

fn key(s: &str) -> AskKey {
    let path = match s.split_once('.') {
        Some((head, attr)) => KeyPath {
            head: id(head),
            attr: Some(id(attr)),
        },
        None => KeyPath {
            head: id(s),
            attr: None,
        },
    };
    AskKey::Key(path)
}

fn spec() -> Spec {
    let table = |name| Table {
        schema: id("public"),
        name: id(name),
    };
    let m = |name, agg| Measure {
        name: id(name),
        agg,
        filter: vec![],
    };
    let zones = Dim {
        name: id("zones"),
        table: table("zones"),
        key: id("id"),
        attrs: vec![id("borough")],
    };
    let trips = Fact {
        name: id("trips"),
        table: table("trips"),
        key: id("id"),
        time: Time {
            column: id("at"),
            grain: Grain(HOUR),
        },
        refs: vec![Ref {
            name: id("pickup"),
            dim: id("zones"),
            via: id("pu"),
        }],
        by: vec![id("pay")],
        measures: vec![
            m("trips", Agg::Count),
            m("fare", Agg::Sum(id("fare"))),
            m("tip_max", Agg::Max(id("tip"))),
        ],
    };
    Spec::new(vec![zones], vec![trips]).unwrap()
}

/// A store whose rollup holds `cells` cells: one row per pay, zone and hour.
fn store(cells: i64) -> Store {
    let mut store = Store::new(spec());
    let dim = store.dim("zones").unwrap();
    for zone in 0..ZONES {
        let borough = BOROUGHS[zone as usize % BOROUGHS.len()];
        dim.upsert(&[Datum::Int(zone), Datum::Text(borough.into())])
            .unwrap();
    }
    let rollup = store.rollup("trips").unwrap();
    let columns = rollup.columns().to_vec();
    for n in 0..cells {
        let row: Vec<_> = columns
            .iter()
            .map(|c| match c.as_str() {
                "at" => Datum::Int(n / (2 * ZONES) * i64::from(HOUR)),
                "pay" => Datum::Int(n % 2),
                "pu" => Datum::Int(n / 2 % ZONES),
                "fare" => Datum::Int(n),
                "tip" => Datum::Int(n % 7),
                other => panic!("no value for {other}"),
            })
            .collect();
        rollup.add(&row).unwrap();
    }
    assert_eq!(store.cells("trips"), Some(cells as usize));
    store
}

/// The allocations one ask makes.
fn allocs(store: &Store, by: &[&str]) -> usize {
    let ask = Ask {
        fact: id("trips"),
        measures: vec![id("trips"), id("fare"), id("tip_max")],
        by: by.iter().map(|b| key(b)).collect(),
        filter: vec![],
        from: 0,
        to: 1000 * i64::from(HOUR),
    };
    let before = ALLOCS.with(Cell::get);
    let answer = store.ask(&ask).unwrap();
    let after = ALLOCS.with(Cell::get);
    drop(answer);
    after - before
}

#[test]
fn an_ask_allocates_per_group_not_per_cell() {
    let small = store(1_000);
    let big = store(8_000);
    for by in [&[][..], &["pay"], &["pickup"], &["pickup.borough"]] {
        assert_eq!(
            allocs(&small, by),
            allocs(&big, by),
            "by {by:?}: allocations grow with the cells"
        );
    }
}
