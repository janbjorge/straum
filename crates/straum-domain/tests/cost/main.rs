//! What an ask costs: it allocates per group, never per cell, reads only
//! the cells of the buckets in its range, and stays within a time per cell.
//! What a store holds: one set of cells for a join on a key, the bytes a
//! cell's numbers need and no more, a few bytes a value for a min, max
//! or count of distinct numbers. A percentile holds nothing beyond its
//! column's min or max, and its ask holds the values of its range,
//! however many cells lie outside it. `churn.rs` holds what a store
//! keeps as rows come and go.
//!
//! A counting allocator stands in for the system one: each thread tallies
//! its own allocations and live bytes, and a test reads the tally before
//! and after the work it measures.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::hint::black_box;
use std::time::Instant;

use straum_domain::{
    Aggregate, Ask, AskKey, Attribute, ByKey, ColumnType, Condition, Datum, Dim, Fact, Literal,
    Measure, Ref, RefKind, Spec, Start, Store, Time, Via, Width, Window,
};

#[path = "../common/mod.rs"]
mod common;

mod churn;

use common::{id, measure, percentile, table};

thread_local! {
    /// Allocations this thread has made.
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
    /// Bytes allocated and not yet freed by this thread.
    static LIVE: Cell<isize> = const { Cell::new(0) };
    /// The most `LIVE` has been since last set.
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

#[derive(Clone, Copy)]
enum MemoryChange {
    Allocated,
    Freed,
}

#[expect(
    clippy::cast_possible_wrap,
    reason = "a layout's size is at most isize::MAX"
)]
fn track(bytes: usize, change: MemoryChange) {
    let bytes = bytes as isize;
    let by = match change {
        MemoryChange::Allocated => bytes,
        MemoryChange::Freed => -bytes,
    };
    let live = LIVE.get() + by;
    LIVE.set(live);
    PEAK.set(PEAK.get().max(live));
}

fn count_allocation() {
    ALLOCATIONS.set(ALLOCATIONS.get() + 1);
}

/// Counts the allocations of the thread that makes them, so tests running
/// beside each other don't add to each other's counts. One of the two
/// `unsafe` blocks in Straum, beside the WAL tests' allocator in
/// `straum-pg`, settled in ADR-0013: `GlobalAlloc` is an unsafe trait, so
/// no safe allocator can count.
struct Counting;

// SAFETY: hands every call to `System` unchanged.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        track(layout.size(), MemoryChange::Allocated);
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        track(layout.size(), MemoryChange::Freed);
        unsafe { System.dealloc(pointer, layout) }
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count_allocation();
        track(new_size, MemoryChange::Allocated);
        track(layout.size(), MemoryChange::Freed);
        unsafe { System.realloc(pointer, layout, new_size) }
    }
}

#[global_allocator]
static COUNTING: Counting = Counting;

const HOUR: u32 = 3600;
const ZONES: i64 = 40;
const BOROUGHS: [&str; 4] = ["m", "q", "b", "x"];

fn key(name: &str) -> AskKey {
    AskKey::Key(id(name))
}

/// The fact, with `buckets` (in hours) beside its one-hour width, joined
/// to the zones on a `by` key.
fn spec(buckets: &[u32]) -> Spec {
    spec_of(buckets, Zones::Joined)
}

/// Whether the fact joins the zones, and through what.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Zones {
    /// On a `by` key, so asks read the zones as they are.
    Joined,
    /// On a column no `by` key names, so the view keeps bound cells.
    Bound,
    Apart,
}

/// As [`spec`]; with the zones apart, without them.
fn spec_of(buckets: &[u32], zones: Zones) -> Spec {
    let measures = vec![
        measure("trips", Aggregate::Count),
        measure("fare", Aggregate::Sum(id("fare"))),
        measure("tip_max", Aggregate::Max(id("tip"))),
    ];
    spec_measuring(buckets, zones, measures)
}

