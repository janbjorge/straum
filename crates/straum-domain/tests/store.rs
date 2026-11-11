//! `Store` against a reference that aggregates the live rows directly
//! and joins them to the dims as they are, over random inserts, updates and
//! deletes of trips and of dim rows. Rows hold the fact's columns only; a
//! dim's values come through its projection when asked.

use std::collections::{BTreeMap, HashMap, HashSet};

mod common;

use common::{Rng, cond, dim_row, holds, id, lit, table, text};
use straum_domain::{
    Aggregate, Answer, AnswerColumn, AnswerRow, Ask, AskError, AskKey, Attribute, ByKey,
    ColumnType, Condition, Datum, Dim, Fact, Identifier, KeyPart, Literal, Measure, Origin, Ref,
    RefKind, RowError, Sealed, SealedColumn, SealedError, SealedId, Spec, Store, Time, TimeKey,
    Value, Via, Width,
};

const WIDTH: u32 = 900;
/// 2024-01-15 and 2024-04-15, 00:00 UTC: four months, one leap day.
const START: i64 = 1_705_276_800;
const END: i64 = 1_713_139_200;

// Row positions; `columns` and `rollup_key` check them.
const AT: usize = 0;
const PAY: usize = 1;
const VENDOR: usize = 2;
const SHARED: usize = 3;
const PU: usize = 4;
const DRO: usize = 5;
const FARE: usize = 6;
const TIP: usize = 7;
const DIST: usize = 8;
const RIDER: usize = 9;
const ROW_LEN: usize = 10;

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

const KEYS: [&str; 12] = [
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
    "vend_label",
];

fn col(s: &str) -> ByKey {
    ByKey::Column {
        column: id(s),
        name: id(s),
    }
}

fn attr(name: &str, column: &str) -> Attribute {
    Attribute {
        name: id(name),
        column: id(column),
    }
}

fn spec() -> Spec {
    let m = |name, aggregate, filter| Measure {
        name: id(name),
        aggregate,
        filter,
    };
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
                Width { seconds: 3_600 },
                Width { seconds: 86_400 },
                Width { seconds: 604_800 },
            ],
            origin: Origin::Timestamptz,
        },
        refs: vec![
            Ref {
                alias: id("pickup"),
                dim: id("zones"),
                via: vec![Via::Fact(id("pu"))],
                kind: RefKind::Left,
                reads: vec![
                    attr("pickup_borough", "borough"),
                    attr("pickup_zone", "zone"),
                ],
            },
            Ref {
                alias: id("dropoff"),
                dim: id("zones"),
                via: vec![Via::Fact(id("dro"))],
                kind: RefKind::Left,
                reads: vec![
                    attr("dropoff_borough", "borough"),
                    attr("dropoff_zone", "zone"),
                ],
            },
            Ref {
                alias: id("combo"),
                dim: id("combos"),
                via: vec![Via::Fact(id("pu")), Via::Fact(id("vendor"))],
                kind: RefKind::Left,
                reads: vec![attr("combo_label", "label")],
            },
            Ref {
                alias: id("vend"),
                dim: id("vendors"),
                via: vec![Via::Fact(id("vendor"))],
                kind: RefKind::Left,
                reads: vec![attr("vend_label", "label")],
            },
        ],
        by_keys: vec![
            col("pay"),
            col("vendor"),
            col("shared"),
            col("pu"),
            col("dro"),
            // Over a by column, which the row carries once.
            ByKey::IsNull {
                column: id("shared"),
                name: id("known"),
                negated: true,
            },
        ],
        filter: vec![],
        measures: vec![
            m("trips", Aggregate::Count, vec![]),
            m("fare", Aggregate::Sum(id("fare")), vec![]),
            m("tip_min", Aggregate::Min(id("tip")), vec![]),
            m("tip_max", Aggregate::Max(id("tip")), vec![]),
            // A sum over a column the load groups by.
            m("tip_avg", Aggregate::Avg(id("tip")), vec![]),
            m("distance", Aggregate::Avg(id("dist")), vec![]),
            m("riders", Aggregate::CountDistinct(id("rider")), vec![]),
            m("pickups", Aggregate::CountDistinct(id("pu")), vec![]),
            m(
                "card",
                Aggregate::Count,
                vec![cond("pay", vec![Literal::Int(1)])],
            ),
            m(
                "shared_fare",
                Aggregate::Sum(id("fare")),
                vec![cond("shared", vec![Literal::Bool(true)])],
            ),
            m(
                "mq_fare",
                Aggregate::Sum(id("fare")),
                vec![
                    cond("pickup_borough", vec![lit("m"), lit("q")]),
                    cond("vendor", vec![Literal::Int(2)]),
                ],
            ),
            m(
                "m_riders",
                Aggregate::CountDistinct(id("rider")),
                vec![cond("dropoff_borough", vec![lit("m")])],
            ),
            m(
                "far_tip",
                Aggregate::Max(id("tip")),
                vec![cond("dro", vec![Literal::Int(3), Literal::Int(6)])],
            ),
            m(
                "zone_four_distance",
                Aggregate::Avg(id("dist")),
                vec![cond("pu", vec![Literal::Int(4)])],
            ),
        ],
    };
    Spec::new(vec![zones, combos, vendors], vec![trips]).unwrap()
}

