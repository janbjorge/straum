//! Sealing: a bucket's cells handed to a spill, dropped from the store, and
//! thawed back for an ask or an unseal. The answers stay the same; a row of
//! a sealed bucket waits for the unseal; a sealed bucket that doesn't fit the
//! store is refused with the reason.

use std::collections::BTreeMap;

use crate::common::{Rng, id};
use crate::random::trip;
use crate::views::{spec, typed};
use crate::{AT, DAY, HOUR, START, TIP, WEEK, WIDTH};
use straum_domain::{
    Ask, AskKey, Datum, DatumCount, RowError, Sealed, SealedColumn, SealedError, SealedId, Store,
    Value, ValueCount,
};

/// The rows of each tip one cell holds.
type TipCounts = BTreeMap<i64, usize>;

/// One bucket's range, `from..to` in Unix seconds.
#[derive(Clone, Copy)]
struct BucketRange {
    from: i64,
    to: i64,
}

/// The one bucket of each width that holds `START`.
fn buckets_holding_start() -> [BucketRange; 4] {
    let week = START.div_euclid(i64::from(WEEK)) * i64::from(WEEK);
    [
        starting_at_start(WIDTH),
        starting_at_start(HOUR),
        starting_at_start(DAY),
        BucketRange {
            from: week,
            to: week + i64::from(WEEK),
        },
    ]
}

/// The bucket of `width` seconds that starts at `START`.
fn starting_at_start(width: u32) -> BucketRange {
    BucketRange {
        from: START,
        to: START + i64::from(width),
    }
}

/// Puts the tips in a random order, each order as likely.
fn shuffle(tips: &mut [i64], rng: &mut Rng) {
    for at in (1..tips.len()).rev() {
        tips.swap(at, rng.below(at + 1));
    }
}

/// A tip as an answer gives it, or null when the cell holds none.
fn tip_value(tip: Option<(&i64, &usize)>) -> Value {
    match tip {
        Some((&tip, _)) => Value::Int(tip.into()),
        None => Value::Null,
    }
}

/// The trips, least tip and greatest tip the store answers for the bucket
/// of each width that holds `START` are `held`'s. With `sealed`, the
/// narrowest bucket is sealed and its ask reads that copy.
fn check_ends(store: &Store, held: &TipCounts, sealed: Option<&Sealed>) {
    let mut rows: usize = 0;
    for count in held.values() {
        rows += count;
    }
    let expected = [
        Value::Int(rows as i128),
        tip_value(held.first_key_value()),
        tip_value(held.last_key_value()),
    ];
    for range in buckets_holding_start() {
        let ask = Ask {
            view: id("trips"),
            measures: vec![id("trips"), id("tip_min"), id("tip_max")],
            keys: Vec::new(),
            filter: Vec::new(),
            having: Vec::new(),
            from: range.from,
            to: range.to,
        };
        let mut thawed = Vec::new();
        for _ in store.sealed_read(&ask).unwrap() {
            thawed.push(store.thaw(sealed.unwrap().clone()).unwrap());
        }
        // Only the narrowest width is sealed, so only its ask thaws.
        let narrowest = range.to - range.from == i64::from(WIDTH);
        assert_eq!(thawed.len(), usize::from(sealed.is_some() && narrowest));
        assert_eq!(
            store.ask_with(&ask, &thawed).unwrap().rows[0].values,
            expected,
            "{} values, {} s wide",
            held.len(),
            range.to - range.from
        );
    }
}

/// As [`check_ends`], hot, then sealed, then hot again. Sealed, an ask
/// reads the keyed cells: the thawed bucket at the narrowest width, the
/// hot wider ones above it.
fn check_ends_through_a_seal(store: &mut Store, held: &TipCounts) {
    let bucket = sealed_id(i64::from(WIDTH), START);
    check_ends(store, held, None);
    let sealed = store.sealed_copy(&bucket).unwrap();
    store.seal(&bucket).unwrap();
    check_ends(store, held, Some(&sealed));
    store.unseal(store.thaw(sealed).unwrap()).unwrap();
    check_ends(store, held, None);
}

/// Values from below zero up, each once and every third twice.
fn tips_of_many_values() -> Vec<i64> {
    const VALUES: i64 = 3_000;
    let mut tips = Vec::new();
    for index in 0..VALUES {
        let tip = index * 3 - VALUES;
        tips.push(tip);
        if index % 3 == 0 {
            tips.push(tip);
        }
    }
    tips
}