/// As [`spec_of`], with the `measures` given.
fn spec_measuring(buckets: &[u32], zones: Zones, measures: Vec<Measure>) -> Spec {
    let mut wider = Vec::with_capacity(buckets.len());
    for hours in buckets {
        wider.push(Width {
            seconds: hours * HOUR,
        });
    }
    let mut dims = Vec::new();
    let mut refs = Vec::new();
    if zones != Zones::Apart {
        dims.push(Dim {
            as_text: Vec::new(),
            name: id("zones"),
            table: table("zones"),
            key: vec![id("id")],
            attributes: vec![id("borough")],
        });
        refs.push(Ref {
            alias: id("pickup"),
            dim: id("zones"),
            via: vec![Via::Fact(id("pu"))],
            kind: RefKind::Left,
            reads: vec![Attribute {
                name: id("borough"),
                column: id("borough"),
            }],
        });
    }
    let mut by_keys = vec![ByKey::Column {
        column: id("pay"),
        name: id("pay"),
    }];
    if zones != Zones::Bound {
        by_keys.push(ByKey::Column {
            column: id("pu"),
            name: id("pu"),
        });
    }
    let trips = Fact {
        as_text: Vec::new(),
        name: id("trips"),
        table: table("trips"),
        time: Time {
            column: id("at"),
            width: Width { seconds: HOUR },
            wider,
            name: id("bucket"),
        },
        refs,
        by_keys,
        filter: vec![],
        measures,
    };
    Spec::new(dims, vec![trips]).unwrap()
}

/// A store whose rollup holds `cells` cells: one row per pay, zone and hour.
fn store(cells: i64) -> Store {
    store_with(cells, &[])
}

/// As [`store`], with `buckets` (in hours). The zones are loaded first,
/// each with its borough, as a load does.
fn store_with(cells: i64, buckets: &[u32]) -> Store {
    store_of(spec(buckets), cells, 1)
}

/// A store of `spec` whose rollup holds `cells` cells, each of `tips`
/// rows that differ in their tip.
fn store_of(spec: Spec, cells: i64, tips: i64) -> Store {
    let columns = spec.fact("trips").unwrap().columns().clone();
    let joined = !spec.dims().is_empty();
    let mut store = Store::new(spec);
    if joined {
        load_zones(&mut store);
    }
    for cell in 0..cells {
        for tip in 0..tips {
            let mut row = Vec::with_capacity(columns.len());
            for column in &columns {
                row.push(trip_value(column.as_str(), cell, tip));
            }
            store.add_row("trips", &row).unwrap();
        }
    }
    assert_eq!(store.cells("trips"), Some(usize::try_from(cells).unwrap()));
    store
}

/// Every zone, each in a borough, laid end to end as a load reads them.
fn load_zones(store: &mut Store) {
    store
        .set_types("zones", &[ColumnType::Int, ColumnType::Text])
        .unwrap();
    let mut zones = Vec::new();
    for zone in 0..ZONES {
        let borough = BOROUGHS[usize::try_from(zone).unwrap() % BOROUGHS.len()];
        zones.push(Datum::Int(zone));
        zones.push(Datum::Text(borough.into()));
    }
    store.add_rows("zones", &zones).unwrap();
}

/// The value of `column` in the trip of cell number `cell` with tip
/// offset `tip`: one cell per pay, zone and hour.
fn trip_value(column: &str, cell: i64, tip: i64) -> Datum {
    match column {
        "at" => Datum::Int(cell / (2 * ZONES) * i64::from(HOUR)),
        "pay" => Datum::Int(cell % 2),
        "pu" => Datum::Int(cell / 2 % ZONES),
        "fare" => Datum::Int(cell),
        "tip" => Datum::Int(cell % 7 + tip),
        other => panic!("no value for {other}"),
    }
}

/// Every measure, by `by`, over the hours `from..to`.
fn ask(by: &[&str], from: i64, to: i64) -> Ask {
    let mut keys = Vec::with_capacity(by.len());
    for name in by {
        keys.push(key(name));
    }
    Ask {
        view: id("trips"),
        measures: vec![id("trips"), id("fare"), id("tip_max")],
        keys,
        filter: vec![],
        having: Vec::new(),
        from: Some(from * i64::from(HOUR)),
        to: Some(to * i64::from(HOUR)),
        bounded: None,
    }
}

/// The allocations an ask of every measure by `by` makes.
fn allocations(store: &Store, by: &[&str]) -> usize {
    let ask = ask(by, 0, 1000);
    let before = ALLOCATIONS.get();
    let answer = store.ask(&ask).unwrap();
    let after = ALLOCATIONS.get();
    drop(answer);
    after - before
}

