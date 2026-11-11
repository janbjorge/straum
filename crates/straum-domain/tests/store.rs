//! `Store` against a naive reference that aggregates the live rows directly
//! and joins them to the dims as they are, over random inserts, updates and
//! deletes of trips and of dim rows. Rows hold the fact's columns only; a
//! dim's values come through its projection when asked.

use std::collections::{BTreeMap, HashSet};

use straum_domain::{
    Agg, Answer, AnswerColumn, AnswerRow, Ask, AskError, AskKey, Attr, By, ColumnType, Cond, Datum,
    Dim, Fact, Grain, Ident, Literal, Measure, Origin, Ref, RefKind, RowError, Slot, Spec, Store,
    Table, Time, TimeKey, Value, Via,
};

const GRAIN: u32 = 900;
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
const WIDTH: usize = 10;

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

fn id(s: &str) -> Ident {
    Ident::new(s).unwrap()
}

fn key(s: &str) -> Ident {
    id(s)
}

fn col(s: &str) -> By {
    By::Column {
        column: id(s),
        name: id(s),
    }
}

fn attr(name: &str, column: &str) -> Attr {
    Attr {
        name: id(name),
        column: id(column),
    }
}

fn text(s: &str) -> Datum {
    Datum::Text(s.into())
}

fn lit(s: &str) -> Literal {
    Literal::Text(s.into())
}

fn cond(k: &str, values: Vec<Literal>) -> Cond {
    Cond {
        key: key(k),
        values,
    }
}

fn spec() -> Spec {
    let table = |name| Table {
        schema: id("public"),
        name: id(name),
    };
    let m = |name, agg, filter| Measure {
        name: id(name),
        agg,
        filter,
    };
    let zones = Dim {
        name: id("zones"),
        table: table("zones"),
        key: vec![id("id")],
        attrs: vec![id("borough"), id("zone")],
    };
    // Keyed by two fact columns, one of them a by column asks group by.
    let combos = Dim {
        name: id("combos"),
        table: table("combos"),
        key: vec![id("pu_id"), id("vendor_id")],
        attrs: vec![id("label")],
    };
    let vendors = Dim {
        name: id("vendors"),
        table: table("vendors"),
        key: vec![id("id")],
        attrs: vec![id("label")],
    };
    let trips = Fact {
        name: id("trips"),
        table: table("trips"),
        time: Time {
            column: id("at"),
            grain: Grain(GRAIN),
            // Hours, days and weeks, so random ranges read every width.
            buckets: vec![Grain(3_600), Grain(86_400), Grain(604_800)],
            origin: Origin::Timestamptz,
        },
        refs: vec![
            Ref {
                name: id("pickup"),
                dim: id("zones"),
                via: vec![Via::Fact(id("pu"))],
                kind: RefKind::Left,
                attrs: vec![
                    attr("pickup_borough", "borough"),
                    attr("pickup_zone", "zone"),
                ],
            },
            Ref {
                name: id("dropoff"),
                dim: id("zones"),
                via: vec![Via::Fact(id("dro"))],
                kind: RefKind::Left,
                attrs: vec![
                    attr("dropoff_borough", "borough"),
                    attr("dropoff_zone", "zone"),
                ],
            },
            Ref {
                name: id("combo"),
                dim: id("combos"),
                via: vec![Via::Fact(id("pu")), Via::Fact(id("vendor"))],
                kind: RefKind::Left,
                attrs: vec![attr("combo_label", "label")],
            },
            Ref {
                name: id("vend"),
                dim: id("vendors"),
                via: vec![Via::Fact(id("vendor"))],
                kind: RefKind::Left,
                attrs: vec![attr("vend_label", "label")],
            },
        ],
        by: vec![
            col("pay"),
            col("vendor"),
            col("shared"),
            col("pu"),
            col("dro"),
            // Over a by column, which the row carries once.
            By::IsNull {
                column: id("shared"),
                name: id("known"),
                negated: true,
            },
        ],
        filter: vec![],
        measures: vec![
            m("trips", Agg::Count, vec![]),
            m("fare", Agg::Sum(id("fare")), vec![]),
            m("tip_min", Agg::Min(id("tip")), vec![]),
            m("tip_max", Agg::Max(id("tip")), vec![]),
            // A sum over a column the load groups by.
            m("tip_avg", Agg::Avg(id("tip")), vec![]),
            m("distance", Agg::Avg(id("dist")), vec![]),
            m("riders", Agg::CountDistinct(id("rider")), vec![]),
            m("pickups", Agg::CountDistinct(id("pu")), vec![]),
            m("card", Agg::Count, vec![cond("pay", vec![Literal::Int(1)])]),
            m(
                "shared_fare",
                Agg::Sum(id("fare")),
                vec![cond("shared", vec![Literal::Bool(true)])],
            ),
            m(
                "mq_fare",
                Agg::Sum(id("fare")),
                vec![
                    cond("pickup_borough", vec![lit("m"), lit("q")]),
                    cond("vendor", vec![Literal::Int(2)]),
                ],
            ),
            m(
                "m_riders",
                Agg::CountDistinct(id("rider")),
                vec![cond("dropoff_borough", vec![lit("m")])],
            ),
            m(
                "far_tip",
                Agg::Max(id("tip")),
                vec![cond("dro", vec![Literal::Int(3), Literal::Int(6)])],
            ),
            m(
                "zone_four_distance",
                Agg::Avg(id("dist")),
                vec![cond("pu", vec![Literal::Int(4)])],
            ),
        ],
    };
    Spec::new(vec![zones, combos, vendors], vec![trips]).unwrap()
}

