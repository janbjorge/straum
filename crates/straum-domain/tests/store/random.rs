//! Random trips, dims, dim changes and asks for the workload. Each draws
//! from `Rng`, so a seed replays the same steps. Some keys find no dim row
//! and some rows have a null key, so joins miss as they do in PostgreSQL.

use std::collections::BTreeMap;

use crate::common::{Rng, condition, dim_row, id, lit, text};
use crate::reference::measure_scale;
use crate::{DAY, DRO, DimRows, Dims, END, HOME, HOUR, KEYS, START, WEEK, WIDTH, trips_ask};
use straum_domain::{
    Aggregate, Answer, Ask, AskKey, Comparison, Condition, Datum, Fact, Identifier, Literal,
    MeasureCondition, Spec, Store, Value, Width,
};

/// The widths an ask's rows may take: the `date_bin` width and each
/// wider one the view keeps.
const ROW_WIDTHS: [u32; 4] = [WIDTH, HOUR, DAY, WEEK];

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

/// `count` random trips.
pub(crate) fn trips(rng: &mut Rng, count: usize) -> Vec<Vec<Datum>> {
    let mut trips = Vec::with_capacity(count);
    for _ in 0..count {
        trips.push(trip(rng));
    }
    trips
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
pub(crate) fn held(rows: &DimRows) -> usize {
    let mut held = 0;
    for key in rows.keys() {
        if !key.contains(&Datum::Null) {
            held += 1;
        }
    }
    held
}

/// A dim's rows laid end to end, as a load reads them.
fn laid_end_to_end(rows: &DimRows) -> Vec<Datum> {
    let mut flat = Vec::new();
    for (key, attributes) in rows {
        flat.extend(dim_row(key, attributes));
    }
    flat
}

/// Loads every dim, as a load does, rows laid end to end.
pub(crate) fn load_dims(store: &mut Store, dims: &Dims) {
    for (dim, rows) in dims {
        store.add_rows(dim, &laid_end_to_end(rows)).unwrap();
    }
}

/// One dim row a change brings: the dim, the key and the attributes.
struct DimChange {
    dim: &'static str,
    key: Vec<Datum>,
    attributes: Vec<Datum>,
}

/// A row of a random dim, under a key with a null now and then.
fn random_dim_row(rng: &mut Rng) -> DimChange {
    match rng.below(3) {
        0 => DimChange {
            dim: "zones",
            key: vec![rng.maybe(1, 7, 5)],
            attributes: vec![
                rng.attribute(&["m", "q", "b"], 25),
                rng.attribute(&["a", "b", "c"], 25),
            ],
        },
        1 => DimChange {
            dim: "combos",
            key: vec![rng.maybe(1, 7, 5), rng.maybe(1, 3, 5)],
            attributes: vec![rng.attribute(&["x", "y"], 50)],
        },
        _ => DimChange {
            dim: "vendors",
            key: vec![rng.maybe(1, 3, 5)],
            attributes: vec![rng.attribute(&["v1", "v2"], 50)],
        },
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
        store.add_rows(dim, &laid_end_to_end(&rows)).unwrap();
        dims.insert(dim, rows);
        return;
    }
    let DimChange {
        dim,
        key,
        attributes,
    } = random_dim_row(rng);
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

/// An ask over random keys, filters and measures of `trips`, rows by the
/// bucket at a random width it keeps, and a random range on that width's
/// edges that may reach past the rows at either end.
pub(crate) fn random_ask(rng: &mut Rng, spec: &Spec) -> Ask {
    let mut by = Vec::new();
    for key in KEYS {
        if rng.chance(25) {
            by.push(AskKey::Key(id(key)));
        }
    }
    let mut row_width = Width { seconds: WIDTH };
    if rng.chance(60) {
        let at = rng.below(by.len() + 1);
        row_width = Width {
            seconds: *rng.pick(&ROW_WIDTHS),
        };
        let key = if row_width.seconds == WIDTH {
            AskKey::Key(id("bucket"))
        } else {
            AskKey::WiderBucket {
                name: id("bucket"),
                width: row_width,
            }
        };
        by.insert(at, key);
    }
    let mut filter = Vec::new();
    for condition in condition_pool() {
        if rng.chance(15) {
            filter.push(condition);
        }
    }
    let all = &spec.fact("trips").unwrap().measures;
    let mut measures: Vec<Identifier> = Vec::new();
    for measure in all {
        if rng.chance(50) {
            measures.push(measure.name.clone());
        }
    }
    if measures.is_empty() {
        measures.push(rng.pick(all).name.clone());
    }
    let fact = spec.fact("trips").unwrap();
    let mut having = Vec::new();
    while rng.chance(30) {
        let measure = rng.pick(&measures).clone();
        having.push(random_having(rng, fact, measure));
    }
    let step = i64::from(row_width.seconds);
    let first = row_width.bucket_start(START).unwrap();
    let buckets = (END - first) / step;
    let from = first + step * rng.int(-10, buckets);
    let to = from + step * rng.int(1, buckets + 20);
    Ask {
        measures,
        filter,
        having,
        from: maybe_open(rng, from),
        to: maybe_open(rng, to),
        ..trips_ask(&[], by)
    }
}

/// `at`, or now and then an open end, which reads to the buckets held.
fn maybe_open(rng: &mut Rng, at: i64) -> Option<i64> {
    if rng.chance(15) {
        return None;
    }
    Some(at)
}

/// A HAVING condition on `measure`, its value near what the measure
/// answers: a boolean, an integer, or a decimal at the measure's scale or
/// one finer, so equal groups come up as well as larger and smaller.
fn random_having(rng: &mut Rng, fact: &Fact, measure: Identifier) -> MeasureCondition {
    let aggregate = &fact.measure(measure.as_str()).unwrap().aggregate;
    if matches!(aggregate, Aggregate::BoolAnd(_) | Aggregate::BoolOr(_)) {
        let comparison = *rng.pick(&[Comparison::Equal, Comparison::NotEqual]);
        let value = Literal::Bool(rng.chance(50));
        return MeasureCondition {
            measure,
            comparison,
            value,
        };
    }
    let comparison = *rng.pick(&[
        Comparison::Equal,
        Comparison::NotEqual,
        Comparison::Less,
        Comparison::LessOrEqual,
        Comparison::Greater,
        Comparison::GreaterOrEqual,
    ]);
    // A variance's scale, twice its column's, can pass what a value may
    // be written in.
    let scale = measure_scale(aggregate).min(MOST_PLACES - 1);
    let value = match rng.below(4) {
        0 => Literal::Int(rng.int(-2, 12)),
        // Past an i128 at the measure's scale.
        1 => Literal::Int(*rng.pick(&[i64::MIN, -1 - i64::MAX / 2, i64::MAX])),
        2 => Literal::Decimal {
            units: i128::from(rng.int(-500, 10_000)),
            scale,
        },
        _ => Literal::Decimal {
            units: i128::from(rng.int(-5_000, 100_000)),
            scale: scale + 1,
        },
    };
    MeasureCondition {
        measure,
        comparison,
        value,
    }
}

/// The most digits after the point a HAVING value may have.
const MOST_PLACES: u32 = 38;

/// Sets half the number values of `ask`'s HAVING to what a group of
/// `answer`, the ask without HAVING, holds, so a boundary is met exactly:
/// `>` and `>=` part there. An average that is no whole number of its
/// scale's units gives the one below, which only its fraction parts from.
pub(crate) fn meet_a_group(rng: &mut Rng, ask: &mut Ask, answer: &Answer) {
    if answer.rows.is_empty() {
        return;
    }
    let key_count = ask.keys.len();
    for condition in &mut ask.having {
        if matches!(condition.value, Literal::Bool(_)) || rng.chance(50) {
            continue;
        }
        let mut position = 0;
        while ask.measures[position] != condition.measure {
            position += 1;
        }
        let scale = answer.columns[key_count + position].scale;
        if scale > MOST_PLACES {
            continue;
        }
        let row = rng.pick(&answer.rows);
        let units = match row.values[position] {
            Value::Int(units) => units,
            Value::Avg { sum, count } => sum.div_euclid(i128::from(count)),
            _ => continue,
        };
        condition.value = Literal::Decimal { units, scale };
    }
}

/// The conditions a random ask draws its filter from.
fn condition_pool() -> [Condition; 12] {
    [
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
    ]
}

/// Makes `row` fall in the cell of `other`, or beside it: the same keys
/// from another home zone, which the bound cells hold as one cell when
/// the two zones are of one borough.
pub(crate) fn near(rng: &mut Rng, row: &mut [Datum], other: &[Datum]) {
    let same = if rng.chance(50) { HOME } else { DRO };
    row[..=same].clone_from_slice(&other[..=same]);
}