/// The spec has no percentile: it gathers the values of its range, which
/// grow with the cells. Its own tests are below.
#[test]
fn an_ask_allocates_per_group_not_per_cell() {
    let small = store(1_000);
    let big = store(8_000);
    for by in [&[][..], &["pay"], &["pu"], &["borough"]] {
        assert_eq!(
            allocations(&small, by),
            allocations(&big, by),
            "by {by:?}: allocations grow with the cells"
        );
    }
}

#[test]
fn an_ask_reads_only_the_buckets_in_its_range() {
    let store = store(8_000);
    let mut unknown = ask(&[], 0, 1);
    unknown.view = id("nope");
    assert!(store.cells_read(&unknown).is_err());
    let per_hour = 2 * usize::try_from(ZONES).unwrap();
    for (from, to) in [(24, 48), (99, 100), (0, 100)] {
        assert_eq!(
            store.cells_read(&ask(&[], from, to)).unwrap(),
            usize::try_from(to - from).unwrap() * per_hour,
            "hours {from}..{to}: reads cells outside its range"
        );
    }
}

/// Every cell of a rollup the size of the taxi month at one hour, against a
/// time budget per cell. Timing depends on the machine and the build, so it
/// runs on request:
/// `cargo test -p straum-domain --release --test cost -- --ignored --nocapture`.
#[test]
#[ignore = "timing depends on the machine and the build"]
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
        #[expect(
            clippy::cast_precision_loss,
            reason = "a time per cell needs no more digits than a float holds"
        )]
        let per_cell = clock.elapsed().as_nanos() as f64 / f64::from(REPS) / CELLS as f64;
        eprintln!("by {by:?}: {per_cell:.1} ns a cell");
        assert!(
            per_cell <= budget,
            "by {by:?}: {per_cell:.1} ns a cell, over the {budget} ns budget"
        );
    }
}

/// A zone row change against a time budget, on a rollup the size of the
/// taxi month at one hour whose bound cells read the zones: a zone no trip
/// names moves no cell, so its time is per change, not per cell; a zone
/// trips name walks the cells once an update. Runs on request, as
/// [`an_ask_stays_within_its_time_per_cell`] does.
#[test]
#[ignore = "timing depends on the machine and the build"]
fn a_zone_change_stays_within_its_time() {
    const CELLS: i64 = 173_600;
    const REPS: u32 = 20;
    let mut store = store_of(spec_of(&[], Zones::Bound), CELLS, 1);
    let unnamed = [Datum::Int(ZONES + 1), Datum::Text("Bronx".into())];
    let clock = Instant::now();
    for _ in 0..REPS {
        store.add_row("zones", &unnamed).unwrap();
        store.remove_row("zones", &unnamed).unwrap();
    }
    let per_change = clock.elapsed() / (2 * REPS);
    eprintln!("a zone no trip names: {per_change:?} a change");
    let named = [Datum::Int(1), Datum::Text(BOROUGHS[1].into())];
    let moved = [Datum::Int(1), Datum::Text("Bronx".into())];
    let clock = Instant::now();
    for _ in 0..REPS {
        store.update_row("zones", &named, &moved).unwrap();
        store.update_row("zones", &moved, &named).unwrap();
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "a time per cell needs no more digits than a float holds"
    )]
    let per_cell = clock.elapsed().as_nanos() as f64 / f64::from(2 * REPS) / CELLS as f64;
    eprintln!("a zone trips name: {per_cell:.1} ns a cell an update");
    assert!(
        per_change.as_micros() <= 50,
        "a zone no trip names: {per_change:?} a change"
    );
    assert!(
        per_cell <= 30.0,
        "a zone trips name: {per_cell:.1} ns a cell an update"
    );
}

/// Hours 5 to 700 over 30 days with day and week buckets: 19 hours, 6 days,
/// 3 weeks, 1 day and 4 hours, each piece 80 cells. Weeks start at the
/// epoch, so hour 168 starts one. An ask grouped by the bucket reads the
/// hours.
#[test]
fn wider_buckets_cut_the_cells_a_range_reads() {
    let per_hour = 2 * ZONES;
    let store = store_with(720 * per_hour, &[24, 168]);
    assert_eq!(
        store.cells_read(&ask(&[], 5, 700)).unwrap(),
        usize::try_from((19 + 6 + 3 + 1 + 4) * per_hour).unwrap()
    );
    let mut by_bucket = ask(&[], 5, 700);
    by_bucket.keys = vec![AskKey::Key(id("bucket"))];
    assert_eq!(
        store.cells_read(&by_bucket).unwrap(),
        usize::try_from(695 * per_hour).unwrap()
    );
}

