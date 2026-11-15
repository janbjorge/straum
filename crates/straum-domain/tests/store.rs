//! `Store` against a reference that aggregates the live rows directly
//! and joins them to the dims as they are, over random inserts, updates and
//! deletes of trips and of dim rows. Rows hold the fact's columns only; a
//! dim's values come through its projection when asked.

use std::collections::{BTreeMap, HashSet};

mod common;

use common::{
    Rng, Spilled, attribute, by_key, condition, dim_row, holds, id, lit, percentile,
    percentile_disc, table, text,
};
use straum_domain::{
    Aggregate, Answer, AnswerColumn, AnswerRow, Ask, AskError, AskKey, ByKey, ColumnType,
    Condition, Datum, Dim, Fact, Identifier, KeyPart, Literal, Measure, Origin, Ref, RefKind,
    RowError, Sealed, SealedColumn, SealedError, SealedId, Spec, Store, Time, Value, ValueCount,
    Via, Width,
};

const WIDTH: u32 = 900;
/// 2024-01-15 and 2024-04-15, 00:00 UTC: four months, one leap day.
const START: i64 = 1_705_276_800;
const END: i64 = 1_713_139_200;

const HOUR: u32 = 3_600;
const DAY: u32 = 86_400;
const WEEK: u32 = 604_800;

// Seeds of the random workload, and its size: the steps, how often it
// asks, and how many asks then.
const SEEDS: u64 = 16;
const STEPS: u32 = 2_000;
const CHECK_EVERY: u32 = 100;
const ASKS_PER_CHECK: usize = 10;

// Row positions; `columns` and `rollup_key` check them.
const AT: usize = 0;
const PAY: usize = 1;
const VENDOR: usize = 2;
const SHARED: usize = 3;
const PU: usize = 4;
const DRO: usize = 5;
const HOME: usize = 6;
const FARE: usize = 7;
const TIP: usize = 8;
const DIST: usize = 9;
const RIDER: usize = 10;
const TOLLED: usize = 11;
const ROW_LEN: usize = 12;

/// The dims as the projections key them: zones by `id`, combos by
/// `(pu_id, vendor_id)`, vendors by `id`; each key to its attributes.
type Dims = BTreeMap<&'static str, BTreeMap<Vec<Datum>, Vec<Datum>>>;

/// The types a load sets for each dim: the key, then the attributes.
const DIM_TYPES: [(&str, &[ColumnType]); 3] = [
    (
        "zones",
        &[ColumnType::Int, ColumnType::Text, ColumnType::Text],
    ),
    (
        "combos",
        &[ColumnType::Int, ColumnType::Int, ColumnType::Text],
    ),
    ("vendors", &[ColumnType::Int, ColumnType::Text]),
];

/// The scales of the numeric columns `run` sets, from this up. Each
/// differs, so an answer that takes a scale from the wrong column shows.
const TRIP_SCALES: u32 = 20;

const KEYS: [&str; 14] = [
    "pay",
    "vendor",
    "shared",
    "pu",
    "dro",
    "pickup_borough",
    "pickup_zone",
    "dropoff_borough",
    "dropoff_zone",
    "combo_label",
    "known",
    "unpaid",
    "vend_label",
    "home_borough",
];

fn spec() -> Spec {
    spec_joining(RefKind::Left)
}

/// As [`spec`], the vendors joined as `vend` says: an inner join drops
/// the trips of a vendor with no row, so their buckets hold no bound
/// cells.
fn spec_joining(vend: RefKind) -> Spec {
    let zones = Dim {
        as_text: Vec::new(),
        name: id("zones"),
        table: table("zones"),
        key: vec![id("id")],
        attributes: vec![id("borough"), id("zone")],
    };
    // Keyed by two fact columns, one of them a by column asks group by.
    let combos = Dim {
        as_text: Vec::new(),
        name: id("combos"),
        table: table("combos"),
        key: vec![id("pu_id"), id("vendor_id")],
        attributes: vec![id("label")],
    };
    let vendors = Dim {
        as_text: Vec::new(),
        name: id("vendors"),
        table: table("vendors"),
        key: vec![id("id")],
        attributes: vec![id("label")],
    };
    let trips = Fact {
        as_text: Vec::new(),
        name: id("trips"),
        table: table("trips"),
        time: Time {
            column: id("at"),
            width: Width { seconds: WIDTH },
            // Hours, days and weeks, so random ranges read every width.
            wider: vec![
                Width { seconds: HOUR },
                Width { seconds: DAY },
                Width { seconds: WEEK },
            ],
            origin: Origin::Timestamptz,
        },
        refs: trip_refs(vend),
        by_keys: trip_by_keys(),
        filter: vec![],
        measures: [
            count_sum_and_rank_measures(),
            average_count_and_bool_measures(),
        ]
        .concat(),
    };
    Spec::new(vec![zones, combos, vendors], vec![trips]).unwrap()
}

fn new_measure(name: &str, aggregate: Aggregate, filter: Vec<Condition>) -> Measure {
    Measure {
        name: id(name),
        aggregate,
        filter,
    }
}

fn trip_refs(vend: RefKind) -> Vec<Ref> {
    vec![
        Ref {
            alias: id("pickup"),
            dim: id("zones"),
            via: vec![Via::Fact(id("pu"))],
            kind: RefKind::Left,
            reads: vec![
                attribute("pickup_borough", "borough"),
                attribute("pickup_zone", "zone"),
            ],
        },
        Ref {
            alias: id("dropoff"),
            dim: id("zones"),
            via: vec![Via::Fact(id("dro"))],
            kind: RefKind::Left,
            reads: vec![
                attribute("dropoff_borough", "borough"),
                attribute("dropoff_zone", "zone"),
            ],
        },
        Ref {
            alias: id("combo"),
            dim: id("combos"),
            via: vec![Via::Fact(id("pu")), Via::Fact(id("vendor"))],
            kind: RefKind::Left,
            reads: vec![attribute("combo_label", "label")],
        },
        Ref {
            alias: id("vend"),
            dim: id("vendors"),
            via: vec![Via::Fact(id("vendor"))],
            kind: vend,
            reads: vec![attribute("vend_label", "label")],
        },
        // On a column that is no by key: trips from several home
        // zones of one borough share a bound cell, so a zone that
        // moves takes its trips out of a cell the others stay in.
        Ref {
            alias: id("home"),
            dim: id("zones"),
            via: vec![Via::Fact(id("home"))],
            kind: RefKind::Left,
            reads: vec![attribute("home_borough", "borough")],
        },
    ]
}

fn trip_by_keys() -> Vec<ByKey> {
    vec![
        by_key("pay"),
        by_key("vendor"),
        by_key("shared"),
        by_key("pu"),
        by_key("dro"),
        // Over by columns, which the row carries once: one key for
        // each way round.
        ByKey::IsNull {
            column: id("shared"),
            name: id("known"),
            negated: true,
        },
        ByKey::IsNull {
            column: id("pay"),
            name: id("unpaid"),
            negated: false,
        },
    ]
}

fn count_sum_and_rank_measures() -> Vec<Measure> {
    vec![
        new_measure("trips", Aggregate::Count, vec![]),
        new_measure("fare", Aggregate::Sum(id("fare")), vec![]),
        new_measure("tip_min", Aggregate::Min(id("tip")), vec![]),
        new_measure("tip_max", Aggregate::Max(id("tip")), vec![]),
        new_measure("tip_median", percentile(5, 1, "tip"), vec![]),
        new_measure("tip_lowest", percentile(0, 0, "tip"), vec![]),
        new_measure("tip_highest", percentile(1, 0, "tip"), vec![]),
        // A rank PostgreSQL's float8 puts one past exact math's at
        // every hundred values.
        new_measure("fare_p07", percentile(7, 2, "fare"), vec![]),
        new_measure(
            "card_tip_p90",
            percentile(90, 2, "tip"),
            vec![condition("pay", vec![Literal::Int(1)])],
        ),
    ]
}

