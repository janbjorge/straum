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
    Ask, AskKey, Datum, RowError, Sealed, SealedColumn, SealedError, SealedId, Store, Value,
    ValueCount,
};

pub(crate) fn buckets_holding_start() -> [(i64, i64); 4] {
    // The one bucket of each width that holds START.
    let week = START.div_euclid(i64::from(WEEK)) * i64::from(WEEK);
    [
        (START, START + i64::from(WIDTH)),
        (START, START + i64::from(HOUR)),
        (START, START + i64::from(DAY)),
        (week, week + i64::from(WEEK)),
    ]
}

pub(crate) fn shuffle(tips: &mut [i64], rng: &mut Rng) {
    for at in (1..tips.len()).rev() {
        tips.swap(at, rng.below(at + 1));
    }
}

pub(crate) fn end_value(end: Option<(&i64, &usize)>) -> Value {
    end.map_or(Value::Null, |(&tip, _)| Value::Int(tip.into()))
}

pub(crate) fn check_ends(store: &Store, held: &BTreeMap<i64, usize>, sealed: Option<&Sealed>) {
    let rows = held.values().sum::<usize>() as i128;
    let expected = [
        Value::Int(rows),
        end_value(held.first_key_value()),
        end_value(held.last_key_value()),
    ];
    for (from, to) in buckets_holding_start() {
        let ask = Ask {
            view: id("trips"),
            measures: vec![id("trips"), id("tip_min"), id("tip_max")],
            keys: Vec::new(),
            filter: Vec::new(),
            from,
            to,
        };
        let thawed: Vec<_> = (store.sealed_read(&ask).unwrap().iter())
            .map(|_| store.thaw(sealed.unwrap().clone()).unwrap())
            .collect();
        // Only the narrowest width is sealed, so only its ask thaws.
        let narrowest = to - from == i64::from(WIDTH);
        assert_eq!(thawed.len(), usize::from(sealed.is_some() && narrowest));
        assert_eq!(
            store.ask_with(&ask, &thawed).unwrap().rows[0].values,
            expected,
            "{} values, {} s wide",
            held.len(),
            to - from
        );
    }
}

// Sealed, an ask reads the keyed cells: the thawed bucket at the
// narrowest width, the hot wider ones above it.
pub(crate) fn check_ends_through_a_seal(store: &mut Store, held: &BTreeMap<i64, usize>) {
    let bucket = sealed_id(i64::from(WIDTH), START);
    check_ends(store, held, None);
    let sealed = store.sealed_copy(&bucket).unwrap();
    store.seal(&bucket).unwrap();
    check_ends(store, held, Some(&sealed));
    store.unseal(store.thaw(sealed).unwrap()).unwrap();
    check_ends(store, held, None);
}

/// One cell through every form its count per value takes: a few values in
/// the cell itself, a run of them, and past what a run holds several, then
/// back down as the rows leave. All the way the min and max of every width
/// are the reference's: in the bound cells, and with the bucket sealed in
/// the keyed cells and the thawed bucket.
#[test]
fn a_cell_of_many_values_keeps_its_ends_at_every_width() {
    const VALUES: i64 = 3_000;
    let mut store = typed(&spec());
    let mut rng = Rng(7);
    let mut row = trip(&mut rng);
    row[AT] = Datum::Int(START);
    // Each value once, every third twice.
    let mut tips: Vec<i64> = (0..VALUES)
        .flat_map(|index| vec![index * 3 - VALUES; if index % 3 == 0 { 2 } else { 1 }])
        .collect();
    let mut held: BTreeMap<i64, usize> = BTreeMap::new();
    for round in 0..2 {
        shuffle(&mut tips, &mut rng);
        for (position, &tip) in tips.iter().enumerate() {
            row[TIP] = Datum::Int(tip);
            store.add_row("trips", &row).unwrap();
            *held.entry(tip).or_default() += 1;
            // One value, a few, a run more than half full, and several.
            if [0, 3, 900, tips.len() - 1].contains(&position) {
                check_ends_through_a_seal(&mut store, &held);
            } else if position % 97 == 0 {
                check_ends(&store, &held, None);
            }
        }
        assert_eq!(store.cells("trips"), Some(1));
        shuffle(&mut tips, &mut rng);
        for (position, &tip) in tips.iter().enumerate() {
            row[TIP] = Datum::Int(tip);
            store.remove_row("trips", &row).unwrap();
            let count = held.get_mut(&tip).unwrap();
            *count -= 1;
            if *count == 0 {
                held.remove(&tip);
            }
            // Runs thinned, one down to a few values, and the last.
            let left = tips.len() - 1 - position;
            if [2_000, 3, 1].contains(&left) {
                check_ends_through_a_seal(&mut store, &held);
            } else if position % 97 == 0 {
                check_ends(&store, &held, None);
            }
        }
        assert_eq!(store.cells("trips"), Some(0), "round {round}");
    }
}