/// Rows a day or a week apart read one stored bucket a row, so 24 or 168
/// times fewer cells than the hours under them: every hour here holds the
/// same keys, which a real day rarely does.
#[test]
fn rows_at_a_wider_width_read_its_buckets() {
    let per_hour = 2 * ZONES;
    let store = store_with(720 * per_hour, &[24, 168]);
    let mut by_day = ask(&[], 24, 696);
    by_day.keys = vec![AskKey::WiderBucket {
        name: id("bucket"),
        width: Width { seconds: 24 * HOUR },
    }];
    assert_eq!(
        store.cells_read(&by_day).unwrap(),
        usize::try_from(28 * per_hour).unwrap()
    );
    let mut by_week = ask(&[], 168, 672);
    by_week.keys = vec![AskKey::WiderBucket {
        name: id("bucket"),
        width: Width {
            seconds: 168 * HOUR,
        },
    }];
    assert_eq!(
        store.cells_read(&by_week).unwrap(),
        usize::try_from(3 * per_hour).unwrap()
    );
    // Open ends stand on the hours held, here whole days.
    by_day.from = None;
    by_day.to = None;
    assert_eq!(
        store.cells_read(&by_day).unwrap(),
        usize::try_from(30 * per_hour).unwrap()
    );
}

fn live() -> isize {
    LIVE.get()
}

/// The heap the store `build` makes holds.
// A function to call, not a store: the heap is read before and after the
// store is built, and each test builds its own.
fn held(build: impl FnOnce() -> Store) -> isize {
    let empty = live();
    let store = build();
    let held = live() - empty;
    drop(store);
    assert_eq!(live(), empty);
    held
}

/// A join on a `by` key adds no cells, which bound cells would be as many
/// as: the view holds what it would without the join, and the dim's rows.
/// Each cell holds a max over several tips, at the hour and the day, so
/// a second set of cells would hold every value twice too.
#[test]
fn a_join_on_a_by_key_keeps_one_set_of_cells() {
    let plain = held(|| store_of(spec_of(&[24], Zones::Apart), 20_000, 4));
    let joined = held(|| store_of(spec_of(&[24], Zones::Joined), 20_000, 4));
    eprintln!("{plain} bytes without the join, {joined} with it");
    assert!(joined - plain < 16 << 10, "{joined} bytes against {plain}");
}

/// A cell costs the bytes its numbers need and a slot to find it by: here
/// two keys of four bytes, a row count of one, a sum of four, a min and a
/// max of eight each with their values' 32, and four for the slot, with an
/// eighth more to grow into. A count and a sum at their widest, a count
/// beside every sum and a map from keys made it twice that.
#[test]
fn a_cell_costs_the_bytes_its_numbers_need() {
    const CELLS: i64 = 80_000;
    let a_cell = held(|| store_of(spec(&[]), CELLS, 1)) / isize::try_from(CELLS).unwrap();
    eprintln!("{a_cell} bytes a cell");
    assert!(a_cell <= 80, "{a_cell} bytes a cell");
}

/// A min or max keeps a cell's values in the cell itself while they are
/// few, and more of them in a run of as many bytes as they need: one or
/// two each for tips this close, and an eighth of the run to grow into. A
/// run left to double would be half as long again, a pair of integers a
/// value is 16 bytes, and a tree's nodes twice that or more.
#[test]
fn a_min_or_max_costs_a_cell_a_run_of_its_values() {
    const CELLS: i64 = 2_000;
    let one = held(|| store_of(spec(&[]), CELLS, 1));
    for (tips, most) in [(4, 0), (40, 180), (520, 260)] {
        let several = held(|| store_of(spec(&[]), CELLS, tips));
        let a_value = (several - one) * 100 / isize::try_from(CELLS * tips).unwrap();
        eprintln!("{tips} tips a cell: {a_value} hundredths of a byte a value");
        assert!(
            a_value <= most,
            "{tips} tips a cell: {a_value} hundredths of a byte a value"
        );
    }
}

fn tip_max() -> Measure {
    measure("tip_max", Aggregate::Max(id("tip")))
}

fn tip_median() -> Measure {
    measure("tip_median", percentile(5, 1, "tip"))
}