/// splitmix64; the tests need no crate for randomness.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    fn int(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % (hi - lo) as u64) as i64
    }

    /// An integer in `lo..hi`, or null `percent` percent of the time.
    fn maybe(&mut self, lo: i64, hi: i64, percent: usize) -> Datum {
        if self.chance(percent) {
            Datum::Null
        } else {
            Datum::Int(self.int(lo, hi))
        }
    }

    fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }

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

/// A dim row as a load reads it: the key, then the attributes.
fn dim_row(key: &[Datum], attrs: &[Datum]) -> Vec<Datum> {
    [key, attrs].concat()
}

/// Loads every dim, as a load does, rows laid end to end.
fn load_dims(store: &mut Store, dims: &Dims) {
    for (dim, rows) in dims {
        let flat: Vec<Datum> = (rows.iter())
            .flat_map(|(key, attrs)| dim_row(key, attrs))
            .collect();
        store.add_dim_rows(dim, &flat).unwrap();
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
        store.remove_dim_row(dim, &dim_row(&key, &old)).unwrap();
        if rng.chance(40) {
            return;
        }
    }
    store.add_dim_row(dim, &dim_row(&key, &attrs)).unwrap();
    rows.insert(key, attrs);
}

fn random_ask(rng: &mut Rng, spec: &Spec) -> Ask {
    let mut by: Vec<AskKey> = KEYS
        .iter()
        .filter(|_| rng.chance(25))
        .map(|k| AskKey::Key(key(k)))
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
    let mut measures: Vec<Ident> = all
        .iter()
        .filter(|_| rng.chance(50))
        .map(|m| m.name.clone())
        .collect();
    if measures.is_empty() {
        measures.push(rng.pick(all).name.clone());
    }
    let buckets = (END - START) / i64::from(GRAIN);
    let from = START + i64::from(GRAIN) * rng.int(-10, buckets);
    let to = from + i64::from(GRAIN) * rng.int(1, buckets + 20);
    Ask {
        fact: id("trips"),
        measures,
        by,
        filter,
        from,
        to,
    }
}

/// SQL `=`: null matches nothing.
fn holds(value: &Datum, values: &[Literal]) -> bool {
    values.iter().any(|l| match (value, l) {
        (Datum::Int(a), Literal::Int(b)) => a == b,
        (Datum::Text(a), Literal::Text(b)) => **a == **b,
        (Datum::Bool(a), Literal::Bool(b)) => a == b,
        _ => false,
    })
}

/// A key's value for a row: a column, the null-ness key, or a dim value
/// joined as the dims are now, null when the join finds no row.
fn value(row: &[Datum], key: &Ident, dims: &Dims) -> Datum {
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
    assert_eq!(types.len(), WIDTH);
    types
}

/// A store with the types a load would set, for the fact and the dims.
fn typed(spec: &Spec) -> Store {
    let mut store = Store::new(spec.clone());
    store.set_fact_types("trips", &trip_types()).unwrap();
    for (dim, types) in DIM_TYPES {
        store.set_dim_types(dim, types).unwrap();
    }
    store
}

