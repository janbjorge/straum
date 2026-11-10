//! `Store` against a naive reference that aggregates the live rows directly,
//! over random inserts, updates and deletes of trips and zones.

use std::collections::{BTreeMap, HashSet};

use straum_domain::{
    Agg, Answer, AnswerColumn, Ask, AskError, AskKey, ColumnType, Cond, Datum, Dim, Fact, Grain,
    Ident, KeyPath, Literal, Measure, Ref, RowError, Spec, Store, Table, Time, TimeKey, Value,
};

const GRAIN: u32 = 900;
/// 2024-01-15 and 2024-04-15, 00:00 UTC: four months, one leap day.
const START: i64 = 1_705_276_800;
const END: i64 = 1_713_139_200;

// Row positions; `columns` checks them.
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

/// The scales of the numeric columns `run` sets, from this up. Each
/// differs, so an answer that takes a scale from the wrong column shows.
const TRIP_SCALES: u32 = 20;

const KEYS: [&str; 9] = [
    "pay",
    "vendor",
    "shared",
    "pickup",
    "dropoff",
    "pickup.id",
    "pickup.borough",
    "dropoff.borough",
    "dropoff.zone",
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
        key: id("id"),
        attrs: vec![id("borough"), id("zone")],
    };
    let trips = Fact {
        name: id("trips"),
        table: table("trips"),
        time: Time {
            column: id("at"),
            grain: Grain(GRAIN),
            // Hours, days and weeks, so random ranges read every width.
            buckets: vec![Grain(3_600), Grain(86_400), Grain(604_800)],
        },
        refs: vec![
            Ref {
                name: id("pickup"),
                dim: id("zones"),
                via: id("pu"),
            },
            Ref {
                name: id("dropoff"),
                dim: id("zones"),
                via: id("dro"),
            },
        ],
        by: vec![id("pay"), id("vendor"), id("shared")],
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
                vec![cond("dropoff", vec![Literal::Int(3), Literal::Int(6)])],
            ),
            m(
                "zone_four_distance",
                Agg::Avg(id("dist")),
                vec![cond("pickup.id", vec![Literal::Int(4)])],
            ),
        ],
    };
    Spec::new(vec![zones], vec![trips]).unwrap()
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
}

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
        // Zones 5 and 6 never exist.
        rng.maybe(1, 7, 5),
        rng.maybe(1, 7, 5),
        rng.maybe(-500, 10_000, 5),
        rng.maybe(0, 300, 20),
        rng.maybe(0, 5_000, 5),
        rider,
    ]
}