/// The heap a store of the `measures` holds, 2,000 cells of 40 tips each.
/// The measures are made inside, so the store frees all it took.
fn held_by(measures: fn() -> Vec<Measure>) -> isize {
    const CELLS: i64 = 2_000;
    held(|| store_of(spec_measuring(&[], Zones::Apart, measures()), CELLS, 40))
}

/// A percentile reads the values a min or max on its column keeps: beside
/// one, or in its place, it holds no byte more than a max but its name in
/// the spec. A second set of values would be over 100 KiB here.
#[test]
fn a_percentile_holds_what_a_max_holds() {
    let alone = held_by(|| vec![tip_max()]);
    let beside = held_by(|| vec![tip_max(), tip_median()]);
    let instead = held_by(|| vec![tip_median()]);
    eprintln!("max {alone} bytes, max and median {beside}, median {instead}");
    assert!(beside - alone < 256, "{beside} bytes against {alone}");
    assert!(
        instead.abs_diff(alone) < 256,
        "{instead} bytes against {alone}"
    );
}

/// The heap an ask of the median by `by` over hours `from..to` reaches
/// at its most, the answer dropped.
fn peak(store: &Store, by: &[&str], from: i64, to: i64) -> isize {
    let mut ask = ask(by, from, to);
    ask.measures = vec![id("tip_median")];
    let before = live();
    PEAK.set(before);
    drop(store.ask(&ask).unwrap());
    PEAK.get() - before
}

/// The fact joined to the zones, measuring the median tip alone.
fn median_spec() -> Spec {
    spec_measuring(&[], Zones::Joined, vec![tip_median()])
}

/// An ask of a percentile gathers each value of its range once a cell,
/// a pair of 16 bytes, in a list that may have doubled past them: here 80
/// cells an hour, 4 tips each. Cells outside the range add nothing.
#[test]
fn a_percentile_ask_holds_the_values_of_its_range() {
    const HOURS: i64 = 100;
    const TIPS: i64 = 4;
    let per_hour = 2 * ZONES;
    let small = store_of(median_spec(), HOURS * per_hour, TIPS);
    let big = store_of(median_spec(), 8 * HOURS * per_hour, TIPS);
    for (by, from, to) in [(&[][..], 24, 48), (&["pay"], 0, 100), (&["pu"], 99, 100)] {
        let values = (to - from) * per_hour * TIPS;
        let most = isize::try_from(2 * 16 * values + (4 << 10)).unwrap();
        let (small, big) = (peak(&small, by, from, to), peak(&big, by, from, to));
        eprintln!("by {by:?}, hours {from}..{to}: {small} bytes, {big} with 8x the cells");
        assert!(small <= most, "{small} bytes for {values} values");
        assert_eq!(small, big, "by {by:?}: cells outside the range cost");
    }
}

/// The allocations an ask of the median by `by` makes, the answer dropped.
fn median_allocations(store: &Store, by: &[&str]) -> usize {
    let mut ask = ask(by, 0, 1000);
    ask.measures = vec![id("tip_median")];
    let before = ALLOCATIONS.get();
    drop(store.ask(&ask).unwrap());
    ALLOCATIONS.get() - before
}

/// An ask of a percentile allocates once more each time a group's list
/// of values doubles, not once a value: 8 times the values add about 3
/// allocations a group.
#[test]
fn a_percentile_ask_allocates_per_doubling_of_its_values() {
    const CELLS: i64 = 4_000;
    let few = store_of(median_spec(), CELLS, 2);
    let many = store_of(median_spec(), CELLS, 16);
    for (by, groups) in [
        (&[][..], 1),
        (&["pay"], 2),
        (&["borough"], 4),
        (&["pu"], 40),
    ] {
        let (few, many) = (median_allocations(&few, by), median_allocations(&many, by));
        eprintln!("by {by:?}: {few} allocations, {many} with 8x the values");
        assert!(many - few <= 3 * groups, "by {by:?}: {few} then {many}");
    }
}

/// The most bytes an ask of the trips held at once, its filter
/// `pu = 1` repeated `repeats` times.
fn repeated_filter_peak(store: &Store, repeats: usize) -> isize {
    let mut ask = ask(&[], 0, 1000);
    let pickup = Condition {
        key: id("pu"),
        values: vec![Literal::Int(1)],
    };
    ask.filter = vec![pickup; repeats];
    let before = live();
    PEAK.set(before);
    drop(store.ask(&ask).unwrap());
    PEAK.get() - before
}