/// The scale a key's values print in: the dims' attributes are all text.
fn scale(key: &Ident) -> u32 {
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

fn passes(row: &[Datum], conds: &[Cond], dims: &Dims) -> bool {
    conds
        .iter()
        .all(|c| holds(&value(row, &c.key, dims), &c.values))
}

/// The ask over the raw rows joined to the dims: no rollup, no buckets.
fn naive(spec: &Spec, ask: &Ask, rows: &[Vec<Datum>], dims: &Dims) -> Answer {
    let fact = spec.fact("trips").unwrap();
    let mut groups: BTreeMap<Vec<Datum>, Vec<&[Datum]>> = BTreeMap::new();
    if ask.by.is_empty() {
        groups.insert(Vec::new(), Vec::new());
    }
    for row in rows {
        let Datum::Int(t) = row[AT] else { continue };
        if t < ask.from || t >= ask.to || !passes(row, &ask.filter, dims) {
            continue;
        }
        let key = ask
            .by
            .iter()
            .map(|k| match k {
                AskKey::Key(p) => value(row, p, dims),
                AskKey::Time(tk) => Datum::Int(tk.start(t, Grain(GRAIN))),
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
    let columns = (ask.by.iter().map(|k| match k {
        AskKey::Key(p) => AnswerColumn {
            name: p.to_string(),
            scale: scale(p),
            time: false,
        },
        AskKey::Time(t) => AnswerColumn {
            name: t.to_string(),
            scale: 0,
            time: true,
        },
    }))
    .chain(ask.measures.iter().map(|name| AnswerColumn {
        name: name.to_string(),
        scale: match &fact.measure(name.as_str()).unwrap().agg {
            Agg::Count | Agg::CountDistinct(_) => 0,
            Agg::Sum(c) | Agg::Min(c) | Agg::Max(c) | Agg::Avg(c) => {
                trip_types()[column(c.as_str())].scale()
            }
        },
        time: false,
    }))
    .collect();
    Answer { columns, rows }
}

fn measure(m: &Measure, rows: &[&[Datum]], dims: &Dims) -> Value {
    let rows: Vec<_> = rows.iter().filter(|r| passes(r, &m.filter, dims)).collect();
    let ints = |c: &Ident| -> Vec<i128> {
        rows.iter()
            .filter_map(|r| match r[column(c.as_str())] {
                Datum::Int(v) => Some(i128::from(v)),
                _ => None,
            })
            .collect()
    };
    let or_null = |v: Option<i128>| v.map_or(Value::Null, Value::Int);
    match &m.agg {
        Agg::Count => Value::Int(rows.len() as i128),
        Agg::Sum(c) => {
            let v = ints(c);
            or_null((!v.is_empty()).then(|| v.iter().sum()))
        }
        Agg::Avg(c) => {
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
        Agg::Min(c) => or_null(ints(c).into_iter().min()),
        Agg::Max(c) => or_null(ints(c).into_iter().max()),
        Agg::CountDistinct(c) => {
            let seen: HashSet<_> = rows
                .iter()
                .map(|r| &r[column(c.as_str())])
                .filter(|d| **d != Datum::Null)
                .collect();
            Value::Int(seen.len() as i128)
        }
    }
}

fn run(seed: u64) {
    let spec = spec();
    let mut store = typed(&spec);
    let mut rng = Rng(seed);
    let mut rows: Vec<Vec<Datum>> = Vec::new();
    let mut dims = random_dims(&mut rng);
    load_dims(&mut store, &dims);

    for step in 0..2_000 {
        match rng.below(40) {
            0..=3 => change_dim(&mut rng, &mut store, &mut dims),
            4..=8 if !rows.is_empty() => {
                let row = rows.swap_remove(rng.below(rows.len()));
                store.remove_fact_row("trips", &row).unwrap();
            }
            9..=14 if !rows.is_empty() => {
                // An update: the old row out, a new one in, often in the
                // same cell.
                let i = rng.below(rows.len());
                let mut new = trip(&mut rng);
                if rng.chance(50) {
                    new[..=DRO].clone_from_slice(&rows[i][..=DRO]);
                }
                store.remove_fact_row("trips", &rows[i]).unwrap();
                store.add_fact_row("trips", &new).unwrap();
                rows[i] = new;
            }
            _ => {
                let mut row = trip(&mut rng);
                // Often into a cell that already holds rows.
                if !rows.is_empty() && rng.chance(40) {
                    let other = rng.below(rows.len());
                    row[..=DRO].clone_from_slice(&rows[other][..=DRO]);
                }
                store.add_fact_row("trips", &row).unwrap();
                rows.push(row);
            }
        }
        if step % 100 == 99 {
            for _ in 0..10 {
                let ask = random_ask(&mut rng, &spec);
                assert_eq!(
                    store.ask(&ask).unwrap(),
                    naive(&spec, &ask, &rows, &dims),
                    "seed {seed}, step {step}, {ask:?}"
                );
            }
        }
    }

    assert!(store.cells("trips").unwrap() > 0);
    for row in rows {
        store.remove_fact_row("trips", &row).unwrap();
    }
    assert_eq!(
        store.cells("trips").unwrap(),
        0,
        "seed {seed}: a cell outlived its rows"
    );
}

#[test]
fn store_matches_naive_reference() {
    for seed in 0..16 {
        run(seed);
    }
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
            by_row.add_fact_row("trips", row).unwrap();
        }
        let flat: Vec<Datum> = rows.concat();
        let mut rest = &flat[..];
        while !rest.is_empty() {
            let (batch, after) = rest.split_at(rest.len().min((1 + rng.below(300)) * WIDTH));
            together.add_fact_rows("trips", batch).unwrap();
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
            together.remove_fact_row("trips", row).unwrap();
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
        row[AT] = Datum::Int(START + i64::from(GRAIN) * i);
        store.add_fact_row("trips", &row).unwrap();
        rows.push(row);
    }
    let ask = |from, to| Ask {
        fact: id("trips"),
        measures: vec![id("trips")],
        by: Vec::new(),
        filter: Vec::new(),
        from,
        to,
    };
    let trips = |ask: &Ask| store.ask(ask).unwrap().rows[0].values[0].clone();
    let count = |n: i128| Value::Int(n);
    let grain = i64::from(GRAIN);
    assert_eq!(trips(&ask(START - grain, END + grain)), count(4));
    assert_eq!(trips(&ask(START + grain, END)), count(3));
    assert_eq!(trips(&ask(START, START + grain)), count(1));
    assert_eq!(trips(&ask(START - grain, START)), count(0));
    assert_eq!(trips(&ask(END, END + grain)), count(0));
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
        .add_dim_row("vendors", &[Datum::Int(1), text("v1")])
        .unwrap();
    let mut row = trip(&mut Rng(3));
    row[AT] = Datum::Int(START);
    row[VENDOR] = Datum::Int(1);
    store.add_fact_row("trips", &row).unwrap();
    let by_label = Ask {
        fact: id("trips"),
        measures: vec![id("trips")],
        by: vec![AskKey::Key(key("vend_label"))],
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

    store
        .remove_dim_row("vendors", &[Datum::Int(1), text("v1")])
        .unwrap();
    assert_eq!(labels(&store), [Datum::Null]);
    store
        .add_dim_row("vendors", &[Datum::Int(1), text("v2")])
        .unwrap();
    assert_eq!(labels(&store), [text("v2")]);
    assert_eq!(store.cells("trips"), cells);
    // Another vendor's row changes nothing here.
    store
        .add_dim_row("vendors", &[Datum::Int(2), text("v9")])
        .unwrap();
    assert_eq!(labels(&store), [text("v2")]);
    store.clear_dim("vendors");
    assert_eq!(labels(&store), [Datum::Null]);
    assert_eq!(store.dim_rows("vendors"), Some(0));
    // The fact row comes out as it went in, whatever the dim did.
    store.remove_fact_row("trips", &row).unwrap();
    assert_eq!(store.cells("trips"), Some(0));
}

/// A dim keyed by several columns matches on all of them; a via that is
/// null joins nothing, even when a dim row has a null there.
#[test]
fn a_composite_key_matches_every_column() {
    let spec = spec();
    let mut store = typed(&spec);
    store
        .add_dim_rows(
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
        fact: id("trips"),
        measures: vec![id("trips")],
        by: vec![AskKey::Key(key("combo_label"))],
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
            .add_dim_rows(
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
        store.add_fact_row("trips", &row).unwrap();
        let answer = store.ask(&by_label).unwrap();
        assert_eq!(answer.rows[0].keys, [label]);
    }
    // Filtered on the attribute too.
    by_label.filter = vec![cond("combo_label", vec![lit("x")])];
    by_label.by.clear();
    let mut row = trip(&mut rng);
    row[AT] = Datum::Int(START);
    row[PU] = Datum::Int(1);
    row[VENDOR] = Datum::Int(1);
    store.add_fact_row("trips", &row).unwrap();
    assert_eq!(
        store.ask(&by_label).unwrap().rows[0].values,
        [Value::Int(1)]
    );
    store
        .remove_dim_row("combos", &[Datum::Int(1), Datum::Int(1), text("x")])
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
        .add_dim_rows("zones", &[Datum::Int(2), text("m"), text("a")])
        .unwrap();
    let mut row = trip(&mut Rng(8));
    row[AT] = Datum::Int(START);
    row[PU] = Datum::Int(2);
    row[VENDOR] = Datum::Int(2);
    row[FARE] = Datum::Int(100);
    store.add_fact_row("trips", &row).unwrap();
    let mq_fare = Ask {
        fact: id("trips"),
        measures: vec![id("mq_fare")],
        by: Vec::new(),
        filter: Vec::new(),
        from: START,
        to: END,
    };
    let fare = |store: &Store| store.ask(&mq_fare).unwrap().rows[0].values[0].clone();
    assert_eq!(fare(&store), Value::Int(100));
    store
        .remove_dim_row("zones", &[Datum::Int(2), text("m"), text("a")])
        .unwrap();
    store
        .add_dim_row("zones", &[Datum::Int(2), text("b"), text("a")])
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
        store.add_fact_row("trips", row).unwrap();
    }
    let ask = Ask {
        fact: id("trips"),
        measures: vec![id("trips"), id("riders")],
        by: KEYS.iter().map(|k| AskKey::Key(key(k))).collect(),
        filter: Vec::new(),
        from: START,
        to: END,
    };
    assert_eq!(store.ask(&ask).unwrap(), naive(&spec, &ask, &rows, &dims));
}

#[test]
fn dim_row_errors() {
    let mut store = typed(&spec());
    let row = [Datum::Int(1), text("m"), text("a")];
    assert_eq!(
        store.add_dim_row("zones", &row[1..]),
        Err(RowError::Arity {
            expected: 3,
            found: 2
        })
    );
    assert_eq!(
        store.add_dim_rows("zones", &row[1..]),
        Err(RowError::Arity {
            expected: 3,
            found: 2
        })
    );
    assert_eq!(
        store.set_dim_types("zones", &[ColumnType::Int; 2]),
        Err(RowError::Arity {
            expected: 3,
            found: 2
        })
    );
    store.add_dim_row("zones", &row).unwrap();
    // The same row again is a second row under the key, as a join finds.
    store.add_dim_row("zones", &row).unwrap();
    assert_eq!(store.dim_rows("zones"), Some(2));
    let other = [Datum::Int(1), text("q"), text("a")];
    assert_eq!(
        store.remove_dim_row("zones", &other),
        Err(RowError::Missing)
    );
    store.remove_dim_row("zones", &row).unwrap();
    store.remove_dim_row("zones", &row).unwrap();
    assert_eq!(store.remove_dim_row("zones", &row), Err(RowError::Missing));
    // A batch is all or nothing.
    let bad = [row.to_vec(), vec![Datum::Int(2)]].concat();
    assert_eq!(
        store.add_dim_rows("zones", &bad),
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
            by_row.add_dim_row(dim, &dim_row(key, attrs)).unwrap();
        }
    }
    let rows: Vec<Vec<Datum>> = (0..300).map(|_| trip(&mut rng)).collect();
    for row in &rows {
        together.add_fact_row("trips", row).unwrap();
        by_row.add_fact_row("trips", row).unwrap();
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
        store.add_fact_rows("trips", &good[1..]),
        Err(RowError::Arity {
            expected: WIDTH,
            found: WIDTH - 1
        })
    );
    let mut bad = good.clone();
    bad[WIDTH + AT] = text("noon");
    assert_eq!(
        store.add_fact_rows("trips", &bad),
        Err(RowError::NotInt { column: id("at") })
    );
    assert_eq!(store.cells("trips"), Some(0));
    store.add_fact_rows("trips", &[]).unwrap();
    store.add_fact_rows("trips", &good).unwrap();
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
        .map(Ident::to_string)
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
            Slot::Column(c) => c.to_string(),
            Slot::IsNull { column, negated } => {
                format!("{column} is {}null", if negated { "not " } else { "" })
            }
            Slot::Bucket => "bucket".into(),
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
    store.add_fact_row("trips", &row).unwrap();
    row[AT] = Datum::Int(START + i64::from(GRAIN) - 1);
    store.add_fact_row("trips", &row).unwrap();
    assert_eq!(store.cells("trips"), Some(1));
    row[AT] = Datum::Int(START + i64::from(GRAIN));
    store.add_fact_row("trips", &row).unwrap();
    assert_eq!(store.cells("trips"), Some(2));
    // A join column is a key: another value is another cell.
    row[PU] = Datum::Int(9);
    store.add_fact_row("trips", &row).unwrap();
    assert_eq!(store.cells("trips"), Some(3));
    // A dim row is none.
    store
        .add_dim_row("zones", &[Datum::Int(9), text("m"), text("a")])
        .unwrap();
    assert_eq!(store.cells("trips"), Some(3));
}

#[test]
fn removing_an_extreme_reveals_the_next() {
    let mut store = Store::new(spec());
    let ask = Ask {
        fact: id("trips"),
        measures: vec![id("tip_min"), id("tip_max")],
        by: Vec::new(),
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
        store.add_fact_row("trips", row).unwrap();
    }
    assert_eq!(store.cells("trips"), Some(1));
    assert_eq!(extremes(&store), [Value::Int(2), Value::Int(9)]);

    // One of two 9s out: the max stays.
    store.remove_fact_row("trips", &rows[1]).unwrap();
    assert_eq!(extremes(&store), [Value::Int(2), Value::Int(9)]);
    store.remove_fact_row("trips", &rows[2]).unwrap();
    assert_eq!(extremes(&store), [Value::Int(2), Value::Int(7)]);
    store.remove_fact_row("trips", &rows[3]).unwrap();
    assert_eq!(extremes(&store), [Value::Int(5), Value::Int(7)]);
    // A value past the old extremes after they went.
    row[TIP] = Datum::Int(1);
    store.add_fact_row("trips", &row).unwrap();
    assert_eq!(extremes(&store), [Value::Int(1), Value::Int(7)]);
}

#[test]
fn clear_drops_every_row() {
    let mut store = Store::new(spec());
    let trip = trip(&mut Rng(5));
    store.add_fact_row("trips", &trip).unwrap();
    store.clear_fact("trips");
    assert_eq!(store.cells("trips"), Some(0));
    assert_eq!(
        store.remove_fact_row("trips", &trip),
        Err(RowError::Missing)
    );
    store.add_fact_row("trips", &trip).unwrap();
    assert_eq!(store.cells("trips"), Some(1));
}

#[test]
fn filter_values_are_of_their_column_type() {
    let mut store = typed(&spec());
    let ask = |filter| Ask {
        fact: id("trips"),
        measures: vec![id("trips")],
        by: Vec::new(),
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
    store.set_fact_types("trips", &types).unwrap();
    assert!(store.ask(&pay()).is_ok());
    types[PAY] = ColumnType::Numeric { scale: 2 };
    store.set_fact_types("trips", &types).unwrap();
    assert_eq!(
        store.ask(&pay()).unwrap_err()[0].to_string(),
        "pay is a numeric of scale 2, so it can't be the integer 1"
    );
}

#[test]
fn types_stay_through_clear() {
    let mut store = Store::new(spec());
    let ask = Ask {
        fact: id("trips"),
        measures: vec![id("trips"), id("fare"), id("tip_max")],
        by: vec![AskKey::Key(key("pay")), AskKey::Key(key("pickup_zone"))],
        filter: Vec::new(),
        from: START,
        to: END,
    };
    let scales = |store: &Store| -> Vec<u32> {
        let answer = store.ask(&ask).unwrap();
        answer.columns.iter().map(|c| c.scale).collect()
    };
    assert_eq!(scales(&store), [0, 0, 0, 0, 0]);
    let mut types = vec![ColumnType::Int; WIDTH];
    types[PAY] = ColumnType::Numeric { scale: 3 };
    types[FARE] = ColumnType::Numeric { scale: 2 };
    types[TIP] = ColumnType::Numeric { scale: 1 };
    store.set_fact_types("trips", &types).unwrap();
    // An attribute has the type its dim column has.
    let zone = ColumnType::Numeric { scale: 4 };
    store
        .set_dim_types("zones", &[ColumnType::Int, ColumnType::Text, zone])
        .unwrap();
    store.clear_fact("trips");
    store.clear_dim("zones");
    assert_eq!(scales(&store), [3, 4, 0, 2, 1]);
}

#[test]
fn columns_name_keys_and_measures_as_asked() {
    let store = Store::new(spec());
    let ask = Ask {
        fact: id("trips"),
        measures: vec![id("fare"), id("trips")],
        by: vec![
            AskKey::Key(key("pickup_zone")),
            AskKey::Time(TimeKey::Bucket),
        ],
        filter: Vec::new(),
        from: START,
        to: END,
    };
    let columns: Vec<(String, bool)> = (store.ask(&ask).unwrap().columns.into_iter())
        .map(|c| (c.name, c.time))
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
    let grain = Grain(GRAIN);
    let cases = [
        // 2024-02-29 13:47:05 UTC.
        (1_709_214_425, 1_709_214_300),
        // 1969-12-31 23:59:59, before the epoch.
        (-1, -900),
    ];
    for (t, start) in cases {
        assert_eq!(TimeKey::Bucket.start(t, grain), start, "{t}");
    }
}

#[test]
fn ask_without_by_answers_one_row() {
    let store = Store::new(spec());
    let fact = spec();
    let measures: Vec<Ident> = fact
        .fact("trips")
        .unwrap()
        .measures
        .iter()
        .map(|m| m.name.clone())
        .collect();
    let ask = Ask {
        fact: id("trips"),
        measures,
        by: Vec::new(),
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
        fact: id("zones"),
        measures: vec![id("trips")],
        by: Vec::new(),
        filter: Vec::new(),
        from: START,
        to: END,
    };
    assert_eq!(
        store.ask(&ask),
        Err(vec![AskError::UnknownFact(id("zones"))])
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
        store.add_fact_row("trips", &base[1..]),
        Err(RowError::Arity {
            expected: WIDTH,
            found: WIDTH - 1
        })
    );
    assert_eq!(
        store.add_fact_row("trips", &with(AT, text("noon"))),
        Err(RowError::NotInt { column: id("at") })
    );
    assert_eq!(
        store.remove_fact_row("trips", &with(TIP, Datum::Bool(true))),
        Err(RowError::NotInt { column: id("tip") })
    );
    // Count distinct takes any value.
    store
        .add_fact_row("trips", &with(RIDER, Datum::Bool(true)))
        .unwrap();
    assert_eq!(
        store.remove_fact_row("trips", &base),
        Err(RowError::Missing)
    );
    // A batch that isn't whole rows.
    assert_eq!(
        store.add_fact_rows("trips", &base[1..]),
        Err(RowError::Arity {
            expected: WIDTH,
            found: WIDTH - 1
        })
    );

    // Each kind of state notices a value it never saw.
    let seen = with(FARE, Datum::Null);
    for (pos, v) in [
        (FARE, Datum::Int(1)),
        (TIP, Datum::Int(301)),
        (RIDER, text("zed")),
    ] {
        let mut store = fresh();
        store.add_fact_row("trips", &seen).unwrap();
        let mut other = seen.clone();
        other[pos] = v;
        assert_eq!(
            store.remove_fact_row("trips", &other),
            Err(RowError::Missing),
            "{pos}"
        );
    }

    let mut store = fresh();
    assert_eq!(
        store.set_fact_types("trips", &[ColumnType::Int; WIDTH + 1]),
        Err(RowError::Arity {
            expected: WIDTH,
            found: WIDTH + 1
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
    ];
    for (err, msg) in cases {
        assert_eq!(err.to_string(), msg);
    }
}