/// A store with two rows in one 15-minute bucket and one in the next.
pub(crate) fn three_rows() -> (Store, Vec<Vec<Datum>>) {
    let mut store = typed(&spec());
    let mut row = trip(&mut Rng(3));
    let rows: Vec<Vec<Datum>> = [(START, 4), (START + 60, 9), (START + 900, 1)]
        .iter()
        .map(|&(at, tip)| {
            row[AT] = Datum::Int(at);
            row[TIP] = Datum::Int(tip);
            row.clone()
        })
        .collect();
    for row in &rows {
        store.add_row("trips", row).unwrap();
    }
    (store, rows)
}

pub(crate) fn sealed_id(width: i64, bucket_start: i64) -> SealedId {
    SealedId {
        fact: id("trips"),
        width,
        bucket_start,
    }
}

pub(crate) fn whole_range(by: Vec<AskKey>) -> Ask {
    Ask {
        view: id("trips"),
        measures: vec![id("trips"), id("tip_min"), id("tip_max"), id("riders")],
        keys: by,
        filter: Vec::new(),
        from: START,
        to: START + i64::from(WEEK),
    }
}

#[test]
fn a_sealed_bucket_answers_as_before() {
    let (mut store, _) = three_rows();
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

pub(crate) fn seal_refusal(store: &mut Store, id: &SealedId) -> &'static str {
    store.seal(id).unwrap_err().reason
}

