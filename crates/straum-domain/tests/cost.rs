//! What an ask costs: it allocates per group, never per cell, reads only
//! the cells of the buckets in its range, and stays within a time per cell.
//! And what sealing saves: the heap a sealed bucket's cells held.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::time::Instant;

use straum_domain::{
    Agg, Ask, AskKey, Attr, By, ColumnType, Datum, Dim, Fact, Grain, Ident, Measure, Origin, Ref,
    RefKind, Spec, Store, Table, Time, TimeKey, Via,
};

thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
    /// Bytes allocated and not yet freed by this thread.
    static LIVE: Cell<isize> = const { Cell::new(0) };
}

fn grow(bytes: usize, by: isize) {
    LIVE.with(|n| n.set(n.get() + by * bytes as isize));
}

/// Counts the allocations of the thread that makes them, so tests running
/// beside each other don't add to each other's counts. The one `unsafe`
/// in straum, settled in ADR-0027: `GlobalAlloc` is an unsafe trait, so no
/// safe allocator can count.
struct Counting;

// SAFETY: hands every call to `System` unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.with(|n| n.set(n.get() + 1));
        grow(layout.size(), 1);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        grow(layout.size(), -1);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.with(|n| n.set(n.get() + 1));
        grow(new_size, 1);
        grow(layout.size(), -1);
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
    AskKey::Key(id(s))
}

/// The fact, with `buckets` (in hours) beside its one-hour grain.
fn spec(buckets: &[u32]) -> Spec {
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
        key: vec![id("id")],
        attrs: vec![id("borough")],
    };
    let trips = Fact {
        name: id("trips"),
        table: table("trips"),
        time: Time {
            column: id("at"),
            grain: Grain(HOUR),
            buckets: buckets.iter().map(|h| Grain(h * HOUR)).collect(),
            origin: Origin::Timestamptz,
        },
        refs: vec![Ref {
            name: id("pickup"),
            dim: id("zones"),
            via: vec![Via::Fact(id("pu"))],
            kind: RefKind::Left,
            attrs: vec![Attr {
                name: id("borough"),
                column: id("borough"),
            }],
        }],
        by: vec![
            By::Column {
                column: id("pay"),
                name: id("pay"),
            },
            By::Column {
                column: id("pu"),
                name: id("pu"),
            },
        ],
        filter: vec![],
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
    store_with(cells, &[])
}

/// As [`store`], with `buckets` (in hours). The zones are loaded first,
/// each with its borough, as a load does.
fn store_with(cells: i64, buckets: &[u32]) -> Store {
    let spec = spec(buckets);
    let columns = spec.fact("trips").unwrap().columns().to_vec();
    let mut store = Store::new(spec);
    store
        .set_dim_types("zones", &[ColumnType::Int, ColumnType::Text])
        .unwrap();
    let zones: Vec<Datum> = (0..ZONES)
        .flat_map(|zone| {
            [
                Datum::Int(zone),
                Datum::Text(BOROUGHS[zone as usize % BOROUGHS.len()].into()),
            ]
        })
        .collect();
    store.add_dim_rows("zones", &zones).unwrap();
    for n in 0..cells {
        let zone = n / 2 % ZONES;
        let row: Vec<_> = columns
            .iter()
            .map(|c| match c.as_str() {
                "at" => Datum::Int(n / (2 * ZONES) * i64::from(HOUR)),
                "pay" => Datum::Int(n % 2),
                "pu" => Datum::Int(zone),
                "fare" => Datum::Int(n),
                "tip" => Datum::Int(n % 7),
                other => panic!("no value for {other}"),
            })
            .collect();
        store.add_fact_row("trips", &row).unwrap();
    }
    assert_eq!(store.cells("trips"), Some(cells as usize));
    store
}

/// Every measure, by `by`, over the hours `from..to`.
fn ask(by: &[&str], from: i64, to: i64) -> Ask {
    Ask {
        fact: id("trips"),
        measures: vec![id("trips"), id("fare"), id("tip_max")],
        by: by.iter().map(|b| key(b)).collect(),
        filter: vec![],
        from: from * i64::from(HOUR),
        to: to * i64::from(HOUR),
    }
}

