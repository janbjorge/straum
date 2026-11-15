//! Random trips, dims, dim changes and asks for the workload. Each draws
//! from `Rng`, so a seed replays the same steps. Some keys find no dim row
//! and some rows have a null key, so joins miss as they do in PostgreSQL.

use std::collections::BTreeMap;

use crate::common::{Rng, condition, dim_row, id, lit, text};
use crate::{DRO, Dims, END, HOME, KEYS, START, WIDTH};
use straum_domain::{Ask, AskKey, Datum, Identifier, Literal, Spec, Store};

impl Rng {
    /// One of a few texts, or null `percent` percent of the time, as a
    /// dim value comes.
    fn attribute(&mut self, choices: &[&str], percent: usize) -> Datum {
        if self.chance(percent) {
            Datum::Null
        } else {
            text(self.pick(choices))
        }
    }
}

/// A whole row: the fact's columns, as the source hands them over.
pub(crate) fn trip(rng: &mut Rng) -> Vec<Datum> {
    let shared = if rng.chance(5) {
        Datum::Null
    } else {
        Datum::Bool(rng.chance(30))
    };
    let rider = if rng.chance(10) {
        Datum::Null
    } else {
        text(rng.pick(&["ann", "bo", "cy", "di", "ed"]))
    };
    let tolled = if rng.chance(20) {
        Datum::Null
    } else {
        Datum::Bool(rng.chance(40))
    };
    vec![
        rng.maybe(START, END, 2),
        rng.maybe(0, 4, 10),
        rng.maybe(1, 3, 0),
        shared,
        rng.maybe(1, 7, 5),
        rng.maybe(1, 7, 5),
        rng.maybe(1, 7, 5),
        rng.maybe(-500, 10_000, 5),
        rng.maybe(0, 300, 20),
        rng.maybe(0, 5_000, 5),
        rider,
        tolled,
    ]
}

/// Random dims over the keys trips point at: `pu`, `dro` and `home` in
/// 1..7, `vendor` in 1..3. Some keys have no row, so some joins find
/// nothing, and some rows have a null in their key, which nothing joins.
pub(crate) fn random_dims(rng: &mut Rng) -> Dims {
    let mut dims = Dims::new();
    let zones = dims.entry("zones").or_default();
    for id in 1..7 {
        if rng.chance(75) {
            let attributes = vec![
                rng.attribute(&["m", "q", "b"], 25),
                rng.attribute(&["a", "b", "c"], 25),
            ];
            zones.insert(vec![Datum::Int(id)], attributes);
        }
    }
    if rng.chance(50) {
        zones.insert(vec![Datum::Null], vec![text("m"), text("a")]);
    }
    let combos = dims.entry("combos").or_default();
    for pu in 1..7 {
        for vendor in 1..3 {
            if rng.chance(60) {
                let attributes = vec![rng.attribute(&["x", "y"], 50)];
                combos.insert(vec![Datum::Int(pu), Datum::Int(vendor)], attributes);
            }
        }
    }
    if rng.chance(50) {
        combos.insert(vec![Datum::Null, Datum::Int(1)], vec![text("x")]);
    }
    let vendors = dims.entry("vendors").or_default();
    for id in 1..3 {
        vendors.insert(vec![Datum::Int(id)], vec![rng.attribute(&["v1", "v2"], 50)]);
    }
    dims
}

/// The rows of a dim the store holds: those with no null in their key.
pub(crate) fn held(rows: &BTreeMap<Vec<Datum>, Vec<Datum>>) -> usize {
    (rows.keys())
        .filter(|key| !key.contains(&Datum::Null))
        .count()
}

/// Loads every dim, as a load does, rows laid end to end.
pub(crate) fn load_dims(store: &mut Store, dims: &Dims) {
    for (dim, rows) in dims {
        let flat: Vec<Datum> = (rows.iter())
            .flat_map(|(key, attributes)| dim_row(key, attributes))
            .collect();
        store.add_rows(dim, &flat).unwrap();
    }
}