impl Rng {
    /// One of a few texts, or null `percent` percent of the time, as a
    /// dim value comes.
    fn attr(&mut self, choices: &[&str], percent: usize) -> Datum {
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
    vec![
        rng.maybe(START, END, 2),
        rng.maybe(0, 4, 10),
        rng.maybe(1, 3, 0),
        shared,
        rng.maybe(1, 7, 5),
        rng.maybe(1, 7, 5),
        rng.maybe(-500, 10_000, 5),
        rng.maybe(0, 300, 20),
        rng.maybe(0, 5_000, 5),
        rider,
    ]
}

/// Random dims over the keys trips point at: `pu` and `dro` in 1..7,
/// `vendor` in 1..3. Some keys have no row, so some joins find nothing.
fn random_dims(rng: &mut Rng) -> Dims {
    let mut dims = Dims::new();
    let zones = dims.entry("zones").or_default();
    for id in 1..7 {
        if rng.chance(75) {
            let attrs = vec![
                rng.attr(&["m", "q", "b"], 25),
                rng.attr(&["a", "b", "c"], 25),
            ];
            zones.insert(vec![Datum::Int(id)], attrs);
        }
    }
    let combos = dims.entry("combos").or_default();
    for pu in 1..7 {
        for vendor in 1..3 {
            if rng.chance(60) {
                let attrs = vec![rng.attr(&["x", "y"], 50)];
                combos.insert(vec![Datum::Int(pu), Datum::Int(vendor)], attrs);
            }
        }
    }
    let vendors = dims.entry("vendors").or_default();
    for id in 1..3 {
        vendors.insert(vec![Datum::Int(id)], vec![rng.attr(&["v1", "v2"], 50)]);
    }
    dims
}

/// Loads every dim, as a load does, rows laid end to end.
fn load_dims(store: &mut Store, dims: &Dims) {
    for (dim, rows) in dims {
        let flat: Vec<Datum> = (rows.iter())
            .flat_map(|(key, attrs)| dim_row(key, attrs))
            .collect();
        store.add_rows(dim, &flat).unwrap();
    }
}

/// A random change to one dim, applied to the store and to `dims`: a row
/// relabelled, removed or added.
fn change_dim(rng: &mut Rng, store: &mut Store, dims: &mut Dims) {
    let (dim, key, attrs): (&'static str, Vec<Datum>, Vec<Datum>) = match rng.below(3) {
        0 => (
            "zones",
            vec![Datum::Int(rng.int(1, 7))],
            vec![
                rng.attr(&["m", "q", "b"], 25),
                rng.attr(&["a", "b", "c"], 25),
            ],
        ),
        1 => (
            "combos",
            vec![Datum::Int(rng.int(1, 7)), Datum::Int(rng.int(1, 3))],
            vec![rng.attr(&["x", "y"], 50)],
        ),
        _ => (
            "vendors",
            vec![Datum::Int(rng.int(1, 3))],
            vec![rng.attr(&["v1", "v2"], 50)],
        ),
    };
    let rows = dims.get_mut(dim).unwrap();
    if let Some(old) = rows.remove(&key) {
        store.remove_row(dim, &dim_row(&key, &old)).unwrap();
        if rng.chance(40) {
            return;
        }
    }
    store.add_row(dim, &dim_row(&key, &attrs)).unwrap();
    rows.insert(key, attrs);
}

fn random_ask(rng: &mut Rng, spec: &Spec) -> Ask {
    let mut by: Vec<AskKey> = KEYS
        .iter()
        .filter(|_| rng.chance(25))
        .map(|k| AskKey::Key(id(k)))
        .collect();
    if rng.chance(60) {
        let at = rng.below(by.len() + 1);
        by.insert(at, AskKey::Time(TimeKey::Bucket));
    }
    let pool = [
        cond("pay", vec![Literal::Int(1), Literal::Int(2)]),
        cond("vendor", vec![Literal::Int(2)]),
        cond("shared", vec![Literal::Bool(false)]),
        cond("pickup_borough", vec![lit("m")]),
        cond("dro", vec![Literal::Int(5)]),
        cond("dropoff_zone", vec![lit("a"), lit("c")]),
        cond("pu", vec![Literal::Int(1), Literal::Int(3)]),
        cond("combo_label", vec![lit("x")]),
        cond("known", vec![Literal::Bool(true)]),
        cond("vend_label", vec![lit("v1")]),
    ];
    let filter = pool.into_iter().filter(|_| rng.chance(15)).collect();
    let all = &spec.fact("trips").unwrap().measures;
    let mut measures: Vec<Identifier> = all
        .iter()
        .filter(|_| rng.chance(50))
        .map(|m| m.name.clone())
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

/// A key's value for a row: a column, the null-ness key, or a dim value
/// joined as the dims are now, null when the join finds no row.
fn value(row: &[Datum], key: &Identifier, dims: &Dims) -> Datum {
    let joined = |dim: &str, key: Vec<Datum>, attr: usize| -> Datum {
        if key.contains(&Datum::Null) {
            return Datum::Null;
        }
        dims[dim]
            .get(&key)
            .map_or(Datum::Null, |attrs| attrs[attr].clone())
    };
    match key.as_str() {
        "known" => Datum::Bool(row[SHARED] != Datum::Null),
        "pickup_borough" => joined("zones", vec![row[PU].clone()], 0),
        "pickup_zone" => joined("zones", vec![row[PU].clone()], 1),
        "dropoff_borough" => joined("zones", vec![row[DRO].clone()], 0),
        "dropoff_zone" => joined("zones", vec![row[DRO].clone()], 1),
        "combo_label" => joined("combos", vec![row[PU].clone(), row[VENDOR].clone()], 0),
        "vend_label" => joined("vendors", vec![row[VENDOR].clone()], 0),
        name => row[column(name)].clone(),
    }
}

/// The column types `run` sets.
fn trip_types() -> Vec<ColumnType> {
    let numeric = |column: usize| ColumnType::Numeric {
        scale: TRIP_SCALES + column as u32,
    };
    let types = vec![
        ColumnType::Time { zoned: true },
        ColumnType::Int,
        ColumnType::Int,
        ColumnType::Bool,
        ColumnType::Int,
        ColumnType::Int,
        numeric(FARE),
        numeric(TIP),
        numeric(DIST),
        ColumnType::Text,
    ];
    assert_eq!(types.len(), ROW_LEN);
    types
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
        "known" | "pickup_borough" | "pickup_zone" | "dropoff_borough" | "dropoff_zone"
        | "combo_label" | "vend_label" => 0,
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
        other => panic!("no column {other}"),
    }
}

fn passes(row: &[Datum], conds: &[Condition], dims: &Dims) -> bool {
    conds
        .iter()
        .all(|c| holds(&value(row, &c.key, dims), &c.values))
}

/// The reference answer: the ask over the raw rows joined to the dims: no rollup, no buckets.
fn reference(spec: &Spec, ask: &Ask, rows: &[Vec<Datum>], dims: &Dims) -> Answer {
    let fact = spec.fact("trips").unwrap();
    let mut groups: BTreeMap<Vec<Datum>, Vec<&[Datum]>> = BTreeMap::new();
    if ask.keys.is_empty() {
        groups.insert(Vec::new(), Vec::new());
    }
    for row in rows {
        let Datum::Int(t) = row[AT] else { continue };
        if t < ask.from || t >= ask.to || !passes(row, &ask.filter, dims) {
            continue;
        }
        let key = ask
            .keys
            .iter()
            .map(|k| match k {
                AskKey::Key(p) => value(row, p, dims),
                AskKey::Time(tk) => Datum::Int(tk.bucket_start(t, Width { seconds: WIDTH })),
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
    let columns = (ask.keys.iter().map(|k| match k {
        AskKey::Key(p) => AnswerColumn {
            name: p.to_string(),
            scale: scale(p),
            is_time: false,
        },
        AskKey::Time(t) => AnswerColumn {
            name: t.to_string(),
            scale: 0,
            is_time: true,
        },
    }))
    .chain(ask.measures.iter().map(|name| AnswerColumn {
        name: name.to_string(),
        scale: match &fact.measure(name.as_str()).unwrap().aggregate {
            Aggregate::Count | Aggregate::CountDistinct(_) => 0,
            Aggregate::Sum(c) | Aggregate::Min(c) | Aggregate::Max(c) | Aggregate::Avg(c) => {
                trip_types()[column(c.as_str())].scale()
            }
        },
        is_time: false,
    }))
    .collect();
    Answer { columns, rows }
}

fn measure(m: &Measure, rows: &[&[Datum]], dims: &Dims) -> Value {
    let rows: Vec<_> = rows.iter().filter(|r| passes(r, &m.filter, dims)).collect();
    let ints = |c: &Identifier| -> Vec<i128> {
        rows.iter()
            .filter_map(|r| match r[column(c.as_str())] {
                Datum::Int(v) => Some(i128::from(v)),
                _ => None,
            })
            .collect()
    };
    let or_null = |v: Option<i128>| v.map_or(Value::Null, Value::Int);
    match &m.aggregate {
        Aggregate::Count => Value::Int(rows.len() as i128),
        Aggregate::Sum(c) => {
            let v = ints(c);
            or_null((!v.is_empty()).then(|| v.iter().sum()))
        }
        Aggregate::Avg(c) => {
            let v = ints(c);
            if v.is_empty() {
                Value::Null
            } else {
                Value::Avg {
                    sum: v.iter().sum(),
                    count: v.len() as u64,
                }
            }
        }
        Aggregate::Min(c) => or_null(ints(c).into_iter().min()),
        Aggregate::Max(c) => or_null(ints(c).into_iter().max()),
        Aggregate::CountDistinct(c) => {
            let seen: HashSet<_> = rows
                .iter()
                .map(|r| &r[column(c.as_str())])
                .filter(|d| **d != Datum::Null)
                .collect();
            Value::Int(seen.len() as i128)
        }
    }
}

/// Sealed buckets kept aside, as a spill keeps them, when `run` seals.
#[derive(Default)]
struct Spilled {
    buckets: HashMap<SealedId, Sealed>,
    /// Sealed buckets asks read.
    read: usize,
}

impl Spilled {
    /// Seals every bucket that ends by `before`, or some of them.
    fn seal(&mut self, rng: &mut Rng, store: &mut Store, before: i64) {
        for id in store.sealable(before) {
            if rng.chance(70) {
                let sealed = store.sealed_copy(&id).unwrap();
                store.seal(&id).unwrap();
                self.buckets.insert(id, sealed);
            }
        }
    }

    /// Unseals the buckets a row falls in, as the writer does before it
    /// applies a change.
    fn unseal(&mut self, store: &mut Store, row: &[Datum]) {
        for id in store.sealed_touched("trips", row) {
            let thawed = store.thaw(self.buckets.remove(&id).unwrap()).unwrap();
            store.unseal(thawed).unwrap();
        }
    }

    fn ask(&mut self, store: &Store, ask: &Ask) -> Answer {
        let thawed: Vec<_> = (store.sealed_read(ask).unwrap().iter())
            .map(|id| store.thaw(self.buckets[id].clone()).unwrap())
            .collect();
        self.read += thawed.len();
        store.ask_with(ask, &thawed).unwrap()
    }
}

/// Random changes and asks against the reference. With `sealing`,
/// buckets are sealed and unsealed at random as they go, and the answers
/// must not change; returns the sealed buckets asks read. With
/// `changing`, dims change as they go, so the bound cells go stale and
/// the keyed cells answer; without, the bound cells answer throughout.
fn run(seed: u64, sealing: bool, changing: bool) -> usize {
    let spec = spec();
    let mut store = typed(&spec);
    let mut rng = Rng(seed);
    let mut rows: Vec<Vec<Datum>> = Vec::new();
    let mut dims = random_dims(&mut rng);
    let mut spilled = Spilled::default();
    load_dims(&mut store, &dims);

    for step in 0..2_000 {
        if sealing && rng.chance(5) {
            let before = rng.int(START, END + 7 * 86_400);
            spilled.seal(&mut rng, &mut store, before);
        }
        match rng.below(40) {
            0..=3 if changing => change_dim(&mut rng, &mut store, &mut dims),
            4..=8 if !rows.is_empty() => {
                let row = rows.swap_remove(rng.below(rows.len()));
                spilled.unseal(&mut store, &row);
                store.remove_row("trips", &row).unwrap();
            }
            9..=14 if !rows.is_empty() => {
                // An update: the old row out, a new one in, often in the
                // same cell.
                let i = rng.below(rows.len());
                let mut new = trip(&mut rng);
                if rng.chance(50) {
                    new[..=DRO].clone_from_slice(&rows[i][..=DRO]);
                }
                spilled.unseal(&mut store, &rows[i]);
                store.remove_row("trips", &rows[i]).unwrap();
                spilled.unseal(&mut store, &new);
                store.add_row("trips", &new).unwrap();
                rows[i] = new;
            }
            _ => {
                let mut row = trip(&mut rng);
                // Often into a cell that already holds rows.
                if !rows.is_empty() && rng.chance(40) {
                    let other = rng.below(rows.len());
                    row[..=DRO].clone_from_slice(&rows[other][..=DRO]);
                }
                spilled.unseal(&mut store, &row);
                store.add_row("trips", &row).unwrap();
                rows.push(row);
            }
        }
        if step % 100 == 99 {
            assert_eq!(store.bound_cells("trips").is_some(), !changing || step < 99);
            for _ in 0..10 {
                let ask = random_ask(&mut rng, &spec);
                assert_eq!(
                    spilled.ask(&store, &ask),
                    reference(&spec, &ask, &rows, &dims),
                    "seed {seed}, step {step}, {ask:?}"
                );
            }
        }
    }

    assert!(store.cells("trips").unwrap() > 0);
    for row in rows {
        spilled.unseal(&mut store, &row);
        store.remove_row("trips", &row).unwrap();
    }
    assert_eq!(
        store.cells("trips").unwrap(),
        0,
        "seed {seed}: a cell outlived its rows"
    );
    spilled.read
}

#[test]
fn store_matches_naive_reference() {
    for seed in 0..16 {
        assert_eq!(run(seed, false, true), 0);
    }
}

#[test]
fn sealed_store_matches_naive_reference() {
    let read: usize = (0..16).map(|seed| run(seed, true, true)).sum();
    assert!(read > 1_000, "asks read {read} sealed buckets");
}

#[test]
fn bound_store_matches_naive_reference() {
    for seed in 0..16 {
        assert_eq!(run(seed, false, false), 0);
    }
}

/// A sealed bucket has no bound cells, so an ask over one reads the keyed
/// cells, thawed; the rest read the bound cells.
#[test]
fn sealed_bound_store_matches_naive_reference() {
    let read: usize = (0..16).map(|seed| run(seed, true, false)).sum();
    assert!(read > 1_000, "asks read {read} sealed buckets");
}

/// Trips by pay, joined to zones for the borough, or with no join at
/// all.
fn zones_spec(joined: bool) -> Spec {
    let zones = Dim {
        as_text: Vec::new(),
        name: id("zones"),
        table: table("zones"),
        key: vec![id("id")],
        attributes: vec![id("borough")],
    };
    let pickup = Ref {
        alias: id("pickup"),
        dim: id("zones"),
        via: vec![Via::Fact(id("pu"))],
        kind: RefKind::Left,
        reads: vec![attr("borough", "borough")],
    };
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
        refs: if joined { vec![pickup] } else { vec![] },
        by_keys: vec![col("pay")],
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
    let mut store = Store::new(zones_spec(true));
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
    Ask {
        view: id("trips"),
        measures: vec![id("trips")],
        keys: vec![AskKey::Key(id("borough"))],
        filter: Vec::new(),
        from: START,
        to: END,
    }
}

/// The bound cells are keyed by the dim's column, so the zones' boroughs
/// make fewer of them than the zone ids make keyed cells, and an ask
/// reads the fewer. Once a zone changes they are gone, and the ask reads
/// the keyed cells through the projection, to the same answer.
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
    assert_eq!(store.bound_cells("trips"), None);
    assert_eq!(store.cells_read(&by_borough), Ok(6));
    let moved = store.ask(&by_borough).unwrap();
    assert_eq!(answer.rows[0].values, [Value::Int(3)]);
    assert_eq!(moved.rows[0].values, [Value::Int(4)]);
    assert_eq!(moved.columns, answer.columns);
}

/// A fact without refs has nothing to bind: its keyed cells are its
/// answer.
#[test]
fn a_fact_without_refs_keeps_no_bound_cells() {
    let mut store = Store::new(zones_spec(false));
    // Without the join, a row carries no zone.
    store
        .add_row("trips", &[Datum::Int(START), Datum::Int(1)])
        .unwrap();
    assert_eq!(store.cells("trips"), Some(1));
    assert_eq!(store.bound_cells("trips"), None);
}

/// A dim change under a fact with no rows leaves nothing bound wrong, so
/// the bound cells stay and bind the rows that come after it.
#[test]
fn a_dim_change_under_no_rows_keeps_the_bound_cells() {
    let mut store = Store::new(zones_spec(true));
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

/// Sealing a bucket drops its bound cells for good: once the keyed cells
/// are back, a row of the bucket goes into them alone, and an ask over
/// the bucket reads them, 6 cells for 2, to the same answer.
#[test]
fn a_sealed_bucket_is_read_through_its_keyed_cells() {
    let mut store = zoned_store();
    let by_borough = by_borough();
    let id = store.sealable(START + i64::from(WIDTH)).remove(0);
    let copy = store.sealed_copy(&id).unwrap();
    store.seal(&id).unwrap();
    assert_eq!(store.sealed_read(&by_borough), Ok(vec![id.clone()]));
    let thawed = store.thaw(copy).unwrap();
    store.unseal(thawed).unwrap();
    assert_eq!(store.sealed_read(&by_borough), Ok(vec![]));

    store
        .add_row("trips", &[Datum::Int(START), Datum::Int(1), Datum::Int(1)])
        .unwrap();
    assert_eq!(store.cells_read(&by_borough), Ok(6));
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
    for i in 0..4 {
        let mut row = trip(&mut rng);
        row[AT] = Datum::Int(START + i64::from(WIDTH) * i);
        store.add_row("trips", &row).unwrap();
        rows.push(row);
    }
    let ask = |from, to| Ask {
        view: id("trips"),
        measures: vec![id("trips")],
        keys: Vec::new(),
        filter: Vec::new(),
        from,
        to,
    };
    let trips = |ask: &Ask| store.ask(ask).unwrap().rows[0].values[0].clone();
    let count = |n: i128| Value::Int(n);
    let width = i64::from(WIDTH);
    assert_eq!(trips(&ask(START - width, END + width)), count(4));
    assert_eq!(trips(&ask(START + width, END)), count(3));
    assert_eq!(trips(&ask(START, START + width)), count(1));
    assert_eq!(trips(&ask(START - width, START)), count(0));
    assert_eq!(trips(&ask(END, END + width)), count(0));
    let empty = typed(&spec);
    assert_eq!(
        empty.ask(&ask(START, END)).unwrap().rows[0].values[0],
        count(0)
    );
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
    let labels = |store: &Store| -> Vec<Datum> {
        (store.ask(&by_label).unwrap().rows.into_iter())
            .map(|row| row.keys[0].clone())
            .collect()
    };
    assert_eq!(labels(&store), [text("v1")]);
    let cells = store.cells("trips");
    assert_eq!(store.bound_cells("trips"), cells);

    store
        .remove_row("vendors", &[Datum::Int(1), text("v1")])
        .unwrap();
    assert_eq!(store.bound_cells("trips"), None);
    assert_eq!(labels(&store), [Datum::Null]);
    store
        .add_row("vendors", &[Datum::Int(1), text("v2")])
        .unwrap();
    assert_eq!(labels(&store), [text("v2")]);
    assert_eq!(store.cells("trips"), cells);
    // Another vendor's row changes nothing here.
    store
        .add_row("vendors", &[Datum::Int(2), text("v9")])
        .unwrap();
    assert_eq!(labels(&store), [text("v2")]);
    store.clear("vendors");
    assert_eq!(labels(&store), [Datum::Null]);
    assert_eq!(store.dim_rows("vendors"), Some(0));
    // The fact row comes out as it went in, whatever the dim did.
    store.remove_row("trips", &row).unwrap();
    assert_eq!(store.cells("trips"), Some(0));
}

/// An update of a dim row that reads the same changed no column the
/// views read, so the bound cells stay; one that reads otherwise drops
/// them, and the fact's rows then load and change through the keyed
/// cells alone.
#[test]
fn a_dim_update_drops_the_bound_cells_only_when_it_reads_otherwise() {
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
    assert_eq!(store.bound_cells("trips"), None);

    let mut other = trip(&mut Rng(6));
    other[AT] = Datum::Int(START + i64::from(WIDTH));
    store.update_row("trips", &row, &other).unwrap();
    assert_eq!(store.cells("trips"), Some(1));
    let flat: Vec<Datum> = [row.clone(), other.clone()].concat();
    store.add_rows("trips", &flat).unwrap();
    assert_eq!(store.cells("trips"), Some(2));
    assert_eq!(store.bound_cells("trips"), None);
}

/// A dim keyed by several columns matches on all of them; a via that is
/// null joins nothing, even when a dim row has a null there.
#[test]
fn a_composite_key_matches_every_column() {
    let spec = spec();
    let mut store = typed(&spec);
    store
        .add_rows(
            "combos",
            &[
                Datum::Int(1),
                Datum::Int(1),
                text("x"),
                Datum::Int(1),
                Datum::Int(2),
                text("y"),
                Datum::Null,
                Datum::Int(2),
                text("z"),
            ],
        )
        .unwrap();
    // A row with a null in its key is never held: nothing could join it.
    assert_eq!(store.dim_rows("combos"), Some(2));
    let mut by_label = Ask {
        view: id("trips"),
        measures: vec![id("trips")],
        keys: vec![AskKey::Key(id("combo_label"))],
        filter: Vec::new(),
        from: START,
        to: END,
    };
    let mut rng = Rng(4);
    for (pu, vendor, label) in [
        (Datum::Int(1), Datum::Int(1), text("x")),
        (Datum::Int(1), Datum::Int(2), text("y")),
        (Datum::Int(2), Datum::Int(1), Datum::Null),
        (Datum::Null, Datum::Int(2), Datum::Null),
    ] {
        let mut store = typed(&spec);
        store
            .add_rows(
                "combos",
                &[
                    Datum::Int(1),
                    Datum::Int(1),
                    text("x"),
                    Datum::Int(1),
                    Datum::Int(2),
                    text("y"),
                ],
            )
            .unwrap();
        let mut row = trip(&mut rng);
        row[AT] = Datum::Int(START);
        row[PU] = pu;
        row[VENDOR] = vendor;
        store.add_row("trips", &row).unwrap();
        let answer = store.ask(&by_label).unwrap();
        assert_eq!(answer.rows[0].keys, [label]);
    }
    // Filtered on the attribute too.
    by_label.filter = vec![cond("combo_label", vec![lit("x")])];
    by_label.keys.clear();
    let mut row = trip(&mut rng);
    row[AT] = Datum::Int(START);
    row[PU] = Datum::Int(1);
    row[VENDOR] = Datum::Int(1);
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
    let fare = |store: &Store| store.ask(&mq_fare).unwrap().rows[0].values[0].clone();
    assert_eq!(fare(&store), Value::Int(100));
    store
        .remove_row("zones", &[Datum::Int(2), text("m"), text("a")])
        .unwrap();
    store
        .add_row("zones", &[Datum::Int(2), text("b"), text("a")])
        .unwrap();
    assert_eq!(fare(&store), Value::Null);
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
        keys: KEYS.iter().map(|k| AskKey::Key(id(k))).collect(),
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

#[test]
fn dim_rows_added_together_match_rows_added_one_by_one() {
    let spec = spec();
    let mut rng = Rng(2);
    let dims = random_dims(&mut rng);
    let mut together = typed(&spec);
    load_dims(&mut together, &dims);
    let mut by_row = typed(&spec);
    for (dim, rows) in &dims {
        for (key, attrs) in rows {
            by_row.add_row(dim, &dim_row(key, attrs)).unwrap();
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
        (FARE, "fare"),
        (TIP, "tip"),
        (DIST, "dist"),
        (RIDER, "rider"),
    ];
    assert_eq!(columns, expected.map(|(_, c)| c.to_owned()));
    for (pos, name) in expected {
        assert_eq!(columns[pos], name);
    }
}

/// The refs' via columns follow the by keys in the rollup key, each slot
/// once: here every via is a by column already.
#[test]
fn rollup_key_puts_via_columns_after_the_by_keys_each_once() {
    let spec = spec();
    let key: Vec<String> = (spec.fact("trips").unwrap().rollup_key())
        .map(|slot| match slot {
            KeyPart::Column(c) => c.to_string(),
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
    let extremes = |store: &Store| store.ask(&ask).unwrap().rows[0].values.clone();
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
    assert_eq!(extremes(&store), [Value::Int(2), Value::Int(9)]);

    // One of two 9s out: the max stays.
    store.remove_row("trips", &rows[1]).unwrap();
    assert_eq!(extremes(&store), [Value::Int(2), Value::Int(9)]);
    store.remove_row("trips", &rows[2]).unwrap();
    assert_eq!(extremes(&store), [Value::Int(2), Value::Int(7)]);
    store.remove_row("trips", &rows[3]).unwrap();
    assert_eq!(extremes(&store), [Value::Int(5), Value::Int(7)]);
    // A value past the old extremes after they went.
    row[TIP] = Datum::Int(1);
    store.add_row("trips", &row).unwrap();
    assert_eq!(extremes(&store), [Value::Int(1), Value::Int(7)]);
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

#[test]
fn filter_values_are_of_their_column_type() {
    let mut store = typed(&spec());
    let ask = |filter| Ask {
        view: id("trips"),
        measures: vec![id("trips")],
        keys: Vec::new(),
        filter,
        from: START,
        to: END,
    };
    let fits = vec![
        cond("pay", vec![Literal::Int(1)]),
        cond("shared", vec![Literal::Bool(true)]),
        cond("pu", vec![Literal::Int(2)]),
        cond("pickup_borough", vec![lit("m")]),
    ];
    assert!(store.ask(&ask(fits)).is_ok());

    let wrong = vec![
        cond("pay", vec![lit("1"), Literal::Int(1)]),
        cond("shared", vec![Literal::Int(1)]),
        cond("pickup_borough", vec![Literal::Bool(true)]),
    ];
    let errors: Vec<String> = (store.ask(&ask(wrong)).unwrap_err().iter())
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
    let pay = || ask(vec![cond("pay", vec![Literal::Int(1)])]);
    let mut types = trip_types();
    types[PAY] = ColumnType::Numeric { scale: 0 };
    store.set_types("trips", &types).unwrap();
    assert!(store.ask(&pay()).is_ok());
    types[PAY] = ColumnType::Numeric { scale: 2 };
    store.set_types("trips", &types).unwrap();
    assert_eq!(
        store.ask(&pay()).unwrap_err()[0].to_string(),
        "pay is a numeric of scale 2, so it can't be the integer 1"
    );
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
    let scales = |store: &Store| -> Vec<u32> {
        let answer = store.ask(&ask).unwrap();
        answer.columns.iter().map(|c| c.scale).collect()
    };
    assert_eq!(scales(&store), [0, 0, 0, 0, 0]);
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
    assert_eq!(scales(&store), [3, 4, 0, 2, 1]);
}

#[test]
fn columns_name_keys_and_measures_as_asked() {
    let store = Store::new(spec());
    let ask = Ask {
        view: id("trips"),
        measures: vec![id("fare"), id("trips")],
        keys: vec![
            AskKey::Key(id("pickup_zone")),
            AskKey::Time(TimeKey::Bucket),
        ],
        filter: Vec::new(),
        from: START,
        to: END,
    };
    let columns: Vec<(String, bool)> = (store.ask(&ask).unwrap().columns.into_iter())
        .map(|c| (c.name, c.is_time))
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
    for (t, start) in cases {
        assert_eq!(TimeKey::Bucket.bucket_start(t, width), start, "{t}");
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
        .map(|m| m.name.clone())
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
                null.clone(),
                null.clone(),
                zero.clone(),
                zero.clone(),
                zero.clone(),
                null.clone(),
                null.clone(),
                zero,
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

#[test]
fn row_errors() {
    let base = trip(&mut Rng(3));
    let fresh = || Store::new(spec());
    let with = |pos: usize, v: Datum| {
        let mut row = base.clone();
        row[pos] = v;
        row
    };

    let mut store = fresh();
    assert_eq!(
        store.add_row("trips", &base[1..]),
        Err(RowError::Arity {
            expected: ROW_LEN,
            found: ROW_LEN - 1
        })
    );
    assert_eq!(
        store.add_row("trips", &with(AT, text("noon"))),
        Err(RowError::NotInt { column: id("at") })
    );
    assert_eq!(
        store.remove_row("trips", &with(TIP, Datum::Bool(true))),
        Err(RowError::NotInt { column: id("tip") })
    );
    // Count distinct takes any value.
    store
        .add_row("trips", &with(RIDER, Datum::Bool(true)))
        .unwrap();
    assert_eq!(store.remove_row("trips", &base), Err(RowError::Missing));

    // Each kind of state notices a value it never saw.
    let seen = with(FARE, Datum::Null);
    for (pos, v) in [
        (FARE, Datum::Int(1)),
        (TIP, Datum::Int(301)),
        (RIDER, text("zed")),
    ] {
        let mut store = fresh();
        store.add_row("trips", &seen).unwrap();
        let mut other = seen.clone();
        other[pos] = v;
        assert_eq!(
            store.remove_row("trips", &other),
            Err(RowError::Missing),
            "{pos}"
        );
    }

    let mut store = fresh();
    assert_eq!(
        store.set_types("trips", &[ColumnType::Int; ROW_LEN + 1]),
        Err(RowError::Arity {
            expected: ROW_LEN,
            found: ROW_LEN + 1
        })
    );
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
        (RowError::Missing, "removed row was never added"),
        (RowError::Sealed, "row falls in a sealed bucket"),
    ];
    for (err, msg) in cases {
        assert_eq!(err.to_string(), msg);
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
        to: START + 604_800,
    }
}

#[test]
fn a_sealed_bucket_answers_as_before() {
    let (mut store, _) = three_rows();
    let ask = whole_range(vec![AskKey::Time(TimeKey::Bucket)]);
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

    store.unseal(store.thaw(sealed).unwrap()).unwrap();
    assert_eq!(store.sealed_read(&ask).unwrap(), []);
    assert_eq!(store.ask(&ask).unwrap(), before);
}

#[test]
fn sealable_buckets_end_by_the_cutoff() {
    let (mut store, _) = three_rows();
    let width = i64::from(WIDTH);
    assert_eq!(store.sealable(START + width - 1), []);
    assert_eq!(store.sealable(START + width), [sealed_id(width, START)]);
    let all = store.sealable(START + 604_800 * 2);
    // The two buckets of 15 minutes; wider widths stay in memory.
    assert_eq!(
        all,
        [sealed_id(width, START), sealed_id(width, START + width)]
    );
    for id in &all {
        store.seal(id).unwrap();
    }
    assert_eq!(store.sealable(START + 604_800 * 2), []);
    assert_eq!(store.sealed("trips"), all);
    store.clear("trips");
    assert_eq!(store.sealed("trips"), []);
    // Gone with the truncate, or never there.
    let mut reason = |id: &SealedId| store.seal(id).unwrap_err().reason;
    assert_eq!(reason(&all[0]), "no such bucket");
    assert_eq!(reason(&sealed_id(width, START - width)), "no such bucket");
    assert_eq!(reason(&sealed_id(7, START)), "no such width");
    let nope = SealedId {
        fact: id("nope"),
        ..all[0].clone()
    };
    assert_eq!(reason(&nope), "unknown fact");
    assert_eq!(store.sealed_copy(&all[0]), None);
}

#[test]
fn a_row_in_a_sealed_bucket_waits_for_unseal() {
    let (mut store, rows) = three_rows();
    let id = sealed_id(86_400, START);
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
    later[AT] = Datum::Int(START + 86_400);
    assert_eq!(store.sealed_touched("trips", &later), []);
    store.add_row("trips", &later).unwrap();

    store.unseal(store.thaw(sealed).unwrap()).unwrap();
    assert_eq!(store.sealed_touched("trips", &rows[2]), []);
    store.remove_row("trips", &rows[0]).unwrap();
    store.add_row("trips", &rows[0]).unwrap();
}

type Change<'a> = dyn Fn(&mut Sealed) + 'a;

#[test]
fn thaw_refuses_a_bucket_that_does_not_fit() {
    let (mut store, _) = three_rows();
    let bucket = sealed_id(i64::from(WIDTH), START);
    let good = store.sealed_copy(&bucket).unwrap();
    store.seal(&bucket).unwrap();
    let refused = |change: &Change<'_>| {
        let mut sealed = good.clone();
        change(&mut sealed);
        store.thaw(sealed).unwrap_err().reason
    };
    let extent = good
        .columns
        .iter()
        .position(|c| matches!(c, SealedColumn::Extent { .. }))
        .unwrap();
    let distinct = good
        .columns
        .iter()
        .position(|c| matches!(c, SealedColumn::Distinct { .. }))
        .unwrap();
    let cases: [(&str, &Change<'_>); 13] = [
        ("unknown fact", &|s| s.id.fact = id("nope")),
        ("no such width", &|s| s.id.width = 7),
        ("no cells", &|s| s.counts.clear()),
        ("a cell without rows", &|s| s.counts[0] = 0),
        ("keys don't fit the rollup key", &|s| {
            s.keys.pop();
        }),
        ("a code the store never gave", &|s| s.keys[0] = 99),
        ("columns don't fit the measures", &|s| {
            s.columns.pop();
        }),
        ("columns don't fit the measures", &|s| {
            s.columns.swap(0, extent);
        }),
        ("columns don't fit the measures", &|s| {
            s.columns[0] = SealedColumn::Sum {
                sums: Vec::new(),
                counts: vec![0],
            };
        }),
        ("columns don't fit the measures", &|s| {
            s.columns[extent] = SealedColumn::Extent {
                values: vec![vec![(1, 0)]],
            };
        }),
        ("columns don't fit the measures", &|s| {
            s.columns[extent] = SealedColumn::Extent { values: vec![] };
        }),
        ("columns don't fit the measures", &|s| {
            s.columns[distinct] = SealedColumn::Distinct {
                values: vec![vec![(Datum::Null, 0)]],
            };
        }),
        ("columns don't fit the measures", &|s| {
            s.columns[distinct] = SealedColumn::Distinct { values: vec![] };
        }),
    ];
    for (reason, change) in cases {
        assert_eq!(refused(change), reason);
    }
    assert!(store.thaw(good).is_ok());
}

#[test]
fn unseal_refuses_a_bucket_not_sealed_or_keyed_twice() {
    let (mut store, _) = three_rows();
    let id = sealed_id(86_400, START);
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
            SealedColumn::Sum { sums, counts } => {
                sums.extend_from_slice(&sums.clone());
                counts.extend_from_slice(&counts.clone());
            }
            SealedColumn::Extent { values } => values.extend_from_slice(&values.clone()),
            SealedColumn::Distinct { values } => values.extend_from_slice(&values.clone()),
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
    elsewhere[AT] = Datum::Int(START + 86_400);
    assert_eq!(
        store.remove_row("trips", &elsewhere),
        Err(RowError::Missing)
    );
}
