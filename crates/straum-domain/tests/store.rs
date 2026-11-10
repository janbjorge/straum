//! `Store` against a naive reference that aggregates the live rows directly,
//! over random inserts, updates and deletes of trips. Rows come whole, the
//! ref attributes resolved, as the source hands them over.

use std::collections::{BTreeMap, HashSet};

use straum_domain::{
    Agg, Answer, AnswerColumn, Ask, AskError, AskKey, By, ColumnType, Cond, Datum, Dim, Fact,
    Grain, Ident, KeyPath, Literal, Measure, Origin, Ref, RowError, Slot, Spec, Store, Table, Time,
    TimeKey, Value,
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
/// The ref attributes, after the columns read.
const PICKUP_BOROUGH: usize = 10;
const PICKUP_ZONE: usize = 11;
const DROPOFF_BOROUGH: usize = 12;
const DROPOFF_ZONE: usize = 13;
const COMBO_LABEL: usize = 14;
const VEND_LABEL: usize = 15;
const WIDTH: usize = 16;

/// The scales of the numeric columns `run` sets, from this up. Each
/// differs, so an answer that takes a scale from the wrong column shows.
const TRIP_SCALES: u32 = 20;

const KEYS: [&str; 12] = [
    "pay",
    "vendor",
    "shared",
    "pu",
    "dro",
    "pickup.borough",
    "pickup.zone",
    "dropoff.borough",
    "dropoff.zone",
    "combo.label",
    "known",
    "vend.label",
];

fn id(s: &str) -> Ident {
    Ident::new(s).unwrap()
}

fn path(s: &str) -> KeyPath {
    match s.split_once('.') {
        Some((head, attr)) => KeyPath {
            head: id(head),
            attr: Some(id(attr)),
        },
        None => KeyPath {
            head: id(s),
            attr: None,
        },
    }
}

fn text(s: &str) -> Datum {
    Datum::Text(s.into())
}

fn lit(s: &str) -> Literal {
    Literal::Text(s.into())
}

fn cond(p: &str, values: Vec<Literal>) -> Cond {
    Cond {
        path: path(p),
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
                via: vec![id("pu")],
                attrs: vec![id("borough"), id("zone")],
            },
            Ref {
                name: id("dropoff"),
                dim: id("zones"),
                via: vec![id("dro")],
                attrs: vec![id("borough"), id("zone")],
            },
            Ref {
                name: id("combo"),
                dim: id("combos"),
                via: vec![id("pu"), id("vendor")],
                attrs: vec![id("label")],
            },
            Ref {
                name: id("vend"),
                dim: id("vendors"),
                via: vec![id("vendor")],
                attrs: vec![id("label")],
            },
        ],
        by: vec![
            By::Column(id("pay")),
            By::Column(id("vendor")),
            By::Column(id("shared")),
            By::Column(id("pu")),
            By::Column(id("dro")),
            // Over a by column, which the row carries once.
            By::IsNull {
                column: id("shared"),
                name: id("known"),
                negated: true,
            },
        ],
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
                    cond("pickup.borough", vec![lit("m"), lit("q")]),
                    cond("vendor", vec![Literal::Int(2)]),
                ],
            ),
            m(
                "m_riders",
                Agg::CountDistinct(id("rider")),
                vec![cond("dropoff.borough", vec![lit("m")])],
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
    /// resolved attribute comes.
    fn attr(&mut self, choices: &[&str], percent: usize) -> Datum {
        if self.chance(percent) {
            Datum::Null
        } else {
            text(self.pick(choices))
        }
    }
}

/// A whole row: the columns read, then the attributes as the source
/// resolved them.
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
        rng.attr(&["m", "q", "b"], 25),
        rng.attr(&["a", "b", "c"], 25),
        rng.attr(&["m", "q", "b"], 25),
        rng.attr(&["a", "b", "c"], 25),
        rng.attr(&["x", "y"], 50),
        rng.attr(&["v1"], 50),
    ]
}