/// Adds a row of each tip in turn to the one cell `row` falls in.
fn add_each_tip(store: &mut Store, row: &mut [Datum], tips: &[i64], held: &mut TipCounts) {
    for (position, &tip) in tips.iter().enumerate() {
        row[TIP] = Datum::Int(tip);
        store.add_row("trips", row).unwrap();
        *held.entry(tip).or_default() += 1;
        // One value, a few, a run more than half full, and several.
        if [0, 3, 900, tips.len() - 1].contains(&position) {
            check_ends_through_a_seal(store, held);
        } else if position % 97 == 0 {
            check_ends(store, held, None);
        }
    }
}

/// Removes the row of each tip in turn from the cell `row` falls in.
fn remove_each_tip(store: &mut Store, row: &mut [Datum], tips: &[i64], held: &mut TipCounts) {
    for (position, &tip) in tips.iter().enumerate() {
        row[TIP] = Datum::Int(tip);
        store.remove_row("trips", row).unwrap();
        let count = held.get_mut(&tip).unwrap();
        *count -= 1;
        if *count == 0 {
            held.remove(&tip);
        }
        // Runs thinned, one down to a few values, and the last.
        let left = tips.len() - 1 - position;
        if [2_000, 3, 1].contains(&left) {
            check_ends_through_a_seal(store, held);
        } else if position % 97 == 0 {
            check_ends(store, held, None);
        }
    }
}

/// One cell through every form its count per value takes: a few values in
/// the cell itself, a run of them, and past what a run holds several, then
/// back down as the rows leave. All the way the min and max of every width
/// are the reference's: in the bound cells, and with the bucket sealed in
/// the keyed cells and the thawed bucket.
#[test]
fn a_cell_of_many_values_keeps_its_ends_at_every_width() {
    let mut store = typed(&spec());
    let mut rng = Rng(7);
    let mut row = trip(&mut rng);
    row[AT] = Datum::Int(START);
    let mut tips = tips_of_many_values();
    let mut held = TipCounts::new();
    for round in 0..2 {
        shuffle(&mut tips, &mut rng);
        add_each_tip(&mut store, &mut row, &tips, &mut held);
        assert_eq!(store.cells("trips"), Some(1));
        shuffle(&mut tips, &mut rng);
        remove_each_tip(&mut store, &mut row, &tips, &mut held);
        assert_eq!(store.cells("trips"), Some(0), "round {round}");
    }
}

/// A store and the rows it holds.
struct Filled {
    store: Store,
    rows: Vec<Vec<Datum>>,
}

/// A store with two rows in one 15-minute bucket and one in the next.
fn three_rows() -> Filled {
    let mut store = typed(&spec());
    let mut row = trip(&mut Rng(3));
    let mut rows = Vec::new();
    for (at, tip) in [(START, 4), (START + 60, 9), (START + 900, 1)] {
        row[AT] = Datum::Int(at);
        row[TIP] = Datum::Int(tip);
        rows.push(row.clone());
    }
    for row in &rows {
        store.add_row("trips", row).unwrap();
    }
    Filled { store, rows }
}

fn sealed_id(width: i64, bucket_start: i64) -> SealedId {
    SealedId {
        fact: id("trips"),
        width,
        bucket_start,
    }
}

/// Trips, tips and riders by `by` over the week from `START`.
fn whole_range(by: Vec<AskKey>) -> Ask {
    Ask {
        view: id("trips"),
        measures: vec![id("trips"), id("tip_min"), id("tip_max"), id("riders")],
        keys: by,
        filter: Vec::new(),
        having: Vec::new(),
        from: START,
        to: START + i64::from(WEEK),
    }
}

#[test]
fn a_sealed_bucket_answers_as_before() {
    let mut store = three_rows().store;
    let ask = whole_range(vec![AskKey::Bucket]);
    let before = store.ask(&ask).unwrap();
    let cells = store.cells_read(&ask).unwrap();

    let id = sealed_id(i64::from(WIDTH), START);
    let sealed = store.sealed_copy(&id).unwrap();
    assert_eq!(sealed.counts, [2]);
    store.seal(&id).unwrap();
    assert_eq!(store.sealed_read(&ask).unwrap(), std::slice::from_ref(&id));
    assert_eq!(store.cells_read(&ask).unwrap(), cells);
    assert_eq!(store.cells("trips"), Some(2));
    let thawed = store.thaw(sealed.clone()).unwrap();
    assert_eq!(store.ask_with(&ask, &[thawed]).unwrap(), before);

    assert_eq!(store.sealed_count(), 1);
    store.unseal(store.thaw(sealed).unwrap()).unwrap();
    assert_eq!(store.sealed_read(&ask).unwrap(), []);
    assert_eq!(store.sealed_count(), 0);
    assert_eq!(store.ask(&ask).unwrap(), before);
}