#[test]
fn sealable_buckets_end_by_the_cutoff() {
    let (mut store, _) = three_rows();
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
    let (mut store, rows) = three_rows();
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
    let mut many = rows[0].clone();
    many.extend(rows[1].iter().cloned());
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

pub(crate) type Tamper<'a> = dyn Fn(&mut Sealed) + 'a;

pub(crate) struct SealedBucket {
    store: Store,
    good: Sealed,
}

impl SealedBucket {
    fn new() -> SealedBucket {
        let (mut store, _) = three_rows();
        let bucket = sealed_id(i64::from(WIDTH), START);
        let good = store.sealed_copy(&bucket).unwrap();
        store.seal(&bucket).unwrap();
        SealedBucket { store, good }
    }

    fn column(&self, is_kind: fn(&SealedColumn) -> bool) -> usize {
        self.good.columns.iter().position(is_kind).unwrap()
    }

    fn assert_refused(&self, cases: &[(&str, &Tamper<'_>)]) {
        for (reason, tamper) in cases {
            let mut sealed = self.good.clone();
            tamper(&mut sealed);
            assert_eq!(self.store.thaw(sealed).unwrap_err().reason, *reason);
        }
        assert!(self.store.thaw(self.good.clone()).is_ok());
    }
}

#[test]
fn thaw_refuses_a_bucket_whose_id_or_keys_do_not_fit() {
    SealedBucket::new().assert_refused(&[
        ("unknown fact", &|sealed| sealed.id.fact = id("nope")),
        ("no such width", &|sealed| sealed.id.width = 7),
        ("no cells", &|sealed| sealed.counts.clear()),
        ("a cell without rows", &|sealed| sealed.counts[0] = 0),
        ("keys don't fit the rollup key", &|sealed| {
            sealed.keys.pop();
        }),
        ("a code the store never gave", &|sealed| sealed.keys[0] = 99),
    ]);
}

#[test]
fn thaw_refuses_columns_that_do_not_fit_the_measures() {
    let bucket = SealedBucket::new();
    let extent = bucket.column(|column| matches!(column, SealedColumn::Extent { .. }));
    let count = bucket.column(|column| matches!(column, SealedColumn::Count { .. }));
    let bools = bucket.column(|column| matches!(column, SealedColumn::Bools { .. }));
    let sum = bucket.column(|column| matches!(column, SealedColumn::Sum { .. }));
    bucket.assert_refused(&[
        ("columns don't fit the measures", &|sealed| {
            sealed.columns.pop();
        }),
        ("columns don't fit the measures", &|sealed| {
            sealed.columns.swap(0, extent);
        }),
        ("columns don't fit the measures", &|sealed| {
            sealed.columns[0] = SealedColumn::Sum {
                sums: Vec::new(),
                counts: vec![0],
            };
        }),
        ("columns don't fit the measures", &|sealed| {
            sealed.columns[count] = SealedColumn::Count { counts: vec![] };
        }),
        // More values than the cell has rows.
        ("columns don't fit the measures", &|sealed| {
            sealed.columns[count] = SealedColumn::Count {
                counts: sealed.counts.iter().map(|rows| rows + 1).collect(),
            };
        }),
        ("columns don't fit the measures", &|sealed| {
            let SealedColumn::Sum { counts, .. } = &mut sealed.columns[sum] else {
                panic!("a sum");
            };
            counts[0] = sealed.counts[0] + 1;
        }),
        ("columns don't fit the measures", &|sealed| {
            sealed.columns[bools] = SealedColumn::Bools {
                trues: vec![0],
                falses: vec![],
            };
        }),
    ]);
}

#[test]
fn thaw_refuses_values_that_do_not_fit_their_cell() {
    let bucket = SealedBucket::new();
    let extent = bucket.column(|column| matches!(column, SealedColumn::Extent { .. }));
    let distinct = bucket.column(|column| matches!(column, SealedColumn::Distinct { .. }));
    bucket.assert_refused(&[
        ("columns don't fit the measures", &|sealed| {
            sealed.columns[extent] = SealedColumn::Extent {
                values: vec![vec![ValueCount { value: 1, count: 0 }]],
            };
        }),
        ("columns don't fit the measures", &|sealed| {
            sealed.columns[extent] = SealedColumn::Extent { values: vec![] };
        }),
        ("columns don't fit the measures", &|sealed| {
            sealed.columns[extent] = SealedColumn::Extent {
                values: vec![vec![
                    ValueCount { value: 9, count: 1 },
                    ValueCount { value: 4, count: 1 },
                ]],
            };
        }),
        ("columns don't fit the measures", &|sealed| {
            sealed.columns[extent] = SealedColumn::Extent {
                values: vec![vec![
                    ValueCount { value: 4, count: 1 },
                    ValueCount { value: 4, count: 1 },
                ]],
            };
        }),
        ("columns don't fit the measures", &|sealed| {
            sealed.columns[distinct] = SealedColumn::Distinct {
                values: vec![vec![(Datum::Null, 0)]],
            };
        }),
        ("columns don't fit the measures", &|sealed| {
            sealed.columns[distinct] = SealedColumn::Distinct { values: vec![] };
        }),
    ]);
}

#[test]
fn unseal_refuses_a_bucket_not_sealed_or_keyed_twice() {
    let (mut store, _) = three_rows();
    let id = sealed_id(i64::from(DAY), START);
    let mut sealed = store.sealed_copy(&id).unwrap();
    let err = store
        .unseal(store.thaw(sealed.clone()).unwrap())
        .unwrap_err();
    assert_eq!(err.reason, "not sealed");
    store.seal(&id).unwrap();
    // Two cells under one key.
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
    let err = store.unseal(store.thaw(sealed).unwrap()).unwrap_err();
    assert_eq!(err.reason, "a key held twice");
}

#[test]
fn sealed_error_display() {
    let err = SealedError {
        id: sealed_id(900, 1_800),
        reason: "no cells",
    };
    assert_eq!(
        err.to_string(),
        "sealed trips bucket of 900s at 1800: no cells"
    );
}

#[test]
fn a_row_of_a_bucket_never_filled_is_missing() {
    let (mut store, rows) = three_rows();
    let mut elsewhere = rows[0].clone();
    elsewhere[AT] = Datum::Int(START + i64::from(DAY));
    assert_eq!(
        store.remove_row("trips", &elsewhere),
        Err(RowError::Missing)
    );
}