fn random_ask(rng: &mut Rng, spec: &Spec) -> Ask {
    let mut by: Vec<AskKey> = KEYS
        .iter()
        .filter(|_| rng.chance(25))
        .map(|k| AskKey::Key(path(k)))
        .collect();
    if rng.chance(60) {
        let at = rng.below(by.len() + 1);
        by.insert(at, AskKey::Time(TimeKey::Bucket));
    }
    let pool = [
        cond("pay", vec![Literal::Int(1), Literal::Int(2)]),
        cond("vendor", vec![Literal::Int(2)]),
        cond("shared", vec![Literal::Bool(false)]),
        cond("pickup.borough", vec![lit("m")]),
        cond("dro", vec![Literal::Int(5)]),
        cond("dropoff.zone", vec![lit("a"), lit("c")]),
        cond("pu", vec![Literal::Int(1), Literal::Int(3)]),
        cond("combo.label", vec![lit("x")]),
        cond("known", vec![Literal::Bool(true)]),
        cond("vend.label", vec![lit("v1")]),
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

fn value(row: &[Datum], path: &KeyPath) -> Datum {
    if path.head.as_str() == "known" {
        return Datum::Bool(row[SHARED] != Datum::Null);
    }
    row[column(&path.to_string())].clone()
}

/// The column types `run` sets.
fn trip_types() -> Vec<ColumnType> {
    let numeric = |column: usize| ColumnType::Numeric {
        scale: TRIP_SCALES + column as u32,
    };
    let mut types = vec![
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
    types.resize(WIDTH, ColumnType::Text);
    types
}

/// A store with the types a load would set.
fn typed(spec: &Spec) -> Store {
    let mut store = Store::new(spec.clone());
    store.set_fact_types("trips", &trip_types()).unwrap();
    store
}

fn scale(path: &KeyPath) -> u32 {
    if path.head.as_str() == "known" {
        return 0;
    }
    trip_types()[column(&path.to_string())].scale()
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
        "pickup.borough" => PICKUP_BOROUGH,
        "pickup.zone" => PICKUP_ZONE,
        "dropoff.borough" => DROPOFF_BOROUGH,
        "dropoff.zone" => DROPOFF_ZONE,
        "combo.label" => COMBO_LABEL,
        "vend.label" => VEND_LABEL,
        other => panic!("no column {other}"),
    }
}

fn passes(row: &[Datum], conds: &[Cond]) -> bool {
    conds.iter().all(|c| holds(&value(row, &c.path), &c.values))
}

/// The ask over the raw rows: no rollup, no buckets.
fn naive(spec: &Spec, ask: &Ask, rows: &[Vec<Datum>]) -> Answer {
    let fact = spec.fact("trips").unwrap();
    let mut groups: BTreeMap<Vec<Datum>, Vec<&[Datum]>> = BTreeMap::new();
    if ask.by.is_empty() {
        groups.insert(Vec::new(), Vec::new());
    }
    for row in rows {
        let Datum::Int(t) = row[AT] else { continue };
        if t < ask.from || t >= ask.to || !passes(row, &ask.filter) {
            continue;
        }
        let key = ask
            .by
            .iter()
            .map(|k| match k {
                AskKey::Key(p) => value(row, p),
                AskKey::Time(tk) => Datum::Int(tk.start(t, Grain(GRAIN))),
            })
            .collect();
        groups.entry(key).or_default().push(row);
    }
    let rows = groups
        .into_iter()
        .map(|(key, rows)| {
            let values = ask
                .measures
                .iter()
                .map(|name| measure(fact.measure(name.as_str()).unwrap(), &rows))
                .collect();
            (key, values)
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

fn measure(m: &Measure, rows: &[&[Datum]]) -> Value {
    let rows: Vec<_> = rows.iter().filter(|r| passes(r, &m.filter)).collect();
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

    for step in 0..2_000 {
        match rng.below(40) {
            0..=5 if !rows.is_empty() => {
                let row = rows.swap_remove(rng.below(rows.len()));
                store.remove_fact_row("trips", &row).unwrap();
            }
            6..=11 if !rows.is_empty() => {
                // An update: the old row out, a new one in, often in the
                // same cell.
                let i = rng.below(rows.len());
                let mut new = trip(&mut rng);
                if rng.chance(50) {
                    new[..=DRO].clone_from_slice(&rows[i][..=DRO]);
                    new[PICKUP_BOROUGH..].clone_from_slice(&rows[i][PICKUP_BOROUGH..]);
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
                    row[PICKUP_BOROUGH..].clone_from_slice(&rows[other][PICKUP_BOROUGH..]);
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
                    naive(&spec, &ask, &rows),
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
            together.rollup("trips").unwrap().cells(),
            by_row.rollup("trips").unwrap().cells(),
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

/// An attribute is part of the cell key as it was resolved when the row
/// came in. A row that comes back out with the attribute resolved
/// otherwise, because the dim row changed meanwhile, is not found, which
/// is what makes the writer start over.
#[test]
fn a_row_is_found_only_under_the_attributes_it_came_with() {
    let spec = spec();
    let mut store = typed(&spec);
    let mut row = trip(&mut Rng(3));
    row[AT] = Datum::Int(START);
    row[VEND_LABEL] = text("v1");
    store.add_fact_row("trips", &row).unwrap();
    let by_label = Ask {
        fact: id("trips"),
        measures: vec![id("trips")],
        by: vec![AskKey::Key(path("vend.label"))],
        filter: Vec::new(),
        from: START,
        to: END,
    };
    let labels = |store: &Store| -> Vec<Datum> {
        (store.ask(&by_label).unwrap().rows.into_iter())
            .map(|(key, _)| key[0].clone())
            .collect()
    };
    assert_eq!(labels(&store), [text("v1")]);

    // The same row, its label resolved anew.
    let mut relabelled = row.clone();
    relabelled[VEND_LABEL] = text("v2");
    assert_eq!(
        store.remove_fact_row("trips", &relabelled),
        Err(RowError::Missing)
    );
    store.add_fact_row("trips", &relabelled).unwrap();
    assert_eq!(labels(&store), [text("v1"), text("v2")]);
    store.remove_fact_row("trips", &row).unwrap();
    store.remove_fact_row("trips", &relabelled).unwrap();
    // A row with no dim row has null attributes, as the join gives.
    row[VEND_LABEL] = Datum::Null;
    store.add_fact_row("trips", &row).unwrap();
    assert_eq!(labels(&store), [Datum::Null]);
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
    let mut store = Store::new(spec());
    let columns: Vec<_> = store
        .rollup("trips")
        .unwrap()
        .columns()
        .iter()
        .map(Ident::as_str)
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
    assert_eq!(columns, expected.map(|(_, c)| c));
    for (pos, name) in expected {
        assert_eq!(columns[pos], name);
    }
}

/// The attributes follow the by keys in the rollup key, ref by ref, so a
/// row holds them after the columns read.
#[test]
fn rollup_key_puts_attributes_after_the_by_keys() {
    let spec = spec();
    let key: Vec<String> = (spec.fact("trips").unwrap().rollup_key())
        .map(|slot| match slot {
            Slot::Column(c) => c.to_string(),
            Slot::IsNull { column, negated } => {
                format!("{column} is {}null", if negated { "not " } else { "" })
            }
            Slot::Attr(r, a) => format!("{}.{a}", r.name),
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
            "pickup.borough",
            "pickup.zone",
            "dropoff.borough",
            "dropoff.zone",
            "combo.label",
            "vend.label",
            "bucket",
        ]
    );
    assert_eq!(key.len() - 1 - 6 + 10, WIDTH);
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
    // An attribute is a key: another value is another cell.
    row[PICKUP_ZONE] = text("other");
    store.add_fact_row("trips", &row).unwrap();
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
    let extremes = |store: &Store| store.ask(&ask).unwrap().rows[0].1.clone();
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
    store.rollup("trips").unwrap().clear();
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
        cond("pickup.borough", vec![lit("m")]),
    ];
    assert!(store.ask(&ask(fits)).is_ok());

    let wrong = vec![
        cond("pay", vec![lit("1"), Literal::Int(1)]),
        cond("shared", vec![Literal::Int(1)]),
        cond("pickup.borough", vec![Literal::Bool(true)]),
    ];
    let errors: Vec<String> = (store.ask(&ask(wrong)).unwrap_err().iter())
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        errors,
        [
            r#"pay is an integer, so it can't be the text "1""#,
            "shared is a boolean, so it can't be the integer 1",
            "pickup.borough is text, so it can't be the boolean true",
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
        by: vec![AskKey::Key(path("pay")), AskKey::Key(path("pickup.zone"))],
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
    // An attribute has the type its dim column has.
    types[PICKUP_ZONE] = ColumnType::Numeric { scale: 4 };
    store.set_fact_types("trips", &types).unwrap();
    store.rollup("trips").unwrap().clear();
    assert_eq!(scales(&store), [3, 4, 0, 2, 1]);
}

#[test]
fn columns_name_keys_and_measures_as_asked() {
    let store = Store::new(spec());
    let ask = Ask {
        fact: id("trips"),
        measures: vec![id("fare"), id("trips")],
        by: vec![
            AskKey::Key(path("pickup.zone")),
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
            ("pickup.zone".to_owned(), false),
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
        [(
            Vec::new(),
            vec![
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
        )]
    );
}

#[test]
fn unknown_names() {
    let mut store = Store::new(spec());
    assert!(store.rollup("zones").is_none());
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