fn seal_refusal(store: &mut Store, id: &SealedId) -> &'static str {
    store.seal(id).unwrap_err().reason
}

#[test]
fn sealable_buckets_end_by_the_cutoff() {
    let mut store = three_rows().store;
    let width = i64::from(WIDTH);
    assert_eq!(store.sealable(START + width - 1), []);
    assert_eq!(store.sealable(START + width), [sealed_id(width, START)]);
    let all = store.sealable(START + i64::from(WEEK) * 2);
    // The two buckets of 15 minutes; wider widths stay in memory.
    assert_eq!(
        all,
        [sealed_id(width, START), sealed_id(width, START + width)]
    );
    assert_eq!(store.sealed_count(), 0);
    for id in &all {
        store.seal(id).unwrap();
    }
    assert_eq!(store.seal(&all[0]).unwrap_err().reason, "sealed already");
    assert_eq!(store.sealable(START + i64::from(WEEK) * 2), []);
    assert_eq!(store.sealed("trips"), all);
    assert_eq!(store.sealed_count(), 2);
    store.clear("trips");
    assert_eq!(store.sealed("trips"), []);
    assert_eq!(store.sealed_count(), 0);
    // Gone with the truncate, or never there.
    assert_eq!(seal_refusal(&mut store, &all[0]), "no such bucket");
    assert_eq!(
        seal_refusal(&mut store, &sealed_id(width, START - width)),
        "no such bucket"
    );
    assert_eq!(
        seal_refusal(&mut store, &sealed_id(7, START)),
        "no such width"
    );
    let nope = SealedId {
        fact: id("nope"),
        ..all[0].clone()
    };
    assert_eq!(seal_refusal(&mut store, &nope), "unknown fact");
    assert_eq!(store.sealed_copy(&all[0]), None);
}

#[test]
fn a_row_in_a_sealed_bucket_waits_for_unseal() {
    let Filled { mut store, rows } = three_rows();
    let id = sealed_id(i64::from(DAY), START);
    let sealed = store.sealed_copy(&id).unwrap();
    store.seal(&id).unwrap();
    assert_eq!(
        store.sealed_touched("trips", &rows[2]),
        std::slice::from_ref(&id)
    );
    assert_eq!(store.sealed_touched("trips", &[]), []);
    assert_eq!(store.add_row("trips", &rows[0]), Err(RowError::Sealed));
    assert_eq!(store.remove_row("trips", &rows[0]), Err(RowError::Sealed));
    let many = [rows[0].clone(), rows[1].clone()].concat();
    assert_eq!(store.add_rows("trips", &many), Err(RowError::Sealed));
    // A row of another day goes in.
    let mut later = rows[0].clone();
    later[AT] = Datum::Int(START + i64::from(DAY));
    assert_eq!(store.sealed_touched("trips", &later), []);
    store.add_row("trips", &later).unwrap();

    store.unseal(store.thaw(sealed).unwrap()).unwrap();
    assert_eq!(store.sealed_touched("trips", &rows[2]), []);
    store.remove_row("trips", &rows[0]).unwrap();
    store.add_row("trips", &rows[0]).unwrap();
}

/// One way to spoil a sealed bucket, so the store must refuse it. A plain
/// function, so each case of a table has a name.
type Spoil = fn(&mut Sealed);

/// The reason the store gives for refusing a bucket, and how it was
/// spoiled.
struct Refused {
    reason: &'static str,
    spoil: Spoil,
}

/// The kinds of column a sealed bucket holds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ColumnKind {
    Count,
    Sum,
    Extent,
    Distinct,
    Bools,
}

fn kind_of(column: &SealedColumn) -> ColumnKind {
    match column {
        SealedColumn::Count { .. } => ColumnKind::Count,
        SealedColumn::Sum { .. } => ColumnKind::Sum,
        SealedColumn::Extent { .. } => ColumnKind::Extent,
        SealedColumn::Distinct { .. } => ColumnKind::Distinct,
        SealedColumn::Bools { .. } => ColumnKind::Bools,
    }
}

/// Where the first column of `kind` lies among the sealed bucket's.
fn column_of(sealed: &Sealed, kind: ColumnKind) -> usize {
    for (position, column) in sealed.columns.iter().enumerate() {
        if kind_of(column) == kind {
            return position;
        }
    }
    panic!("no such column")
}

/// The bucket at `START` sealed, and the copy a spill keeps of it.
struct SealedBucket {
    store: Store,
    good: Sealed,
}