fn zone(rng: &mut Rng, key: i64) -> Vec<Datum> {
    let borough = match rng.below(4) {
        0 => Datum::Null,
        1 => text("m"),
        2 => text("q"),
        _ => text("b"),
    };
    vec![Datum::Int(key), borough, text(rng.pick(&["a", "b", "c"]))]
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
        cond("dropoff", vec![Literal::Int(5)]),
        cond("dropoff.zone", vec![lit("a"), lit("c")]),
        cond("pickup.id", vec![Literal::Int(1), Literal::Int(3)]),
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

type Zones = BTreeMap<i64, Vec<Datum>>;

/// SQL `=`: null matches nothing.
fn holds(value: &Datum, values: &[Literal]) -> bool {
    values.iter().any(|l| match (value, l) {
        (Datum::Int(a), Literal::Int(b)) => a == b,
        (Datum::Text(a), Literal::Text(b)) => **a == **b,
        (Datum::Bool(a), Literal::Bool(b)) => a == b,
        _ => false,
    })
}

fn value(row: &[Datum], path: &KeyPath, zones: &Zones) -> Datum {
    let v = row[column(path.head.as_str())].clone();
    let Some(attr) = &path.attr else {
        return v;
    };
    let Datum::Int(key) = v else {
        return Datum::Null;
    };
    let Some(zone) = zones.get(&key) else {
        return Datum::Null;
    };
    match attr.as_str() {
        "id" => zone[0].clone(),
        "borough" => zone[1].clone(),
        "zone" => zone[2].clone(),
        other => panic!("no zone attribute {other}"),
    }
}

/// The column types `run` sets.
fn trip_types() -> Vec<ColumnType> {
    let numeric = |column: usize| ColumnType::Numeric {
        scale: TRIP_SCALES + column as u32,
    };
    vec![
        ColumnType::Time,
        ColumnType::Int,
        ColumnType::Int,
        ColumnType::Bool,
        ColumnType::Int,
        ColumnType::Int,
        numeric(FARE),
        numeric(TIP),
        numeric(DIST),
        ColumnType::Text,
    ]
}

fn zone_types() -> Vec<ColumnType> {
    vec![ColumnType::Int, ColumnType::Text, ColumnType::Text]
}

/// A store with the types a load would set.
fn typed(spec: &Spec) -> Store {
    let mut store = Store::new(spec.clone());
    store
        .rollup("trips")
        .unwrap()
        .set_types(&trip_types())
        .unwrap();
    store
        .dim("zones")
        .unwrap()
        .set_types(&zone_types())
        .unwrap();
    store
}

fn scale(path: &KeyPath) -> u32 {
    match &path.attr {
        None => trip_types()[column(path.head.as_str())].scale(),
        Some(attr) => {
            let column = match attr.as_str() {
                "id" => 0,
                "borough" => 1,
                _ => 2,
            };
            zone_types()[column].scale()
        }
    }
}

fn column(name: &str) -> usize {
    match name {
        "pay" => PAY,
        "vendor" => VENDOR,
        "shared" => SHARED,
        "pickup" | "pu" => PU,
        "dropoff" => DRO,
        "fare" => FARE,
        "tip" => TIP,
        "dist" => DIST,
        "rider" => RIDER,
        other => panic!("no column {other}"),
    }
}

fn passes(row: &[Datum], conds: &[Cond], zones: &Zones) -> bool {
    conds
        .iter()
        .all(|c| holds(&value(row, &c.path, zones), &c.values))
}

/// The ask over the raw rows: no rollup, no buckets.
fn naive(spec: &Spec, ask: &Ask, rows: &[Vec<Datum>], zones: &Zones) -> Answer {
    let fact = spec.fact("trips").unwrap();
    let mut groups: BTreeMap<Vec<Datum>, Vec<&[Datum]>> = BTreeMap::new();
    if ask.by.is_empty() {
        groups.insert(Vec::new(), Vec::new());
    }
    for row in rows {
        let Datum::Int(t) = row[AT] else { continue };
        if t < ask.from || t >= ask.to || !passes(row, &ask.filter, zones) {
            continue;
        }
        let key = ask
            .by
            .iter()
            .map(|k| match k {
                AskKey::Key(p) => value(row, p, zones),
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
                .map(|name| measure(fact.measure(name.as_str()).unwrap(), &rows, zones))
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

fn measure(m: &Measure, rows: &[&[Datum]], zones: &Zones) -> Value {
    let rows: Vec<_> = rows
        .iter()
        .filter(|r| passes(r, &m.filter, zones))
        .collect();
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
    let mut zones = Zones::new();
    for key in 1..=4 {
        let row = zone(&mut rng, key);
        store.dim("zones").unwrap().upsert(&row).unwrap();
        zones.insert(key, row);
    }

    for step in 0..2_000 {
        match rng.below(40) {
            0 => {
                let key = rng.int(1, 5);
                let row = zone(&mut rng, key);
                store.dim("zones").unwrap().upsert(&row).unwrap();
                zones.insert(key, row);
            }
            1 if !zones.is_empty() => {
                let key = *zones.keys().nth(rng.below(zones.len())).unwrap();
                store
                    .dim("zones")
                    .unwrap()
                    .delete(&Datum::Int(key))
                    .unwrap();
                zones.remove(&key);
            }
            2..=7 if !rows.is_empty() => {
                let row = rows.swap_remove(rng.below(rows.len()));
                store.rollup("trips").unwrap().remove(&row).unwrap();
            }
            8..=13 if !rows.is_empty() => {
                // An update: the old row out, a new one in, often in the
                // same cell.
                let i = rng.below(rows.len());
                let mut new = trip(&mut rng);
                if rng.chance(50) {
                    new[..=DRO].clone_from_slice(&rows[i][..=DRO]);
                }
                let rollup = store.rollup("trips").unwrap();
                rollup.remove(&rows[i]).unwrap();
                rollup.add(&new).unwrap();
                rows[i] = new;
            }
            _ => {
                let mut row = trip(&mut rng);
                // Often into a cell that already holds rows.
                if !rows.is_empty() && rng.chance(40) {
                    let other = rng.below(rows.len());
                    row[..=DRO].clone_from_slice(&rows[other][..=DRO]);
                }
                store.rollup("trips").unwrap().add(&row).unwrap();
                rows.push(row);
            }
        }
        if step % 100 == 99 {
            for _ in 0..10 {
                let ask = random_ask(&mut rng, &spec);
                assert_eq!(
                    store.ask(&ask).unwrap(),
                    naive(&spec, &ask, &rows, &zones),
                    "seed {seed}, step {step}, {ask:?}"
                );
            }
        }
    }

    let rollup = store.rollup("trips").unwrap();
    assert!(rollup.cells() > 0);
    for row in rows {
        rollup.remove(&row).unwrap();
    }
    assert_eq!(rollup.cells(), 0, "seed {seed}: a cell outlived its rows");
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
        for key in 1..=4 {
            let row = zone(&mut rng, key);
            by_row.dim("zones").unwrap().upsert(&row).unwrap();
            together.dim("zones").unwrap().upsert(&row).unwrap();
        }
        let rows: Vec<Vec<Datum>> = (0..2_000).map(|_| trip(&mut rng)).collect();
        for row in &rows {
            by_row.rollup("trips").unwrap().add(row).unwrap();
        }
        let flat: Vec<Datum> = rows.concat();
        let mut rest = &flat[..];
        while !rest.is_empty() {
            let (batch, after) = rest.split_at(rest.len().min((1 + rng.below(300)) * WIDTH));
            together.rollup("trips").unwrap().add_rows(batch).unwrap();
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
        let rollup = together.rollup("trips").unwrap();
        for row in &rows {
            rollup.remove(row).unwrap();
        }
        assert_eq!(rollup.cells(), 0, "seed {seed}: a cell outlived its rows");
    }
}

#[test]
fn rows_added_together_add_none_on_error() {
    let mut rng = Rng(5);
    let mut store = Store::new(spec());
    let rollup = store.rollup("trips").unwrap();
    let good = [trip(&mut rng), trip(&mut rng)].concat();
    assert_eq!(
        rollup.add_rows(&good[1..]),
        Err(RowError::Arity {
            expected: WIDTH,
            found: WIDTH - 1
        })
    );
    let mut bad = good.clone();
    bad[WIDTH + AT] = text("noon");
    assert_eq!(
        rollup.add_rows(&bad),
        Err(RowError::NotInt { column: id("at") })
    );
    assert_eq!(rollup.cells(), 0);
    rollup.add_rows(&[]).unwrap();
    rollup.add_rows(&good).unwrap();
    assert!(rollup.cells() > 0);
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

#[test]
fn rows_collapse_into_cells() {
    let mut store = Store::new(spec());
    let rollup = store.rollup("trips").unwrap();
    let mut row = trip(&mut Rng(7));
    row[AT] = Datum::Int(START + 1);
    rollup.add(&row).unwrap();
    row[AT] = Datum::Int(START + i64::from(GRAIN) - 1);
    rollup.add(&row).unwrap();
    assert_eq!(rollup.cells(), 1);
    row[AT] = Datum::Int(START + i64::from(GRAIN));
    rollup.add(&row).unwrap();
    assert_eq!(rollup.cells(), 2);
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
    let rollup = store.rollup("trips").unwrap();
    for row in &rows {
        rollup.add(row).unwrap();
    }
    assert_eq!(rollup.cells(), 1);
    assert_eq!(extremes(&store), [Value::Int(2), Value::Int(9)]);

    // One of two 9s out: the max stays.
    store.rollup("trips").unwrap().remove(&rows[1]).unwrap();
    assert_eq!(extremes(&store), [Value::Int(2), Value::Int(9)]);
    store.rollup("trips").unwrap().remove(&rows[2]).unwrap();
    assert_eq!(extremes(&store), [Value::Int(2), Value::Int(7)]);
    store.rollup("trips").unwrap().remove(&rows[3]).unwrap();
    assert_eq!(extremes(&store), [Value::Int(5), Value::Int(7)]);
    // A value past the old extremes after they went.
    row[TIP] = Datum::Int(1);
    store.rollup("trips").unwrap().add(&row).unwrap();
    assert_eq!(extremes(&store), [Value::Int(1), Value::Int(7)]);
}

#[test]
fn clear_drops_every_row() {
    let mut store = Store::new(spec());
    let trip = trip(&mut Rng(5));
    let rollup = store.rollup("trips").unwrap();
    rollup.add(&trip).unwrap();
    rollup.clear();
    assert_eq!(rollup.cells(), 0);
    assert_eq!(rollup.remove(&trip), Err(RowError::Missing));
    rollup.add(&trip).unwrap();
    assert_eq!(rollup.cells(), 1);

    let zone = zone(&mut Rng(5), 1);
    let zones = store.dim("zones").unwrap();
    zones.upsert(&zone).unwrap();
    zones.clear();
    assert_eq!(zones.delete(&zone[0]), Err(RowError::Missing));
    zones.upsert(&zone).unwrap();
    assert_eq!(zones.delete(&zone[0]), Ok(()));
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
        cond("pickup", vec![Literal::Int(2)]),
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
    store.rollup("trips").unwrap().set_types(&types).unwrap();
    assert!(store.ask(&pay()).is_ok());
    types[PAY] = ColumnType::Numeric { scale: 2 };
    store.rollup("trips").unwrap().set_types(&types).unwrap();
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
    let rollup = store.rollup("trips").unwrap();
    let mut types = vec![ColumnType::Int; 10];
    types[PAY] = ColumnType::Numeric { scale: 3 };
    types[FARE] = ColumnType::Numeric { scale: 2 };
    types[TIP] = ColumnType::Numeric { scale: 1 };
    rollup.set_types(&types).unwrap();
    rollup.clear();
    let zones = store.dim("zones").unwrap();
    let zone = ColumnType::Numeric { scale: 4 };
    let types = [ColumnType::Int, ColumnType::Text, zone];
    zones.set_types(&types).unwrap();
    zones.clear();
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
    assert!(store.dim("trips").is_none());
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
    let rollup = store.rollup("trips").unwrap();
    assert_eq!(
        rollup.add(&base[1..]),
        Err(RowError::Arity {
            expected: 10,
            found: 9
        })
    );
    assert_eq!(
        rollup.add(&with(AT, text("noon"))),
        Err(RowError::NotInt { column: id("at") })
    );
    assert_eq!(
        rollup.remove(&with(TIP, Datum::Bool(true))),
        Err(RowError::NotInt { column: id("tip") })
    );
    // Count distinct takes any value.
    rollup.add(&with(RIDER, Datum::Bool(true))).unwrap();
    assert_eq!(rollup.remove(&base), Err(RowError::Missing));

    // Each kind of state notices a value it never saw.
    let seen = with(FARE, Datum::Null);
    for (pos, v) in [
        (FARE, Datum::Int(1)),
        (TIP, Datum::Int(301)),
        (RIDER, text("zed")),
    ] {
        let mut store = fresh();
        let rollup = store.rollup("trips").unwrap();
        rollup.add(&seen).unwrap();
        let mut other = seen.clone();
        other[pos] = v;
        assert_eq!(rollup.remove(&other), Err(RowError::Missing), "{pos}");
    }

    let mut store = fresh();
    let zones = store.dim("zones").unwrap();
    assert_eq!(
        zones.upsert(&[Datum::Int(1)]),
        Err(RowError::Arity {
            expected: 3,
            found: 1
        })
    );
    assert_eq!(zones.delete(&Datum::Int(1)), Err(RowError::Missing));

    let mut store = fresh();
    let rollup = store.rollup("trips").unwrap();
    assert_eq!(
        rollup.set_types(&[ColumnType::Int; 11]),
        Err(RowError::Arity {
            expected: 10,
            found: 11
        })
    );
    let zones = store.dim("zones").unwrap();
    assert_eq!(
        zones.set_types(&[ColumnType::Text; 2]),
        Err(RowError::Arity {
            expected: 3,
            found: 2
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