fn average_count_and_bool_measures() -> Vec<Measure> {
    vec![
        // A sum over a column the load groups by.
        new_measure("tip_avg", Aggregate::Avg(id("tip")), vec![]),
        new_measure("distance", Aggregate::Avg(id("dist")), vec![]),
        new_measure("riders", Aggregate::CountDistinct(id("rider")), vec![]),
        new_measure("pickups", Aggregate::CountDistinct(id("pu")), vec![]),
        new_measure(
            "card",
            Aggregate::Count,
            vec![condition("pay", vec![Literal::Int(1)])],
        ),
        new_measure(
            "shared_fare",
            Aggregate::Sum(id("fare")),
            vec![condition("shared", vec![Literal::Bool(true)])],
        ),
        new_measure(
            "mq_fare",
            Aggregate::Sum(id("fare")),
            vec![
                condition("pickup_borough", vec![lit("m"), lit("q")]),
                condition("vendor", vec![Literal::Int(2)]),
            ],
        ),
        new_measure(
            "m_riders",
            Aggregate::CountDistinct(id("rider")),
            vec![condition("dropoff_borough", vec![lit("m")])],
        ),
        new_measure(
            "far_tip",
            Aggregate::Max(id("tip")),
            vec![condition("dro", vec![Literal::Int(3), Literal::Int(6)])],
        ),
        new_measure(
            "zone_four_distance",
            Aggregate::Avg(id("dist")),
            vec![condition("pu", vec![Literal::Int(4)])],
        ),
        new_measure("tipped", Aggregate::CountNotNull(id("tip")), vec![]),
        // Over text: a count reads no value, only that there is one.
        new_measure(
            "far_riders",
            Aggregate::CountNotNull(id("rider")),
            vec![condition("dro", vec![Literal::Int(3), Literal::Int(6)])],
        ),
        new_measure("all_tolled", Aggregate::BoolAnd(id("tolled")), vec![]),
        new_measure("any_tolled", Aggregate::BoolOr(id("tolled")), vec![]),
        new_measure(
            "card_tolled",
            Aggregate::BoolOr(id("tolled")),
            vec![condition("pay", vec![Literal::Int(1)])],
        ),
    ]
}

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
fn trip(rng: &mut Rng) -> Vec<Datum> {
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
fn random_dims(rng: &mut Rng) -> Dims {
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
fn held(rows: &BTreeMap<Vec<Datum>, Vec<Datum>>) -> usize {
    (rows.keys())
        .filter(|key| !key.contains(&Datum::Null))
        .count()
}

/// Loads every dim, as a load does, rows laid end to end.
fn load_dims(store: &mut Store, dims: &Dims) {
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
fn change_dim(rng: &mut Rng, store: &mut Store, dims: &mut Dims) {
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

fn random_ask(rng: &mut Rng, spec: &Spec) -> Ask {
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

/// A key's value for a row: a column, a null-ness key, or a dim value
/// joined as the dims are now, null when the join finds no row.
fn value(row: &[Datum], key: &Identifier, dims: &Dims) -> Datum {
    match key.as_str() {
        "known" => Datum::Bool(row[SHARED] != Datum::Null),
        "unpaid" => Datum::Bool(row[PAY] == Datum::Null),
        "home_borough" => joined(dims, "zones", &[row[HOME].clone()], 0),
        "pickup_borough" => joined(dims, "zones", &[row[PU].clone()], 0),
        "pickup_zone" => joined(dims, "zones", &[row[PU].clone()], 1),
        "dropoff_borough" => joined(dims, "zones", &[row[DRO].clone()], 0),
        "dropoff_zone" => joined(dims, "zones", &[row[DRO].clone()], 1),
        "combo_label" => joined(dims, "combos", &[row[PU].clone(), row[VENDOR].clone()], 0),
        "vend_label" => joined(dims, "vendors", &[row[VENDOR].clone()], 0),
        name => row[column(name)].clone(),
    }
}

fn joined(dims: &Dims, dim: &str, key: &[Datum], attribute: usize) -> Datum {
    if key.contains(&Datum::Null) {
        return Datum::Null;
    }
    let Some(attributes) = dims[dim].get(key) else {
        return Datum::Null;
    };
    attributes[attribute].clone()
}

/// The column types `run` sets.
fn trip_types() -> Vec<ColumnType> {
    let types = vec![
        ColumnType::Time { zoned: true },
        ColumnType::Int,
        ColumnType::Int,
        ColumnType::Bool,
        ColumnType::Int,
        ColumnType::Int,
        ColumnType::Int,
        trip_numeric(FARE),
        trip_numeric(TIP),
        trip_numeric(DIST),
        ColumnType::Text,
        ColumnType::Bool,
    ];
    assert_eq!(types.len(), ROW_LEN);
    types
}

fn trip_numeric(column: usize) -> ColumnType {
    ColumnType::Numeric {
        scale: TRIP_SCALES + u32::try_from(column).expect("a row position fits a scale"),
    }
}

/// A store with the types a load would set, for the fact and the dims.
fn typed(spec: &Spec) -> Store {
    let mut store = Store::new(spec.clone());
    store.set_types("trips", &trip_types()).unwrap();
    for (dim, types) in DIM_TYPES {
        store.set_types(dim, types).unwrap();
    }
    store
}

/// The scale a key's values print in: the dims' attributes are all text.
fn scale(key: &Identifier) -> u32 {
    match key.as_str() {
        "known" | "unpaid" | "pickup_borough" | "pickup_zone" | "dropoff_borough"
        | "dropoff_zone" | "combo_label" | "vend_label" | "home_borough" => 0,
        name => trip_types()[column(name)].scale(),
    }
}

/// Where a key, as an ask names it, sits in a row.
fn column(name: &str) -> usize {
    match name {
        "pay" => PAY,
        "vendor" => VENDOR,
        "shared" => SHARED,
        "pu" => PU,
        "dro" => DRO,
        "fare" => FARE,
        "tip" => TIP,
        "dist" => DIST,
        "rider" => RIDER,
        "tolled" => TOLLED,
        other => panic!("no column {other}"),
    }
}

fn passes(row: &[Datum], filter: &[Condition], dims: &Dims) -> bool {
    filter
        .iter()
        .all(|condition| holds(&value(row, &condition.key, dims), &condition.values))
}

/// The reference answer: the ask over the raw rows joined to the dims: no rollup, no buckets.
fn reference(spec: &Spec, ask: &Ask, rows: &[Vec<Datum>], dims: &Dims) -> Answer {
    let fact = spec.fact("trips").unwrap();
    let mut groups: BTreeMap<Vec<Datum>, Vec<&[Datum]>> = BTreeMap::new();
    if ask.keys.is_empty() {
        groups.insert(Vec::new(), Vec::new());
    }
    let vend = (fact.refs.iter())
        .find(|fact_ref| fact_ref.alias.as_str() == "vend")
        .unwrap();
    for row in rows {
        let Datum::Int(time) = row[AT] else { continue };
        if time < ask.from || time >= ask.to || !passes(row, &ask.filter, dims) {
            continue;
        }
        let vendor = &row[VENDOR..=VENDOR];
        let joins = vendor[0] != Datum::Null && dims["vendors"].contains_key(vendor);
        if vend.kind == RefKind::Inner && !joins {
            continue;
        }
        let key = ask
            .keys
            .iter()
            .map(|asked| match asked {
                AskKey::Key(name) => value(row, name, dims),
                AskKey::Bucket => Datum::Int(Width { seconds: WIDTH }.bucket_start(time)),
            })
            .collect();
        groups.entry(key).or_default().push(row);
    }
    let rows = groups
        .into_iter()
        .map(|(keys, rows)| {
            let values = ask
                .measures
                .iter()
                .map(|name| measure(fact.measure(name.as_str()).unwrap(), &rows, dims))
                .collect();
            AnswerRow { keys, values }
        })
        .collect();
    Answer {
        columns: reference_columns(fact, ask),
        rows,
    }
}

fn reference_columns(fact: &Fact, ask: &Ask) -> Vec<AnswerColumn> {
    let keys = ask.keys.iter().map(|asked| match asked {
        AskKey::Key(name) => AnswerColumn {
            name: name.to_string(),
            scale: scale(name),
            is_time: false,
        },
        AskKey::Bucket => AnswerColumn {
            name: asked.to_string(),
            scale: 0,
            is_time: true,
        },
    });
    let measures = ask.measures.iter().map(|name| AnswerColumn {
        name: name.to_string(),
        scale: measure_scale(&fact.measure(name.as_str()).unwrap().aggregate),
        is_time: false,
    });
    keys.chain(measures).collect()
}

fn measure_scale(aggregate: &Aggregate) -> u32 {
    match aggregate {
        Aggregate::Count
        | Aggregate::CountNotNull(_)
        | Aggregate::CountDistinct(_)
        | Aggregate::BoolAnd(_)
        | Aggregate::BoolOr(_) => 0,
        Aggregate::Sum(input)
        | Aggregate::Min(input)
        | Aggregate::Max(input)
        | Aggregate::Avg(input)
        | Aggregate::PercentileDisc { column: input, .. } => {
            trip_types()[column(input.as_str())].scale()
        }
    }
}

fn measure(measure: &Measure, rows: &[&[Datum]], dims: &Dims) -> Value {
    let rows: Vec<&[Datum]> = (rows.iter().copied())
        .filter(|row| passes(row, &measure.filter, dims))
        .collect();
    match &measure.aggregate {
        Aggregate::Count => Value::Int(rows.len() as i128),
        Aggregate::CountNotNull(input) => {
            let values = rows
                .iter()
                .filter(|row| row[column(input.as_str())] != Datum::Null);
            Value::Int(values.count() as i128)
        }
        Aggregate::Sum(input) => {
            let values = ints_in(&rows, input);
            int_or_null((!values.is_empty()).then(|| values.iter().sum()))
        }
        Aggregate::Avg(input) => {
            let values = ints_in(&rows, input);
            if values.is_empty() {
                Value::Null
            } else {
                Value::Avg {
                    sum: values.iter().sum(),
                    count: values.len() as u64,
                }
            }
        }
        Aggregate::Min(input) => int_or_null(ints_in(&rows, input).into_iter().min()),
        Aggregate::Max(input) => int_or_null(ints_in(&rows, input).into_iter().max()),
        Aggregate::CountDistinct(input) => {
            let seen: HashSet<_> = rows
                .iter()
                .map(|row| &row[column(input.as_str())])
                .filter(|datum| **datum != Datum::Null)
                .collect();
            Value::Int(seen.len() as i128)
        }
        Aggregate::BoolAnd(input) => bool_or_null(bools_in(&rows, input).into_iter().min()),
        Aggregate::BoolOr(input) => bool_or_null(bools_in(&rows, input).into_iter().max()),
        Aggregate::PercentileDisc { fraction, column } => {
            percentile_disc(ints_in(&rows, column), *fraction)
        }
    }
}

fn ints_in(rows: &[&[Datum]], input: &Identifier) -> Vec<i128> {
    let mut ints = Vec::new();
    for row in rows {
        if let Datum::Int(int) = row[column(input.as_str())] {
            ints.push(i128::from(int));
        }
    }
    ints
}

fn bools_in(rows: &[&[Datum]], input: &Identifier) -> Vec<bool> {
    let mut bools = Vec::new();
    for row in rows {
        if let Datum::Bool(flag) = row[column(input.as_str())] {
            bools.push(flag);
        }
    }
    bools
}

fn int_or_null(found: Option<i128>) -> Value {
    found.map_or(Value::Null, Value::Int)
}

fn bool_or_null(found: Option<bool>) -> Value {
    found.map_or(Value::Null, Value::Bool)
}

/// Makes `row` fall in the cell of `other`, or beside it: the same keys
/// from another home zone, which the bound cells hold as one cell when
/// the two zones are of one borough.
fn near(rng: &mut Rng, row: &mut [Datum], other: &[Datum]) {
    let same = if rng.chance(50) { HOME } else { DRO };
    row[..=same].clone_from_slice(&other[..=same]);
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Sealing {
    Never,
    AtRandom,
}

/// Random changes to trips and dims, and asks against the reference.
/// With `sealing`, buckets are sealed and unsealed at random as they go,
/// and the answers must not change; returns the sealed buckets asks
/// read. An ask that overlaps a sealed bucket reads the keyed cells, the
/// rest the bound cells, which move with the dims. Halfway the trips
/// are truncated. `vend` is how the vendors join.
fn run(seed: u64, sealing: Sealing, vend: RefKind) -> usize {
    let mut workload = Workload::new(seed, vend);
    for step in 0..STEPS {
        if sealing == Sealing::AtRandom && workload.rng.chance(5) {
            workload.seal_some();
        }
        if step == STEPS / 2 {
            workload.truncate_trips();
        }
        workload.change_one();
        if step % CHECK_EVERY == CHECK_EVERY - 1 {
            workload.check(step);
        }
    }
    workload.finish(sealing)
}

struct Workload {
    seed: u64,
    spec: Spec,
    store: Store,
    rng: Rng,
    rows: Vec<Vec<Datum>>,
    dims: Dims,
    spilled: Spilled,
}

impl Workload {
    fn new(seed: u64, vend: RefKind) -> Workload {
        let spec = spec_joining(vend);
        let mut store = typed(&spec);
        let mut rng = Rng(seed);
        let dims = random_dims(&mut rng);
        load_dims(&mut store, &dims);
        Workload {
            seed,
            spec,
            store,
            rng,
            rows: Vec::new(),
            dims,
            spilled: Spilled::new("trips"),
        }
    }

    fn seal_some(&mut self) {
        let before = self.rng.int(START, END + i64::from(WEEK));
        self.spilled.seal(&mut self.rng, &mut self.store, before);
    }

    fn truncate_trips(&mut self) {
        let seed = self.seed;
        self.store.clear("trips");
        self.spilled.forget();
        self.rows.clear();
        assert_eq!(self.store.cells("trips"), Some(0), "seed {seed}");
        assert_eq!(self.store.bound_cells("trips"), Some(0), "seed {seed}");
    }

    fn change_one(&mut self) {
        match self.rng.below(40) {
            0..=3 => change_dim(&mut self.rng, &mut self.store, &mut self.dims),
            4..=8 if !self.rows.is_empty() => self.remove_trip(),
            9..=14 if !self.rows.is_empty() => self.update_trip(),
            _ => self.add_trip(),
        }
    }

    fn remove_trip(&mut self) {
        let row = self.rows.swap_remove(self.rng.below(self.rows.len()));
        self.spilled.unseal(&mut self.store, &row);
        self.store.remove_row("trips", &row).unwrap();
    }

    fn update_trip(&mut self) {
        // Often into the same cell, or the one beside it.
        let index = self.rng.below(self.rows.len());
        let mut new = trip(&mut self.rng);
        if self.rng.chance(50) {
            near(&mut self.rng, &mut new, &self.rows[index]);
        }
        self.spilled.unseal(&mut self.store, &self.rows[index]);
        self.spilled.unseal(&mut self.store, &new);
        self.store
            .update_row("trips", &self.rows[index], &new)
            .unwrap();
        self.rows[index] = new;
    }

    fn add_trip(&mut self) {
        let mut row = trip(&mut self.rng);
        // Often into a cell that already holds rows, or beside it.
        if !self.rows.is_empty() && self.rng.chance(40) {
            let other = self.rng.below(self.rows.len());
            near(&mut self.rng, &mut row, &self.rows[other]);
        }
        self.spilled.unseal(&mut self.store, &row);
        self.store.add_row("trips", &row).unwrap();
        self.rows.push(row);
    }

    fn check(&mut self, step: u32) {
        let seed = self.seed;
        for (dim, rows) in &self.dims {
            assert_eq!(
                self.store.dim_rows(dim),
                Some(held(rows)),
                "seed {seed}, step {step}: {dim} holds a row with a null key"
            );
        }
        for _ in 0..ASKS_PER_CHECK {
            let ask = random_ask(&mut self.rng, &self.spec);
            assert_eq!(
                self.spilled.ask(&self.store, &ask),
                reference(&self.spec, &ask, &self.rows, &self.dims),
                "seed {seed}, step {step}, {ask:?}"
            );
        }
    }

    fn finish(mut self, sealing: Sealing) -> usize {
        let seed = self.seed;
        assert!(self.store.cells("trips").unwrap() > 0);
        // Home zones of one borough share bound cells, so these are fewer.
        if sealing == Sealing::Never {
            let (keyed, bound) = (self.store.cells("trips"), self.store.bound_cells("trips"));
            assert!(bound < keyed, "seed {seed}: {bound:?} bound of {keyed:?}");
        }
        for row in &self.rows {
            self.spilled.unseal(&mut self.store, row);
            self.store.remove_row("trips", row).unwrap();
        }
        assert_eq!(
            self.store.cells("trips").unwrap(),
            0,
            "seed {seed}: a cell outlived its rows"
        );
        assert_eq!(
            self.store.bound_cells("trips"),
            Some(0),
            "seed {seed}: a bound cell outlived its rows"
        );
        self.spilled.read
    }
}

#[test]
fn store_matches_naive_reference() {
    for seed in 0..SEEDS {
        assert_eq!(run(seed, Sealing::Never, RefKind::Left), 0);
    }
}

/// A sealed bucket has no bound cells, so an ask over one reads the keyed
/// cells, thawed; the rest read the bound cells.
#[test]
fn sealed_store_matches_naive_reference() {
    let read: usize = (0..SEEDS)
        .map(|seed| run(seed, Sealing::AtRandom, RefKind::Left))
        .sum();
    assert!(read > 1_000, "asks read {read} sealed buckets");
}

/// With an inner join a bucket whose trips name no vendor holds no bound
/// cells when it is sealed, and gains them when the vendor's row comes.
#[test]
fn sealed_inner_store_matches_naive_reference() {
    let read: usize = (0..SEEDS)
        .map(|seed| run(seed, Sealing::AtRandom, RefKind::Inner))
        .sum();
    assert!(read > 1_000, "asks read {read} sealed buckets");
}

#[derive(Clone, Copy)]
enum Widths {
    Quarter,
    QuarterAndHour,
}

/// Trips by pay, joined to zones for the borough as `pickup` says, or
/// with no join at all.
fn zones_spec(pickup: Option<RefKind>, widths: Widths) -> Spec {
    let zones = Dim {
        as_text: Vec::new(),
        name: id("zones"),
        table: table("zones"),
        key: vec![id("id")],
        attributes: vec![id("borough")],
    };
    let pickup = pickup.map(|kind| Ref {
        alias: id("pickup"),
        dim: id("zones"),
        via: vec![Via::Fact(id("pu"))],
        kind,
        reads: vec![attribute("borough", "borough")],
    });
    let trips = Fact {
        as_text: Vec::new(),
        name: id("trips"),
        table: table("trips"),
        time: Time {
            column: id("at"),
            width: Width { seconds: WIDTH },
            wider: match widths {
                Widths::Quarter => Vec::new(),
                Widths::QuarterAndHour => vec![Width { seconds: HOUR }],
            },
            origin: Origin::Timestamptz,
        },
        refs: pickup.into_iter().collect(),
        by_keys: vec![by_key("pay")],
        filter: vec![],
        measures: vec![Measure {
            name: id("trips"),
            aggregate: Aggregate::Count,
            filter: vec![],
        }],
    };
    Spec::new(vec![zones], vec![trips]).unwrap()
}

/// Zones 1 to 3 in borough m, 4 to 6 in q, and a trip from each at
/// `START`.
fn zoned_store() -> Store {
    let mut store = Store::new(zones_spec(Some(RefKind::Left), Widths::Quarter));
    for zone in 1..7 {
        let borough = if zone < 4 { "m" } else { "q" };
        store
            .add_row("zones", &[Datum::Int(zone), text(borough)])
            .unwrap();
    }
    for zone in 1..7 {
        store
            .add_row(
                "trips",
                &[Datum::Int(START), Datum::Int(1), Datum::Int(zone)],
            )
            .unwrap();
    }
    store
}

fn by_borough() -> Ask {
    by_borough_over(START, END)
}

fn by_borough_over(from: i64, to: i64) -> Ask {
    Ask {
        view: id("trips"),
        measures: vec![id("trips")],
        keys: vec![AskKey::Key(id("borough"))],
        filter: Vec::new(),
        from,
        to,
    }
}

/// The bound cells are keyed by the dim's column, so the zones' boroughs
/// make fewer of them than the zone ids make keyed cells, and an ask
/// reads the fewer. A zone change moves those cells and the ask still
/// reads them.
#[test]
fn bound_cells_follow_the_dim_column() {
    let mut store = zoned_store();
    let by_borough = by_borough();
    let answer = store.ask(&by_borough).unwrap();
    assert_eq!(store.cells("trips"), Some(6));
    assert_eq!(store.bound_cells("trips"), Some(2));
    assert_eq!(store.cells_read(&by_borough), Ok(2));

    store
        .remove_row("zones", &[Datum::Int(6), text("q")])
        .unwrap();
    store.add_row("zones", &[Datum::Int(6), text("m")]).unwrap();
    assert_eq!(store.bound_cells("trips"), Some(2));
    assert_eq!(store.cells_read(&by_borough), Ok(2));
    let moved = store.ask(&by_borough).unwrap();
    assert_eq!(answer.rows[0].values, [Value::Int(3)]);
    assert_eq!(moved.rows[0].values, [Value::Int(4)]);
    assert_eq!(moved.columns, answer.columns);
}

fn percentile_spec() -> Spec {
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
        refs: Vec::new(),
        by_keys: vec![by_key("pay")],
        filter: vec![],
        measures: vec![
            new_measure("top", Aggregate::Max(id("fare")), vec![]),
            new_measure("median", percentile(5, 1, "fare"), vec![]),
            new_measure("p99", percentile(99, 2, "fare"), vec![]),
            new_measure("lowest", percentile(0, 0, "fare"), vec![]),
            new_measure("highest", percentile(1, 0, "fare"), vec![]),
            new_measure("p07", percentile(7, 2, "fare"), vec![]),
        ],
    };
    Spec::new(vec![], vec![trips]).unwrap()
}

fn percentiles(store: &Store, measures: &[&str]) -> Vec<Vec<Value>> {
    let ask = Ask {
        view: id("trips"),
        measures: measures.iter().map(|name| id(name)).collect(),
        keys: vec![AskKey::Key(id("pay"))],
        filter: Vec::new(),
        from: START,
        to: END,
    };
    (store.ask(&ask).unwrap().rows.into_iter())
        .map(|row| row.values)
        .collect()
}

fn fare_row(at: i64, pay: i64, fare: i64) -> [Datum; 3] {
    [Datum::Int(at), Datum::Int(pay), Datum::Int(fare)]
}

/// Pay 1 holds 10 and 20 in one bucket, 20, 40 and 90 in the next; pay 2
/// one row of 5. Five values put the median at rank 3 and p99 at 5; a
/// delete moves p99 to the next value down.
#[test]
fn a_percentile_picks_the_value_at_its_rank() {
    let mut store = Store::new(percentile_spec());
    let width = i64::from(WIDTH);
    for (at, fare) in [(0, 10), (0, 20), (width, 20), (width, 40), (width, 90)] {
        store
            .add_row("trips", &fare_row(START + at, 1, fare))
            .unwrap();
    }
    store.add_row("trips", &fare_row(START, 2, 5)).unwrap();
    store
        .add_row("trips", &[Datum::Int(START), Datum::Int(2), Datum::Null])
        .unwrap();
    let asked = ["median", "p99", "lowest", "highest"];
    let int = Value::Int;
    assert_eq!(
        percentiles(&store, &asked),
        [
            vec![int(20), int(90), int(10), int(90)],
            vec![int(5), int(5), int(5), int(5)],
        ]
    );

    store
        .remove_row("trips", &fare_row(START + width, 1, 90))
        .unwrap();
    store.remove_row("trips", &fare_row(START, 2, 5)).unwrap();
    assert_eq!(
        percentiles(&store, &asked),
        [
            vec![int(20), int(40), int(10), int(40)],
            vec![Value::Null, Value::Null, Value::Null, Value::Null],
        ]
    );
}

/// PostgreSQL ranks in float8: 0.07 of 100 is 7.000000000000001 there,
/// so it gives the 8th value where exact math gives the 7th.
#[test]
fn a_percentile_ranks_in_float8_as_postgresql_does() {
    let mut store = Store::new(percentile_spec());
    for fare in 1..=100 {
        store
            .add_row(
                "trips",
                &[Datum::Int(START), Datum::Int(1), Datum::Int(fare)],
            )
            .unwrap();
    }
    assert_eq!(percentiles(&store, &["p07"]), [vec![Value::Int(8)]]);
}

/// A fact without refs has nothing to bind: its keyed cells are its
/// answer, however its rows come.
#[test]
fn a_fact_without_refs_keeps_no_bound_cells() {
    let mut store = Store::new(zones_spec(None, Widths::Quarter));
    // Without the join, a row carries no zone.
    store
        .add_row("trips", &[Datum::Int(START), Datum::Int(1)])
        .unwrap();
    store
        .add_rows("trips", &[Datum::Int(START), Datum::Int(2)])
        .unwrap();
    assert_eq!(store.cells("trips"), Some(2));
    assert_eq!(store.bound_cells("trips"), None);
}

fn paid_trip_at(time: Datum) -> [Datum; 2] {
    [time, Datum::Int(1)]
}

/// The range runs from the first `date_bin` bucket's start to the last
/// one's end, and a row without a time is in neither.
#[test]
fn the_range_spans_the_buckets_held() {
    let mut store = Store::new(zones_spec(None, Widths::QuarterAndHour));
    assert_eq!(store.range("trips"), None);
    assert_eq!(store.range("zones"), None);
    let width = i64::from(WIDTH);
    store.add_row("trips", &paid_trip_at(Datum::Null)).unwrap();
    assert_eq!(store.range("trips"), None);
    store
        .add_row("trips", &paid_trip_at(Datum::Int(START + 1)))
        .unwrap();
    assert_eq!(store.range("trips"), Some(START..START + width));
    store
        .add_row("trips", &paid_trip_at(Datum::Int(START + 3 * width)))
        .unwrap();
    assert_eq!(store.range("trips"), Some(START..START + 4 * width));
}

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

/// A dim change under a fact with no rows leaves nothing bound wrong, so
/// the bound cells stay and bind the rows that come after it.
#[test]
fn a_dim_change_under_no_rows_keeps_the_bound_cells() {
    let mut store = Store::new(zones_spec(Some(RefKind::Left), Widths::Quarter));
    store.add_row("zones", &[Datum::Int(1), text("m")]).unwrap();
    store
        .remove_row("zones", &[Datum::Int(1), text("m")])
        .unwrap();
    store.add_row("zones", &[Datum::Int(1), text("q")]).unwrap();
    assert_eq!(store.bound_cells("trips"), Some(0));
    store
        .add_row("trips", &[Datum::Int(START), Datum::Int(1), Datum::Int(1)])
        .unwrap();
    assert_eq!(store.bound_cells("trips"), Some(1));
    let answer = store.ask(&by_borough()).unwrap();
    assert_eq!(answer.rows[0].keys, [text("q")]);
}

/// Sealing a bucket takes its bound cells with it, so an ask over the
/// bucket reads the keyed cells, 6 for 2. Once the keyed cells are back
/// they are bound again, and a row of the bucket goes into both.
#[test]
fn a_sealed_bucket_is_read_through_its_keyed_cells() {
    let mut store = zoned_store();
    let by_borough = by_borough();
    let id = store.sealable(START + i64::from(WIDTH)).remove(0);
    let copy = store.sealed_copy(&id).unwrap();
    store.seal(&id).unwrap();
    assert_eq!(store.sealed_read(&by_borough), Ok(vec![id.clone()]));
    assert_eq!(store.cells_read(&by_borough), Ok(6));
    assert_eq!(store.bound_cells("trips"), Some(0));
    let thawed = store.thaw(copy).unwrap();
    store.unseal(thawed).unwrap();
    assert_eq!(store.sealed_read(&by_borough), Ok(vec![]));

    store
        .add_row("trips", &[Datum::Int(START), Datum::Int(1), Datum::Int(1)])
        .unwrap();
    assert_eq!(store.cells_read(&by_borough), Ok(2));
    assert_eq!(store.bound_cells("trips"), Some(2));
    let answer = store.ask(&by_borough).unwrap();
    assert_eq!(answer.rows[0].values, [Value::Int(4)]);
    assert_eq!(answer.rows[1].values, [Value::Int(3)]);
    store
        .remove_row("trips", &[Datum::Int(START), Datum::Int(1), Datum::Int(1)])
        .unwrap();
    assert_eq!(
        store.ask(&by_borough).unwrap().rows[0].values,
        [Value::Int(3)]
    );
}

fn load_three_zones_and_trips(store: &mut Store) {
    store.add_row("zones", &[Datum::Int(1), text("m")]).unwrap();
    store.add_row("zones", &[Datum::Int(2), text("m")]).unwrap();
    store.add_row("zones", &[Datum::Int(3), text("m")]).unwrap();
    store
        .add_row("trips", &[Datum::Int(START), Datum::Int(1), Datum::Int(1)])
        .unwrap();
    let later = START + i64::from(WIDTH);
    store
        .add_row("trips", &[Datum::Int(later), Datum::Int(1), Datum::Int(2)])
        .unwrap();
    store
        .add_row("trips", &[Datum::Int(later), Datum::Int(1), Datum::Int(3)])
        .unwrap();
}

/// A sealed quarter stays on the keyed cells, including the hour that
/// contains it. The next quarter, which the seal does not cover, keeps
/// its bound cells after a dim change.
#[test]
fn a_hot_quarter_keeps_bound_cells_beside_a_sealed_one() {
    let spec = zones_spec(Some(RefKind::Left), Widths::QuarterAndHour);
    let mut sealed = Store::new(spec.clone());
    let mut reference = Store::new(spec);
    load_three_zones_and_trips(&mut sealed);
    load_three_zones_and_trips(&mut reference);
    let sealed_id = sealed.sealable(START + i64::from(WIDTH)).remove(0);
    let copy = sealed.sealed_copy(&sealed_id).unwrap();
    sealed.seal(&sealed_id).unwrap();
    let old = [Datum::Int(1), text("m")];
    let new = [Datum::Int(1), text("q")];
    sealed.update_row("zones", &old, &new).unwrap();
    reference.update_row("zones", &old, &new).unwrap();

    let hot = by_borough_over(START + i64::from(WIDTH), START + 2 * i64::from(WIDTH));
    let hour = by_borough_over(START, START + i64::from(HOUR));
    assert_eq!(sealed.sealed_read(&hot).unwrap(), []);
    // Two keyed cells, one borough: the bound cells answer.
    assert_eq!(sealed.cells_read(&hot).unwrap(), 1);
    assert_eq!(sealed.ask(&hot).unwrap(), reference.ask(&hot).unwrap());
    // The hour also covers the sealed quarter. Its answer matches the
    // reference, which has zone 1's new borough.
    let thawed = sealed.thaw(copy).unwrap();
    assert_eq!(
        sealed.ask_with(&hour, &[thawed]).unwrap(),
        reference.ask(&hour).unwrap()
    );
}

/// Seals the bucket at `START`, hands it to `between`, puts it back and
/// adds one more trip from zone 1 to it.
fn through_a_seal(store: &mut Store, between: impl Fn(&mut Store)) {
    let id = store.sealable(START + i64::from(WIDTH)).remove(0);
    let copy = store.sealed_copy(&id).unwrap();
    store.seal(&id).unwrap();
    between(store);
    let thawed = store.thaw(copy).unwrap();
    store.unseal(thawed).unwrap();
    store
        .add_row("trips", &[Datum::Int(START), Datum::Int(1), Datum::Int(1)])
        .unwrap();
}

/// The same changes with the bucket hot throughout.
fn without_a_seal(store: &mut Store, between: impl Fn(&mut Store)) {
    between(store);
    store
        .add_row("trips", &[Datum::Int(START), Datum::Int(1), Datum::Int(1)])
        .unwrap();
}

fn move_zone_six_to_m(store: &mut Store) {
    let old = [Datum::Int(6), text("q")];
    let new = [Datum::Int(6), text("m")];
    store.update_row("zones", &old, &new).unwrap();
}

fn truncate_zones(store: &mut Store) {
    store.clear("zones");
}

fn reload_zones_into_b(store: &mut Store) {
    store.clear("zones");
    let rows = [Datum::Int(1), text("b"), Datum::Int(2), text("b")];
    store.add_rows("zones", &rows).unwrap();
}

fn insert_zone_one(store: &mut Store) {
    store.add_row("zones", &[Datum::Int(1), text("m")]).unwrap();
}

fn inner_zoned_store_with(trip: &[Datum]) -> Store {
    let mut store = Store::new(zones_spec(Some(RefKind::Inner), Widths::Quarter));
    store.add_row("trips", trip).unwrap();
    assert_eq!(store.bound_cells("trips"), Some(0));
    store
}

/// A dim row that changes while a bucket is sealed reaches the bucket's
/// trips once it is back: the bound cells answer, as they would had it
/// never been sealed, and every trip still comes out.
#[test]
fn a_dim_change_under_a_sealed_bucket_reaches_it_once_unsealed() {
    let (mut sealed, mut hot) = (zoned_store(), zoned_store());
    through_a_seal(&mut sealed, move_zone_six_to_m);
    without_a_seal(&mut hot, move_zone_six_to_m);
    let by_borough = by_borough();
    assert_eq!(sealed.cells_read(&by_borough), Ok(2));
    let answer = sealed.ask(&by_borough).unwrap();
    assert_eq!(answer, hot.ask(&by_borough).unwrap());
    assert_eq!(answer.rows[0].values, [Value::Int(5)]);
    assert_eq!(answer.rows[1].values, [Value::Int(2)]);
    for zone in 1..7 {
        let row = [Datum::Int(START), Datum::Int(1), Datum::Int(zone)];
        sealed.remove_row("trips", &row).unwrap();
    }
    assert_eq!(sealed.cells("trips"), Some(1));
    assert_eq!(sealed.bound_cells("trips"), Some(1));
}

/// A dim truncated while a bucket is sealed rebuilds the bound cells
/// without the bucket. Once it is back its trips are bound again, not
/// only the one that came after.
#[test]
fn a_dim_truncate_under_a_sealed_bucket_keeps_its_trips() {
    let (mut sealed, mut hot) = (zoned_store(), zoned_store());
    through_a_seal(&mut sealed, truncate_zones);
    without_a_seal(&mut hot, truncate_zones);
    let by_borough = by_borough();
    let answer = sealed.ask(&by_borough).unwrap();
    assert_eq!(answer, hot.ask(&by_borough).unwrap());
    assert_eq!(answer.rows[0].keys, [Datum::Null]);
    assert_eq!(answer.rows[0].values, [Value::Int(7)]);
}

/// As the truncate, for a dim loaded while the bucket is sealed.
#[test]
fn a_dim_load_under_a_sealed_bucket_keeps_its_trips() {
    let (mut sealed, mut hot) = (zoned_store(), zoned_store());
    through_a_seal(&mut sealed, reload_zones_into_b);
    without_a_seal(&mut hot, reload_zones_into_b);
    let by_borough = by_borough();
    let answer = sealed.ask(&by_borough).unwrap();
    assert_eq!(answer, hot.ask(&by_borough).unwrap());
    assert_eq!(answer.rows[0].keys, [Datum::Null]);
    assert_eq!(answer.rows[0].values, [Value::Int(4)]);
    assert_eq!(answer.rows[1].values, [Value::Int(3)]);
}

/// An inner join that keeps no trip of a bucket leaves it no bound cells
/// to seal. A zone that comes while it is sealed binds its trips once it
/// is back, and both still come out.
#[test]
fn a_sealed_bucket_without_bound_cells_gains_them_from_a_dim_insert() {
    let first = [Datum::Int(START), Datum::Int(2), Datum::Int(1)];
    let (mut sealed, mut hot) = (
        inner_zoned_store_with(&first),
        inner_zoned_store_with(&first),
    );
    through_a_seal(&mut sealed, insert_zone_one);
    without_a_seal(&mut hot, insert_zone_one);
    let by_borough = by_borough();
    let answer = sealed.ask(&by_borough).unwrap();
    assert_eq!(answer, hot.ask(&by_borough).unwrap());
    assert_eq!(answer.rows[0].values, [Value::Int(2)]);
    sealed.remove_row("trips", &first).unwrap();
    let second = [Datum::Int(START), Datum::Int(1), Datum::Int(1)];
    sealed.remove_row("trips", &second).unwrap();
    assert_eq!(sealed.bound_cells("trips"), Some(0));
}

#[test]
fn rows_added_together_match_rows_added_one_by_one() {
    let spec = spec();
    for seed in 0..4 {
        let mut rng = Rng(seed);
        let mut by_row = typed(&spec);
        let mut together = typed(&spec);
        let dims = random_dims(&mut rng);
        load_dims(&mut by_row, &dims);
        load_dims(&mut together, &dims);
        let rows: Vec<Vec<Datum>> = (0..2_000).map(|_| trip(&mut rng)).collect();
        for row in &rows {
            by_row.add_row("trips", row).unwrap();
        }
        let flat: Vec<Datum> = rows.concat();
        let mut rest = &flat[..];
        while !rest.is_empty() {
            let (batch, after) = rest.split_at(rest.len().min((1 + rng.below(300)) * ROW_LEN));
            together.add_rows("trips", batch).unwrap();
            rest = after;
        }
        assert_eq!(
            together.cells("trips"),
            by_row.cells("trips"),
            "seed {seed}"
        );
        for _ in 0..50 {
            let ask = random_ask(&mut rng, &spec);
            assert_eq!(
                together.ask(&ask).unwrap(),
                by_row.ask(&ask).unwrap(),
                "seed {seed}, {ask:?}"
            );
        }
        // The stream takes out rows the load put in.
        for row in &rows {
            together.remove_row("trips", row).unwrap();
        }
        assert_eq!(
            together.cells("trips").unwrap(),
            0,
            "seed {seed}: a cell outlived its rows"
        );
    }
}

/// A bound cuts even past everything held, and an empty store answers a
/// zero count.
#[test]
fn a_range_cuts_at_both_ends() {
    let spec = spec();
    let mut store = typed(&spec);
    let mut rng = Rng(5);
    let mut rows = Vec::new();
    for bucket in 0..4 {
        let mut row = trip(&mut rng);
        row[AT] = Datum::Int(START + i64::from(WIDTH) * bucket);
        store.add_row("trips", &row).unwrap();
        rows.push(row);
    }
    let width = i64::from(WIDTH);
    assert_eq!(
        trips_counted(&store, START - width, END + width),
        Value::Int(4)
    );
    assert_eq!(trips_counted(&store, START + width, END), Value::Int(3));
    assert_eq!(trips_counted(&store, START, START + width), Value::Int(1));
    assert_eq!(trips_counted(&store, START - width, START), Value::Int(0));
    assert_eq!(trips_counted(&store, END, END + width), Value::Int(0));
    let empty = typed(&spec);
    assert_eq!(trips_counted(&empty, START, END), Value::Int(0));
}

fn first_value(store: &Store, ask: &Ask) -> Value {
    store.ask(ask).unwrap().rows[0].values[0].clone()
}

fn trips_counted(store: &Store, from: i64, to: i64) -> Value {
    first_value(store, &trips_over(from, to, Vec::new()))
}

/// The count of trips, by bucket or not, over a range.
fn trips_over(from: i64, to: i64, keys: Vec<AskKey>) -> Ask {
    Ask {
        view: id("trips"),
        measures: vec![id("trips")],
        keys,
        filter: Vec::new(),
        from,
        to,
    }
}

/// A time before 1970 falls in the bucket that starts below it, at every
/// width: buckets floor, they don't round toward the epoch.
#[test]
fn a_time_before_the_epoch_floors_to_its_bucket() {
    let mut store = typed(&spec());
    let width = i64::from(WIDTH);
    let mut row = trip(&mut Rng(2));
    row[AT] = Datum::Int(-1);
    store.add_row("trips", &row).unwrap();
    let by_bucket = vec![AskKey::Bucket];
    let below = trips_over(-width, 0, by_bucket.clone());
    assert_eq!(
        store.ask(&below).unwrap().rows,
        [AnswerRow {
            keys: vec![Datum::Int(-width)],
            values: vec![Value::Int(1)]
        }]
    );
    assert_eq!(
        store.ask(&trips_over(0, width, by_bucket)).unwrap().rows,
        []
    );
    // The week before 1970 holds it, the one after does not.
    let week = i64::from(WEEK);
    assert_eq!(store.cells_read(&trips_over(-week, 0, Vec::new())), Ok(1));
    assert_eq!(store.cells_read(&trips_over(0, week, Vec::new())), Ok(0));
    store.remove_row("trips", &row).unwrap();
    assert_eq!(store.cells("trips"), Some(0));
}

/// A row without a time is kept, and no ask reads it, however wide its
/// range; it comes out as it went in.
#[test]
fn a_row_without_a_time_is_kept_and_never_answered() {
    let mut store = typed(&spec());
    let width = i64::from(WIDTH);
    let mut timed = trip(&mut Rng(2));
    timed[AT] = Datum::Int(START);
    let mut untimed = timed.clone();
    untimed[AT] = Datum::Null;
    store.add_row("trips", &timed).unwrap();
    store.add_row("trips", &untimed).unwrap();
    assert_eq!(store.cells("trips"), Some(2));
    assert_eq!(store.bound_cells("trips"), Some(2));

    // The widest range on bucket edges.
    let (from, to) = (i64::MIN / width * width, i64::MAX / width * width);
    let all = store.ask(&trips_over(from, to, Vec::new())).unwrap();
    assert_eq!(all.rows[0].values, [Value::Int(1)]);
    let by_bucket = trips_over(from, to, vec![AskKey::Bucket]);
    assert_eq!(
        store.ask(&by_bucket).unwrap().rows,
        [AnswerRow {
            keys: vec![Datum::Int(START)],
            values: vec![Value::Int(1)]
        }]
    );

    // Held once, so it comes out once.
    store.remove_row("trips", &untimed).unwrap();
    assert_eq!(store.cells("trips"), Some(1));
    assert_eq!(store.remove_row("trips", &untimed), Err(RowError::Missing));
}

fn first_keys(store: &Store, ask: &Ask) -> Vec<Datum> {
    (store.ask(ask).unwrap().rows.into_iter())
        .map(|row| row.keys[0].clone())
        .collect()
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
        from: START,
        to: END,
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
    let v1 = [Datum::Int(1), text("v1")];
    let v2 = [Datum::Int(1), text("v2")];
    store.add_row("vendors", &v1).unwrap();
    let mut row = trip(&mut Rng(5));
    row[AT] = Datum::Int(START);
    row[VENDOR] = Datum::Int(1);
    store.add_row("trips", &row).unwrap();
    assert_eq!(store.bound_cells("trips"), Some(1));

    store.update_row("vendors", &v1, &v1).unwrap();
    assert_eq!(store.bound_cells("trips"), Some(1));
    assert_eq!(
        store.update_row("vendors", &v1[..1], &v1[..1]),
        Err(RowError::Arity {
            expected: 2,
            found: 1
        })
    );
    store.update_row("vendors", &v1, &v2).unwrap();
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

fn trip_at_start_from(rng: &mut Rng, pu: Datum, vendor: Datum) -> Vec<Datum> {
    let mut row = trip(rng);
    row[AT] = Datum::Int(START);
    row[PU] = pu;
    row[VENDOR] = vendor;
    row
}

fn by_combo_label() -> Ask {
    Ask {
        view: id("trips"),
        measures: vec![id("trips")],
        keys: vec![AskKey::Key(id("combo_label"))],
        filter: Vec::new(),
        from: START,
        to: END,
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
        from: START,
        to: END,
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

/// Every key at once, so the group table is sparse and a cell's group is
/// numbered as it is met.
#[test]
fn every_key_at_once_uses_the_sparse_index() {
    let spec = spec();
    let mut store = typed(&spec);
    let mut rng = Rng(9);
    let dims = random_dims(&mut rng);
    load_dims(&mut store, &dims);
    let rows: Vec<Vec<Datum>> = (0..500).map(|_| trip(&mut rng)).collect();
    for row in &rows {
        store.add_row("trips", row).unwrap();
    }
    let ask = Ask {
        view: id("trips"),
        measures: vec![id("trips"), id("riders")],
        keys: KEYS.iter().map(|key| AskKey::Key(id(key))).collect(),
        filter: Vec::new(),
        from: START,
        to: END,
    };
    assert_eq!(
        store.ask(&ask).unwrap(),
        reference(&spec, &ask, &rows, &dims)
    );
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

#[test]
fn dim_rows_added_together_match_rows_added_one_by_one() {
    let spec = spec();
    let mut rng = Rng(2);
    let dims = random_dims(&mut rng);
    let mut together = typed(&spec);
    load_dims(&mut together, &dims);
    let mut by_row = typed(&spec);
    for (dim, rows) in &dims {
        for (key, attributes) in rows {
            by_row.add_row(dim, &dim_row(key, attributes)).unwrap();
        }
    }
    let rows: Vec<Vec<Datum>> = (0..300).map(|_| trip(&mut rng)).collect();
    for row in &rows {
        together.add_row("trips", row).unwrap();
        by_row.add_row("trips", row).unwrap();
    }
    for _ in 0..20 {
        let ask = random_ask(&mut rng, &spec);
        assert_eq!(together.ask(&ask).unwrap(), by_row.ask(&ask).unwrap());
    }
}

#[test]
fn rows_added_together_add_none_on_error() {
    let mut rng = Rng(5);
    let mut store = Store::new(spec());
    let good = [trip(&mut rng), trip(&mut rng)].concat();
    assert_eq!(
        store.add_rows("trips", &good[1..]),
        Err(RowError::Arity {
            expected: ROW_LEN,
            found: ROW_LEN - 1
        })
    );
    let mut bad = good.clone();
    bad[ROW_LEN + AT] = text("noon");
    assert_eq!(
        store.add_rows("trips", &bad),
        Err(RowError::NotInt { column: id("at") })
    );
    assert_eq!(store.cells("trips"), Some(0));
    store.add_rows("trips", &[]).unwrap();
    store.add_rows("trips", &good).unwrap();
    assert!(store.cells("trips").unwrap() > 0);
}

#[test]
fn columns_put_measure_inputs_after_the_key() {
    let spec = spec();
    let columns: Vec<_> = spec
        .fact("trips")
        .unwrap()
        .columns()
        .iter()
        .map(Identifier::to_string)
        .collect();
    let expected = [
        (AT, "at"),
        (PAY, "pay"),
        (VENDOR, "vendor"),
        (SHARED, "shared"),
        (PU, "pu"),
        (DRO, "dro"),
        (HOME, "home"),
        (FARE, "fare"),
        (TIP, "tip"),
        (DIST, "dist"),
        (RIDER, "rider"),
        (TOLLED, "tolled"),
    ];
    assert_eq!(columns, expected.map(|(_, name)| name.to_owned()));
    for (pos, name) in expected {
        assert_eq!(columns[pos], name);
    }
}

/// The refs' via columns follow the by keys in the rollup key, each slot
/// once: here `home` is the one via that is no by column.
#[test]
fn rollup_key_puts_via_columns_after_the_by_keys_each_once() {
    let spec = spec();
    let key: Vec<String> = (spec.fact("trips").unwrap().rollup_key())
        .map(|slot| match slot {
            KeyPart::Column(column) => column.to_string(),
            KeyPart::IsNull { column, negated } => {
                format!("{column} is {}null", if negated { "not " } else { "" })
            }
            KeyPart::Bucket => "bucket".into(),
        })
        .collect();
    assert_eq!(
        key,
        [
            "pay",
            "vendor",
            "shared",
            "pu",
            "dro",
            "shared is not null",
            "pay is null",
            "home",
            "bucket",
        ]
    );
}

#[test]
fn rows_collapse_into_cells() {
    let mut store = Store::new(spec());
    let mut row = trip(&mut Rng(7));
    row[AT] = Datum::Int(START + 1);
    store.add_row("trips", &row).unwrap();
    row[AT] = Datum::Int(START + i64::from(WIDTH) - 1);
    store.add_row("trips", &row).unwrap();
    assert_eq!(store.cells("trips"), Some(1));
    row[AT] = Datum::Int(START + i64::from(WIDTH));
    store.add_row("trips", &row).unwrap();
    assert_eq!(store.cells("trips"), Some(2));
    // A join column is a key: another value is another cell.
    row[PU] = Datum::Int(9);
    store.add_row("trips", &row).unwrap();
    assert_eq!(store.cells("trips"), Some(3));
    // A dim row is none.
    store
        .add_row("zones", &[Datum::Int(9), text("m"), text("a")])
        .unwrap();
    assert_eq!(store.cells("trips"), Some(3));
}

fn first_values(store: &Store, ask: &Ask) -> Vec<Value> {
    store.ask(ask).unwrap().rows[0].values.clone()
}

#[test]
fn removing_an_extreme_reveals_the_next() {
    let mut store = Store::new(spec());
    let ask = Ask {
        view: id("trips"),
        measures: vec![id("tip_min"), id("tip_max")],
        keys: Vec::new(),
        filter: Vec::new(),
        from: START,
        to: END,
    };
    let mut row = trip(&mut Rng(11));
    row[AT] = Datum::Int(START);
    let rows: Vec<_> = [5, 9, 9, 2, 7]
        .iter()
        .map(|&tip| {
            row[TIP] = Datum::Int(tip);
            row.clone()
        })
        .collect();
    for row in &rows {
        store.add_row("trips", row).unwrap();
    }
    assert_eq!(store.cells("trips"), Some(1));
    assert_eq!(first_values(&store, &ask), [Value::Int(2), Value::Int(9)]);

    // One of two 9s out: the max stays.
    store.remove_row("trips", &rows[1]).unwrap();
    assert_eq!(first_values(&store, &ask), [Value::Int(2), Value::Int(9)]);
    store.remove_row("trips", &rows[2]).unwrap();
    assert_eq!(first_values(&store, &ask), [Value::Int(2), Value::Int(7)]);
    store.remove_row("trips", &rows[3]).unwrap();
    assert_eq!(first_values(&store, &ask), [Value::Int(5), Value::Int(7)]);
    // A value past the old extremes after they went.
    row[TIP] = Datum::Int(1);
    store.add_row("trips", &row).unwrap();
    assert_eq!(first_values(&store, &ask), [Value::Int(1), Value::Int(7)]);
}

fn buckets_holding_start() -> [(i64, i64); 4] {
    // The one bucket of each width that holds START.
    let week = START.div_euclid(i64::from(WEEK)) * i64::from(WEEK);
    [
        (START, START + i64::from(WIDTH)),
        (START, START + i64::from(HOUR)),
        (START, START + i64::from(DAY)),
        (week, week + i64::from(WEEK)),
    ]
}

fn shuffle(tips: &mut [i64], rng: &mut Rng) {
    for at in (1..tips.len()).rev() {
        tips.swap(at, rng.below(at + 1));
    }
}

fn end_value(end: Option<(&i64, &usize)>) -> Value {
    end.map_or(Value::Null, |(&tip, _)| Value::Int(tip.into()))
}

fn check_ends(store: &Store, held: &BTreeMap<i64, usize>, sealed: Option<&Sealed>) {
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
fn check_ends_through_a_seal(store: &mut Store, held: &BTreeMap<i64, usize>) {
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

// The fact's columns: its time, its key, the column it counts.
fn tip_row(pay: i64, tip: Datum) -> Vec<Datum> {
    vec![Datum::Int(START), Datum::Int(pay), tip]
}

fn trips_and_tipped(store: &Store, keys: Vec<AskKey>) -> Vec<Vec<Value>> {
    let ask = Ask {
        view: id("trips"),
        measures: vec![id("trips"), id("tipped")],
        keys,
        filter: Vec::new(),
        from: START,
        to: START + i64::from(HOUR),
    };
    let rows = store.ask(&ask).unwrap().rows;
    rows.into_iter().map(|row| row.values).collect()
}

fn trips_and_tipped_pair(trips: i128, tipped: i128) -> Vec<Value> {
    vec![Value::Int(trips), Value::Int(tipped)]
}

/// A count of a column that nothing else reads: no sum, min or max is
/// there to notice a value its cell lacks, so the count has to. It counts
/// the nulls among a cell's rows, and tells from them both a null the
/// cell never held and a value where it holds only nulls.
#[test]
fn a_count_alone_notices_a_value_its_cell_lacks() {
    let trips = Fact {
        as_text: Vec::new(),
        name: id("trips"),
        table: table("trips"),
        time: Time {
            column: id("at"),
            width: Width { seconds: WIDTH },
            wider: vec![Width { seconds: HOUR }],
            origin: Origin::Timestamptz,
        },
        refs: Vec::new(),
        by_keys: vec![by_key("pay")],
        filter: vec![],
        measures: vec![
            new_measure("trips", Aggregate::Count, vec![]),
            new_measure("tipped", Aggregate::CountNotNull(id("tip")), vec![]),
        ],
    };
    let spec = Spec::new(Vec::new(), vec![trips]).unwrap();
    // What the count makes of a row that holds each of the two.
    for (held, lacked, of_held, of_lacked) in [
        (Datum::Null, Datum::Int(5), 0, 1),
        (Datum::Int(5), Datum::Null, 1, 0),
    ] {
        let mut store = Store::new(spec.clone());
        // Two cells, so an ask merges them into one group and into two.
        for pay in [1, 1, 2] {
            store.add_row("trips", &tip_row(pay, held.clone())).unwrap();
        }
        store.add_row("trips", &tip_row(2, lacked.clone())).unwrap();
        assert_eq!(
            trips_and_tipped(&store, Vec::new()),
            [trips_and_tipped_pair(4, 3 * of_held + of_lacked)]
        );
        assert_eq!(
            trips_and_tipped(&store, vec![AskKey::Key(id("pay"))]),
            [
                trips_and_tipped_pair(2, 2 * of_held),
                trips_and_tipped_pair(2, of_held + of_lacked)
            ]
        );
        assert_eq!(
            store.remove_row("trips", &tip_row(1, lacked)),
            Err(RowError::Missing),
            "{held:?} held"
        );
        // What a cell holds leaves it. The store above missed a row, so
        // it no longer matches its table.
        let mut store = Store::new(spec.clone());
        store.add_row("trips", &tip_row(1, held.clone())).unwrap();
        assert_eq!(store.remove_row("trips", &tip_row(1, held)), Ok(()));
        assert_eq!(store.cells("trips"), Some(0));
    }
}

/// A row whose tip its cell doesn't hold was never added, whichever form
/// the cell's count per value has: in the cell, a run or several.
#[test]
fn a_tip_its_cell_lacks_is_missing() {
    for values in [1, 5, 300, 3_000] {
        let mut store = typed(&spec());
        let mut row = trip(&mut Rng(7));
        row[AT] = Datum::Int(START);
        for tip in 0..values {
            row[TIP] = Datum::Int(tip);
            store.add_row("trips", &row).unwrap();
        }
        assert_eq!(store.cells("trips"), Some(1));
        row[TIP] = Datum::Int(-1);
        assert_eq!(
            store.remove_row("trips", &row),
            Err(RowError::Missing),
            "{values} values"
        );
    }
}

#[test]
fn clear_drops_every_row() {
    let mut store = Store::new(spec());
    let trip = trip(&mut Rng(5));
    store.add_row("trips", &trip).unwrap();
    store.clear("trips");
    assert_eq!(store.cells("trips"), Some(0));
    assert_eq!(store.remove_row("trips", &trip), Err(RowError::Missing));
    store.add_row("trips", &trip).unwrap();
    assert_eq!(store.cells("trips"), Some(1));
}

fn trips_filtered(filter: Vec<Condition>) -> Ask {
    Ask {
        view: id("trips"),
        measures: vec![id("trips")],
        keys: Vec::new(),
        filter,
        from: START,
        to: END,
    }
}

#[test]
fn filter_values_are_of_their_column_type() {
    let mut store = typed(&spec());
    let fits = vec![
        condition("pay", vec![Literal::Int(1)]),
        condition("shared", vec![Literal::Bool(true)]),
        condition("pu", vec![Literal::Int(2)]),
        condition("pickup_borough", vec![lit("m")]),
    ];
    assert!(store.ask(&trips_filtered(fits)).is_ok());

    let wrong = vec![
        condition("pay", vec![lit("1"), Literal::Int(1)]),
        condition("shared", vec![Literal::Int(1)]),
        condition("pickup_borough", vec![Literal::Bool(true)]),
    ];
    let errors: Vec<String> = (store.ask(&trips_filtered(wrong)).unwrap_err().iter())
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        errors,
        [
            r#"pay is an integer, so it can't be the text "1""#,
            "shared is a boolean, so it can't be the integer 1",
            "pickup_borough is text, so it can't be the boolean true",
        ]
    );

    // An integer reads as one only at scale 0.
    let pay = trips_filtered(vec![condition("pay", vec![Literal::Int(1)])]);
    let mut types = trip_types();
    types[PAY] = ColumnType::Numeric { scale: 0 };
    store.set_types("trips", &types).unwrap();
    assert!(store.ask(&pay).is_ok());
    types[PAY] = ColumnType::Numeric { scale: 2 };
    store.set_types("trips", &types).unwrap();
    assert_eq!(
        store.ask(&pay).unwrap_err()[0].to_string(),
        "pay is a numeric of scale 2, so it can't be the integer 1"
    );
}

fn answer_scales(store: &Store, ask: &Ask) -> Vec<u32> {
    let answer = store.ask(ask).unwrap();
    answer.columns.iter().map(|column| column.scale).collect()
}

#[test]
fn types_stay_through_clear() {
    let mut store = Store::new(spec());
    let ask = Ask {
        view: id("trips"),
        measures: vec![id("trips"), id("fare"), id("tip_max")],
        keys: vec![AskKey::Key(id("pay")), AskKey::Key(id("pickup_zone"))],
        filter: Vec::new(),
        from: START,
        to: END,
    };
    assert_eq!(answer_scales(&store, &ask), [0, 0, 0, 0, 0]);
    let mut types = vec![ColumnType::Int; ROW_LEN];
    types[PAY] = ColumnType::Numeric { scale: 3 };
    types[FARE] = ColumnType::Numeric { scale: 2 };
    types[TIP] = ColumnType::Numeric { scale: 1 };
    store.set_types("trips", &types).unwrap();
    // An attribute has the type its dim column has.
    let zone = ColumnType::Numeric { scale: 4 };
    store
        .set_types("zones", &[ColumnType::Int, ColumnType::Text, zone])
        .unwrap();
    store.clear("trips");
    store.clear("zones");
    assert_eq!(answer_scales(&store, &ask), [3, 4, 0, 2, 1]);
}

#[test]
fn columns_name_keys_and_measures_as_asked() {
    let store = Store::new(spec());
    let ask = Ask {
        view: id("trips"),
        measures: vec![id("fare"), id("trips")],
        keys: vec![AskKey::Key(id("pickup_zone")), AskKey::Bucket],
        filter: Vec::new(),
        from: START,
        to: END,
    };
    let columns: Vec<(String, bool)> = (store.ask(&ask).unwrap().columns.into_iter())
        .map(|column| (column.name, column.is_time))
        .collect();
    assert_eq!(
        columns,
        [
            ("pickup_zone".to_owned(), false),
            ("bucket".to_owned(), true),
            ("fare".to_owned(), false),
            ("trips".to_owned(), false),
        ]
    );
}

#[test]
fn time_key_start() {
    let width = Width { seconds: WIDTH };
    let cases = [
        // 2024-02-29 13:47:05 UTC.
        (1_709_214_425, 1_709_214_300),
        // 1969-12-31 23:59:59, before the epoch.
        (-1, -900),
    ];
    for (time, start) in cases {
        assert_eq!(width.bucket_start(time), start, "{time}");
    }
}

#[test]
fn ask_without_by_answers_one_row() {
    let store = Store::new(spec());
    let fact = spec();
    let measures: Vec<Identifier> = fact
        .fact("trips")
        .unwrap()
        .measures
        .iter()
        .map(|measure| measure.name.clone())
        .collect();
    let ask = Ask {
        view: id("trips"),
        measures,
        keys: Vec::new(),
        filter: Vec::new(),
        from: START,
        to: END,
    };
    let zero = Value::Int(0);
    let null = Value::Null;
    assert_eq!(
        store.ask(&ask).unwrap().rows,
        [AnswerRow {
            keys: Vec::new(),
            values: vec![
                zero.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                // The five percentiles: of no values, none.
                null.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                zero.clone(),
                zero.clone(),
                zero.clone(),
                null.clone(),
                null.clone(),
                zero.clone(),
                null.clone(),
                null.clone(),
                zero.clone(),
                zero,
                null.clone(),
                null.clone(),
                null,
            ]
        }]
    );
}

#[test]
fn unknown_names() {
    let store = Store::new(spec());
    assert!(store.cells("zones").is_none());
    let ask = Ask {
        view: id("zones"),
        measures: vec![id("trips")],
        keys: Vec::new(),
        filter: Vec::new(),
        from: START,
        to: END,
    };
    assert_eq!(
        store.ask(&ask),
        Err(vec![AskError::UnknownView(id("zones"))])
    );
}

fn base_trip_with(column: usize, datum: Datum) -> Vec<Datum> {
    let mut row = trip(&mut Rng(3));
    row[column] = datum;
    row
}

#[test]
fn row_errors() {
    let base = trip(&mut Rng(3));
    let mut store = Store::new(spec());
    assert_eq!(
        store.add_row("trips", &base[1..]),
        Err(RowError::Arity {
            expected: ROW_LEN,
            found: ROW_LEN - 1
        })
    );
    assert_eq!(
        store.add_row("trips", &base_trip_with(AT, text("noon"))),
        Err(RowError::NotInt { column: id("at") })
    );
    assert_eq!(
        store.remove_row("trips", &base_trip_with(TIP, Datum::Bool(true))),
        Err(RowError::NotInt { column: id("tip") })
    );
    assert_eq!(
        store.add_row("trips", &base_trip_with(TOLLED, Datum::Int(1))),
        Err(RowError::NotBool {
            column: id("tolled")
        })
    );
    // Count distinct takes any value.
    store
        .add_row("trips", &base_trip_with(RIDER, Datum::Bool(true)))
        .unwrap();
    assert_eq!(store.remove_row("trips", &base), Err(RowError::Missing));

    let mut store = Store::new(spec());
    assert_eq!(
        store.set_types("trips", &[ColumnType::Int; ROW_LEN + 1]),
        Err(RowError::Arity {
            expected: ROW_LEN,
            found: ROW_LEN + 1
        })
    );
}

#[test]
fn each_kind_of_state_notices_a_value_it_never_saw() {
    let seen = base_trip_with(FARE, Datum::Null);
    let untolled = Datum::Bool(seen[TOLLED] != Datum::Bool(true));
    for (pos, datum) in [
        (FARE, Datum::Int(1)),
        (TIP, Datum::Int(301)),
        (RIDER, text("zed")),
        (TOLLED, untolled),
    ] {
        let mut store = Store::new(spec());
        store.add_row("trips", &seen).unwrap();
        let mut other = seen.clone();
        other[pos] = datum;
        assert_eq!(
            store.remove_row("trips", &other),
            Err(RowError::Missing),
            "{pos}"
        );
    }
}

/// A null is a value where a sum or a count reads the column, so one
/// the cell never held is noticed too.
#[test]
fn a_null_the_cell_never_held_is_noticed() {
    let mut full = base_trip_with(FARE, Datum::Int(5));
    (full[TIP], full[RIDER]) = (Datum::Int(3), text("ann"));
    for pos in [FARE, TIP, RIDER] {
        let mut store = Store::new(spec());
        store.add_row("trips", &full).unwrap();
        let mut other = full.clone();
        other[pos] = Datum::Null;
        assert_eq!(
            store.remove_row("trips", &other),
            Err(RowError::Missing),
            "a null at {pos}"
        );
        // One it does hold leaves. A store that missed a row no longer
        // matches its table, so this is another.
        let mut store = Store::new(spec());
        store.add_row("trips", &full).unwrap();
        store.add_row("trips", &other).unwrap();
        assert_eq!(store.remove_row("trips", &other), Ok(()), "a null at {pos}");
        assert_eq!(store.remove_row("trips", &full), Ok(()), "{pos}");
        assert_eq!(store.cells("trips"), Some(0), "{pos}");
    }
}

#[test]
fn row_error_display() {
    let cases = [
        (
            RowError::Arity {
                expected: 3,
                found: 1,
            },
            "row has 1 columns, expected 3",
        ),
        (
            RowError::NotInt { column: id("tip") },
            "column tip is not an integer",
        ),
        (
            RowError::NotBool {
                column: id("tolled"),
            },
            "column tolled is not a boolean",
        ),
        (RowError::Missing, "removed row was never added"),
        (RowError::Sealed, "row falls in a sealed bucket"),
    ];
    for (error, message) in cases {
        assert_eq!(error.to_string(), message);
    }
}

/// A store with two rows in one 15-minute bucket and one in the next.
fn three_rows() -> (Store, Vec<Vec<Datum>>) {
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

fn sealed_id(width: i64, bucket_start: i64) -> SealedId {
    SealedId {
        fact: id("trips"),
        width,
        bucket_start,
    }
}

fn whole_range(by: Vec<AskKey>) -> Ask {
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

fn seal_refusal(store: &mut Store, id: &SealedId) -> &'static str {
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

type Tamper<'a> = dyn Fn(&mut Sealed) + 'a;

struct SealedBucket {
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