impl SealedBucket {
    fn new() -> SealedBucket {
        let mut store = three_rows().store;
        let bucket = sealed_id(i64::from(WIDTH), START);
        let good = store.sealed_copy(&bucket).unwrap();
        store.seal(&bucket).unwrap();
        SealedBucket { store, good }
    }

    /// Each spoiled copy is refused with its reason; the good one thaws.
    fn assert_refused(&self, cases: &[Refused]) {
        for case in cases {
            let mut sealed = self.good.clone();
            (case.spoil)(&mut sealed);
            assert_eq!(self.store.thaw(sealed).unwrap_err().reason, case.reason);
        }
        assert!(self.store.thaw(self.good.clone()).is_ok());
    }
}

const COLUMNS_DO_NOT_FIT: &str = "columns don't fit the measures";

/// A case refused because its columns don't fit the measures.
fn misfit(spoil: Spoil) -> Refused {
    Refused {
        reason: COLUMNS_DO_NOT_FIT,
        spoil,
    }
}

fn name_another_fact(sealed: &mut Sealed) {
    sealed.id.fact = id("nope");
}

fn name_another_width(sealed: &mut Sealed) {
    sealed.id.width = 7;
}

fn drop_the_cells(sealed: &mut Sealed) {
    sealed.counts.clear();
}

fn empty_a_cell(sealed: &mut Sealed) {
    sealed.counts[0] = 0;
}

fn cut_a_key(sealed: &mut Sealed) {
    sealed.keys.pop();
}

fn code_a_key_anew(sealed: &mut Sealed) {
    sealed.keys[0] = 99;
}

#[test]
fn thaw_refuses_a_bucket_whose_id_or_keys_do_not_fit() {
    SealedBucket::new().assert_refused(&[
        Refused {
            reason: "unknown fact",
            spoil: name_another_fact,
        },
        Refused {
            reason: "no such width",
            spoil: name_another_width,
        },
        Refused {
            reason: "no cells",
            spoil: drop_the_cells,
        },
        Refused {
            reason: "a cell without rows",
            spoil: empty_a_cell,
        },
        Refused {
            reason: "keys don't fit the rollup key",
            spoil: cut_a_key,
        },
        Refused {
            reason: "a code the store never gave",
            spoil: code_a_key_anew,
        },
    ]);
}

fn drop_a_column(sealed: &mut Sealed) {
    sealed.columns.pop();
}

fn swap_the_first_column_and_the_extent(sealed: &mut Sealed) {
    let extent = column_of(sealed, ColumnKind::Extent);
    sealed.columns.swap(0, extent);
}

fn make_the_first_column_an_empty_sum(sealed: &mut Sealed) {
    sealed.columns[0] = SealedColumn::Sum {
        sums: Vec::new(),
        counts: vec![0],
    };
}

fn empty_the_count(sealed: &mut Sealed) {
    let count = column_of(sealed, ColumnKind::Count);
    sealed.columns[count] = SealedColumn::Count { counts: vec![] };
}

/// More values than the cell has rows.
fn count_more_than_the_rows(sealed: &mut Sealed) {
    let count = column_of(sealed, ColumnKind::Count);
    let mut counts = Vec::with_capacity(sealed.counts.len());
    for rows in &sealed.counts {
        counts.push(rows + 1);
    }
    sealed.columns[count] = SealedColumn::Count { counts };
}

fn sum_more_than_the_rows(sealed: &mut Sealed) {
    let sum = column_of(sealed, ColumnKind::Sum);
    let SealedColumn::Sum { counts, .. } = &mut sealed.columns[sum] else {
        panic!("a sum");
    };
    counts[0] = sealed.counts[0] + 1;
}

fn empty_the_falses(sealed: &mut Sealed) {
    let bools = column_of(sealed, ColumnKind::Bools);
    sealed.columns[bools] = SealedColumn::Bools {
        trues: vec![0],
        falses: vec![],
    };
}

#[test]
fn thaw_refuses_columns_that_do_not_fit_the_measures() {
    SealedBucket::new().assert_refused(&[
        misfit(drop_a_column),
        misfit(swap_the_first_column_and_the_extent),
        misfit(make_the_first_column_an_empty_sum),
        misfit(empty_the_count),
        misfit(count_more_than_the_rows),
        misfit(sum_more_than_the_rows),
        misfit(empty_the_falses),
    ]);
}

/// Sets the extent column to hold `values` for its cells.
fn set_extent(sealed: &mut Sealed, values: Vec<Vec<ValueCount>>) {
    let extent = column_of(sealed, ColumnKind::Extent);
    sealed.columns[extent] = SealedColumn::Extent { values };
}