fn allocs(store: &Store, by: &[&str]) -> usize {
    let ask = ask(by, 0, 1000);
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
    for by in [&[][..], &["pay"], &["pu"], &["borough"]] {
        assert_eq!(
            allocs(&small, by),
            allocs(&big, by),
            "by {by:?}: allocations grow with the cells"
        );
    }
}

#[test]
fn an_ask_reads_only_the_buckets_in_its_range() {
    let store = store(8_000);
    let mut unknown = ask(&[], 0, 1);
    unknown.fact = id("nope");
    assert!(store.cells_read(&unknown).is_err());
    let per_hour = 2 * ZONES as usize;
    for (from, to) in [(24, 48), (99, 100), (0, 100)] {
        assert_eq!(
            store.cells_read(&ask(&[], from, to)).unwrap(),
            (to - from) as usize * per_hour,
            "hours {from}..{to}: reads cells outside its range"
        );
    }
}

/// Every cell of a rollup the size of the taxi month at one hour, against a
/// time budget per cell. Timing depends on the machine and the build, so it
/// runs on request:
/// `cargo test -p straum-domain --release --test cost -- --ignored --nocapture`.
#[test]
#[ignore]
fn an_ask_stays_within_its_time_per_cell() {
    const CELLS: i64 = 173_600;
    const REPS: u32 = 20;
    let store = store(CELLS);
    let hours = CELLS / (2 * ZONES);
    for (by, budget) in [(&[][..], 4.0), (&["borough"], 10.0)] {
        let ask = ask(by, 0, hours);
        store.ask(&ask).unwrap();
        let clock = Instant::now();
        for _ in 0..REPS {
            black_box(store.ask(&ask).unwrap());
        }
        let per_cell = clock.elapsed().as_nanos() as f64 / f64::from(REPS) / CELLS as f64;
        eprintln!("by {by:?}: {per_cell:.1} ns a cell");
        assert!(
            per_cell <= budget,
            "by {by:?}: {per_cell:.1} ns a cell, over the {budget} ns budget"
        );
    }
}

/// Hours 5 to 700 over 30 days with day and week buckets: 19 hours, 6 days,
/// 3 weeks, 1 day and 4 hours, each piece 80 cells. Weeks start at the
/// epoch, so hour 168 starts one. An ask grouped by the bucket reads the
/// hours.
#[test]
fn wider_buckets_cut_the_cells_a_range_reads() {
    let per_hour = 2 * ZONES as usize;
    let store = store_with(720 * per_hour as i64, &[24, 168]);
    assert_eq!(
        store.cells_read(&ask(&[], 5, 700)).unwrap(),
        (19 + 6 + 3 + 1 + 4) * per_hour
    );
    let mut by_bucket = ask(&[], 5, 700);
    by_bucket.by = vec![AskKey::Time(TimeKey::Bucket)];
    assert_eq!(store.cells_read(&by_bucket).unwrap(), 695 * per_hour);
}

fn live() -> isize {
    LIVE.with(Cell::get)
}

/// Sealing 90% of the hours, with their copies gone to a spill, frees
/// about 80% of what the store held: the day buckets stay in memory.
#[test]
fn sealing_frees_the_cells_it_seals() {
    let empty = live();
    let mut store = store_with(200_000, &[24]);
    let held = live() - empty;
    let hours = 200_000 / (2 * ZONES);
    let before = hours * 9 / 10 * i64::from(HOUR);
    for id in store.sealable(before) {
        let spilled = store.sealed_copy(&id).unwrap();
        store.seal(&id).unwrap();
        drop(spilled);
    }
    let sealed = live() - empty;
    drop(store);
    let dropped = live() - empty;
    eprintln!("store held {held} bytes, {sealed} after sealing, {dropped} after drop");
    assert!(sealed * 100 < held * 20, "{sealed} of {held} bytes");
    assert_eq!(dropped, 0);
}