/// Conditions on one key fold into one check, so repeating one a thousand
/// times keeps no memo of the key per repeat.
#[test]
fn a_repeated_condition_costs_no_memo_of_its_own() {
    let store = store(8_000);
    let once = repeated_filter_peak(&store, 1);
    let often = repeated_filter_peak(&store, 1_000);
    assert!(
        often - once < 4_096,
        "once {once} B, a thousand times {often} B"
    );
}

/// The most bytes an ask of the trips filtered on `borough = value` held
/// at once, answered or refused.
fn borough_filter_peak(store: &Store, value: Literal) -> isize {
    let mut ask = ask(&[], 0, 1000);
    ask.filter = vec![Condition {
        key: id("borough"),
        values: vec![value],
    }];
    let before = live();
    PEAK.set(before);
    drop(store.ask(&ask));
    PEAK.get() - before
}

/// Checking a condition's type reads the dim's column type, not a join
/// with a memo per pickup zone ever seen: an ask of the wrong type is
/// refused at the cost of its error. Of the right type, the ask keeps
/// that memo to answer.
#[test]
fn checking_an_ask_s_types_keeps_no_memo_of_its_joins() {
    const PICKUPS: i64 = 50_000;
    let mut store = store(1_000);
    let columns = store.spec().fact("trips").unwrap().columns().clone();
    for pickup in 0..PICKUPS {
        let mut row = Vec::with_capacity(columns.len());
        for column in &columns {
            let mut value = trip_value(column.as_str(), 0, 0);
            if column.as_str() == "pu" {
                value = Datum::Int(ZONES + pickup);
            }
            row.push(value);
        }
        store.add_row("trips", &row).unwrap();
    }
    let memo = isize::try_from(PICKUPS).unwrap() * 4;
    let refused = borough_filter_peak(&store, Literal::Int(1));
    assert!(refused < 4_096, "refused, {refused} B");
    let answered = borough_filter_peak(&store, Literal::Text("m".into()));
    assert!(answered >= memo, "answered, {answered} B");
}

/// The most bytes an ask of the trips by every key held at once, built
/// whole or only its first row.
fn by_every_key_peak(store: &Store, window: Option<Window>) -> isize {
    let ask = ask(&["pay", "pu"], 0, 1000);
    let mut by_bucket = ask.clone();
    by_bucket.keys.insert(0, AskKey::Key(id("bucket")));
    let before = live();
    PEAK.set(before);
    match window {
        Some(window) => drop(store.ask_window(&by_bucket, Some(window)).unwrap()),
        None => drop(store.ask(&by_bucket).unwrap()),
    }
    PEAK.get() - before
}

/// A window of one row builds that row, not every row of the answer: the
/// groups and their measures cost the same, the rows built only the
/// whole answer's.
#[test]
fn a_window_builds_its_rows_alone() {
    const CELLS: i64 = 20_000;
    let store = store(CELLS);
    let first_row = Window {
        start: Start::FromFirst(0),
        count: 1,
    };
    let windowed = by_every_key_peak(&store, Some(first_row));
    let whole = by_every_key_peak(&store, None);
    eprintln!("{windowed} bytes for one row, {whole} for {CELLS}");
    // Each row holds three keys and three measures, at least 100 bytes.
    let rows_bytes = isize::try_from(CELLS).unwrap() * 100;
    assert!(whole - windowed > rows_bytes, "{windowed} against {whole}");
}

fn tip_distinct() -> Measure {
    measure("tip_distinct", Aggregate::CountDistinct(id("tip")))
}

/// The heap an ask of `measure` by the bucket and `by` reaches at its
/// most, the answer dropped.
fn by_bucket_peak(store: &Store, by: &[&str], measure: &str) -> isize {
    let mut ask = ask(by, 0, 1000);
    ask.keys.insert(0, AskKey::Key(id("bucket")));
    ask.measures = vec![id(measure)];
    let before = live();
    PEAK.set(before);
    drop(store.ask(&ask).unwrap());
    PEAK.get() - before
}