/// Sets the extent column's first cell to hold `entries`, the others as
/// they are, so the column still has an entry per cell.
fn set_first_extent_cell(sealed: &mut Sealed, entries: Vec<ValueCount>) {
    let extent = column_of(sealed, ColumnKind::Extent);
    let SealedColumn::Extent { values } = &mut sealed.columns[extent] else {
        panic!("column_of found the extent");
    };
    values[0] = entries;
}

fn count_a_value_no_times(sealed: &mut Sealed) {
    set_first_extent_cell(sealed, vec![ValueCount { value: 1, count: 0 }]);
}

fn count_a_value_no_times_in_every_cell(sealed: &mut Sealed) {
    set_extent(sealed, vec![vec![ValueCount { value: 1, count: 0 }]]);
}

fn drop_the_extent_cells(sealed: &mut Sealed) {
    set_extent(sealed, vec![]);
}

fn put_values_out_of_order(sealed: &mut Sealed) {
    set_first_extent_cell(
        sealed,
        vec![
            ValueCount { value: 9, count: 1 },
            ValueCount { value: 4, count: 1 },
        ],
    );
}

fn put_a_value_twice(sealed: &mut Sealed) {
    set_first_extent_cell(
        sealed,
        vec![
            ValueCount { value: 4, count: 1 },
            ValueCount { value: 4, count: 1 },
        ],
    );
}

fn count_a_distinct_value_no_times(sealed: &mut Sealed) {
    let distinct = column_of(sealed, ColumnKind::Distinct);
    let SealedColumn::Distinct { values } = &mut sealed.columns[distinct] else {
        panic!("column_of found the distinct column");
    };
    values[0] = vec![DatumCount {
        value: Datum::Null,
        count: 0,
    }];
}

fn drop_the_distinct_cells(sealed: &mut Sealed) {
    let distinct = column_of(sealed, ColumnKind::Distinct);
    sealed.columns[distinct] = SealedColumn::Distinct { values: vec![] };
}

#[test]
fn thaw_refuses_values_that_do_not_fit_their_cell() {
    SealedBucket::new().assert_refused(&[
        misfit(count_a_value_no_times),
        misfit(count_a_value_no_times_in_every_cell),
        misfit(drop_the_extent_cells),
        misfit(put_values_out_of_order),
        misfit(put_a_value_twice),
        misfit(count_a_distinct_value_no_times),
        misfit(drop_the_distinct_cells),
    ]);
}

/// Every cell twice, so the bucket holds each key twice.
fn hold_every_key_twice(sealed: &mut Sealed) {
    sealed.keys.extend_from_slice(&sealed.keys.clone());
    sealed.counts.extend_from_slice(&sealed.counts.clone());
    for column in &mut sealed.columns {
        match column {
            SealedColumn::Count { counts } => counts.extend_from_slice(&counts.clone()),
            SealedColumn::Sum { sums, counts } => {
                sums.extend_from_slice(&sums.clone());
                counts.extend_from_slice(&counts.clone());
            }
            SealedColumn::Extent { values } => values.extend_from_slice(&values.clone()),
            SealedColumn::Distinct { values } => values.extend_from_slice(&values.clone()),
            SealedColumn::Bools { trues, falses } => {
                trues.extend_from_slice(&trues.clone());
                falses.extend_from_slice(&falses.clone());
            }
        }
    }
}

#[test]
fn unseal_refuses_a_bucket_not_sealed_or_keyed_twice() {
    let mut store = three_rows().store;
    let id = sealed_id(i64::from(DAY), START);
    let mut sealed = store.sealed_copy(&id).unwrap();
    let error = store
        .unseal(store.thaw(sealed.clone()).unwrap())
        .unwrap_err();
    assert_eq!(error.reason, "not sealed");
    store.seal(&id).unwrap();
    hold_every_key_twice(&mut sealed);
    let error = store.unseal(store.thaw(sealed).unwrap()).unwrap_err();
    assert_eq!(error.reason, "a key held twice");
}

#[test]
fn sealed_error_display() {
    let error = SealedError {
        id: sealed_id(900, 1_800),
        reason: "no cells",
    };
    assert_eq!(
        error.to_string(),
        "sealed trips bucket of 900s at 1800: no cells"
    );
}

#[test]
fn a_row_of_a_bucket_never_filled_is_missing() {
    let Filled { mut store, rows } = three_rows();
    let mut elsewhere = rows[0].clone();
    elsewhere[AT] = Datum::Int(START + i64::from(DAY));
    assert_eq!(
        store.remove_row("trips", &elsewhere),
        Err(RowError::Missing)
    );
}