/// A random change to one dim, applied to the store and to `dims`: a row
/// relabelled, removed or added, under a key with a null now and then,
/// or the dim truncated and loaded again.
pub(crate) fn change_dim(rng: &mut Rng, store: &mut Store, dims: &mut Dims) {
    if rng.chance(5) {
        let dim = *rng.pick(&["zones", "combos", "vendors"]);
        store.clear(dim);
        let rows = if rng.chance(50) {
            random_dims(rng).remove(dim).unwrap()
        } else {
            BTreeMap::new()
        };
        let flat: Vec<Datum> = (rows.iter())
            .flat_map(|(key, attributes)| dim_row(key, attributes))
            .collect();
        store.add_rows(dim, &flat).unwrap();
        dims.insert(dim, rows);
        return;
    }
    let (dim, key, attributes): (&'static str, Vec<Datum>, Vec<Datum>) = match rng.below(3) {
        0 => (
            "zones",
            vec![rng.maybe(1, 7, 5)],
            vec![
                rng.attribute(&["m", "q", "b"], 25),
                rng.attribute(&["a", "b", "c"], 25),
            ],
        ),
        1 => (
            "combos",
            vec![rng.maybe(1, 7, 5), rng.maybe(1, 3, 5)],
            vec![rng.attribute(&["x", "y"], 50)],
        ),
        _ => (
            "vendors",
            vec![rng.maybe(1, 3, 5)],
            vec![rng.attribute(&["v1", "v2"], 50)],
        ),
    };
    let rows = dims.get_mut(dim).unwrap();
    match rows.remove(&key) {
        Some(old) if rng.chance(40) => {
            store.remove_row(dim, &dim_row(&key, &old)).unwrap();
            return;
        }
        // Now and then to the row it was, which changes nothing.
        Some(old) => store
            .update_row(dim, &dim_row(&key, &old), &dim_row(&key, &attributes))
            .unwrap(),
        None => store.add_row(dim, &dim_row(&key, &attributes)).unwrap(),
    }
    rows.insert(key, attributes);
}

pub(crate) fn random_ask(rng: &mut Rng, spec: &Spec) -> Ask {
    let mut by: Vec<AskKey> = KEYS
        .iter()
        .filter(|_| rng.chance(25))
        .map(|key| AskKey::Key(id(key)))
        .collect();
    if rng.chance(60) {
        let at = rng.below(by.len() + 1);
        by.insert(at, AskKey::Bucket);
    }
    let pool = [
        condition("pay", vec![Literal::Int(1), Literal::Int(2)]),
        condition("vendor", vec![Literal::Int(2)]),
        condition("shared", vec![Literal::Bool(false)]),
        condition("pickup_borough", vec![lit("m")]),
        condition("dro", vec![Literal::Int(5)]),
        condition("dropoff_zone", vec![lit("a"), lit("c")]),
        condition("pu", vec![Literal::Int(1), Literal::Int(3)]),
        condition("combo_label", vec![lit("x")]),
        condition("known", vec![Literal::Bool(true)]),
        condition("unpaid", vec![Literal::Bool(false)]),
        condition("vend_label", vec![lit("v1")]),
        condition("home_borough", vec![lit("q"), lit("b")]),
    ];
    let filter = pool.into_iter().filter(|_| rng.chance(15)).collect();
    let all = &spec.fact("trips").unwrap().measures;
    let mut measures: Vec<Identifier> = all
        .iter()
        .filter(|_| rng.chance(50))
        .map(|measure| measure.name.clone())
        .collect();
    if measures.is_empty() {
        measures.push(rng.pick(all).name.clone());
    }
    let buckets = (END - START) / i64::from(WIDTH);
    let from = START + i64::from(WIDTH) * rng.int(-10, buckets);
    let to = from + i64::from(WIDTH) * rng.int(1, buckets + 20);
    Ask {
        view: id("trips"),
        measures,
        keys: by,
        filter,
        from,
        to,
    }
}

/// Makes `row` fall in the cell of `other`, or beside it: the same keys
/// from another home zone, which the bound cells hold as one cell when
/// the two zones are of one borough.
pub(crate) fn near(rng: &mut Rng, row: &mut [Datum], other: &[Datum]) {
    let same = if rng.chance(50) { HOME } else { DRO };
    row[..=same].clone_from_slice(&other[..=same]);
}