/// A count of distinct values reads a group of one cell in place: by the
/// bucket, pay and zone each group is one cell, and eight times its
/// values cost the ask nothing. By the bucket alone a group meets 80
/// cells, marked in a set of bits from the least value to the greatest:
/// eight times their values cost nothing either, where a set of them grew.
#[test]
fn a_count_distinct_reads_a_group_of_one_cell_in_place() {
    const CELLS: i64 = 4_000;
    let spec = || spec_measuring(&[], Zones::Apart, vec![tip_distinct()]);
    let few = store_of(spec(), CELLS, 2);
    let many = store_of(spec(), CELLS, 16);
    let (few_cell, many_cell) = (
        by_bucket_peak(&few, &["pay", "pu"], "tip_distinct"),
        by_bucket_peak(&many, &["pay", "pu"], "tip_distinct"),
    );
    eprintln!("a cell a group: {few_cell} bytes, {many_cell} with 8x the values");
    assert!(many_cell - few_cell < 1_024, "{few_cell} then {many_cell}");
    let (few_cells, many_cells) = (
        by_bucket_peak(&few, &[], "tip_distinct"),
        by_bucket_peak(&many, &[], "tip_distinct"),
    );
    eprintln!("80 cells a group: {few_cells} bytes, {many_cells} with 8x the values");
    assert!(
        many_cells - few_cells < 1_024,
        "{few_cells} then {many_cells}"
    );
}

/// A percentile reads a group of one cell in place, its counts in value
/// order: by the bucket, pay and zone each group is one cell, and eight
/// times its values cost the ask nothing.
#[test]
fn a_percentile_reads_a_group_of_one_cell_in_place() {
    const CELLS: i64 = 4_000;
    let few = store_of(median_spec(), CELLS, 2);
    let many = store_of(median_spec(), CELLS, 16);
    let (few_peak, many_peak) = (
        by_bucket_peak(&few, &["pay", "pu"], "tip_median"),
        by_bucket_peak(&many, &["pay", "pu"], "tip_median"),
    );
    eprintln!("a cell a group: {few_peak} bytes, {many_peak} with 8x the values");
    assert!(many_peak - few_peak < 1_024, "{few_peak} then {many_peak}");
}

/// A zone held twice repeats its trips through the join; once the second
/// row goes, every key holds one row again and an ask walks each cell
/// once, allocating per group as before.
#[test]
fn a_dim_key_back_to_one_row_counts_each_cell_once_again() {
    let mut store = store(8_000);
    let before = allocations(&store, &["borough"]);
    let twice = [Datum::Int(3), Datum::Text(BOROUGHS[3].into())];
    store.add_row("zones", &twice).unwrap();
    let repeating = allocations(&store, &["borough"]);
    store.remove_row("zones", &twice).unwrap();
    let after = allocations(&store, &["borough"]);
    eprintln!("{before} allocations, {repeating} with a zone twice, {after} after");
    assert!(repeating > before, "{repeating} against {before}");
    assert_eq!(after, before);
}

/// As [`store_of`] with 40 tips a cell and the distinct tips measured,
/// each tip written as text, which a map of values keeps.
fn text_tips_store() -> Store {
    let spec = spec_measuring(&[], Zones::Apart, vec![tip_distinct()]);
    let columns = spec.fact("trips").unwrap().columns().clone();
    let mut store = Store::new(spec);
    for cell in 0..2_000 {
        for tip in 0..40 {
            let mut row = Vec::with_capacity(columns.len());
            for column in &columns {
                row.push(match trip_value(column.as_str(), cell, tip) {
                    Datum::Int(tip) if column.as_str() == "tip" => {
                        Datum::Text(tip.to_string().into())
                    }
                    value => value,
                });
            }
            store.add_row("trips", &row).unwrap();
        }
    }
    store
}

/// A count of distinct numbers keeps a cell's values as a max does, in a
/// run of as many bytes as they need, and holds no min or max beside. A
/// map of values was over 15 times a max. Text keeps its map, and
/// answers alike.
#[test]
fn a_count_distinct_of_numbers_holds_what_a_max_holds() {
    let max = held_by(|| vec![tip_max()]);
    let numbers = held_by(|| vec![tip_distinct()]);
    let text = held(text_tips_store);
    eprintln!("max {max} bytes, distinct numbers {numbers}, distinct text {text}");
    assert!(numbers <= max, "{numbers} bytes against {max}");
    assert!(text > 4 * max, "{text} bytes against {max}");
    let mut ask = ask(&["pay"], 0, 1000);
    ask.measures = vec![id("tip_distinct")];
    let spec = spec_measuring(&[], Zones::Apart, vec![tip_distinct()]);
    let answer = store_of(spec, 2_000, 40).ask(&ask).unwrap();
    assert_eq!(text_tips_store().ask(&ask).unwrap(), answer);
}
