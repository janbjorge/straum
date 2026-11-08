//! `Store` against a naive reference that aggregates the live rows directly,
//! over random inserts, updates and deletes of trips and zones.

use std::collections::{BTreeMap, HashSet};

use weir_domain::{
    Agg, Answer, Ask, AskError, AskKey, Change, Cond, Datum, Dim, Fact, Grain, Ident, KeyPath,
    Literal, Measure, Ref, RowError, Spec, Store, Table, Time, TimeKey, Value, Zone,
};

const GRAIN: u32 = 900;
/// 2024-01-15 and 2024-04-15, 00:00 UTC: four months, one leap day.
const START: i64 = 1_705_276_800;
const END: i64 = 1_713_139_200;
/// Europe/Oslo's 2024 summer time: 2024-03-31 and 2024-10-27, 01:00 UTC.
const OSLO_SUMMER: i64 = 1_711_846_800;
const OSLO_WINTER: i64 = 1_729_990_800;

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
        key: id("id"),
        time: Time {
            column: id("at"),
            grain: Grain(GRAIN),
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
        append_only: false,
        measures: vec![
            m("trips", Agg::Count, vec![]),
            m("fare", Agg::Sum(id("fare")), vec![]),
            m("tip_min", Agg::Min(id("tip")), vec![]),
            m("tip_max", Agg::Max(id("tip")), vec![]),
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

fn oslo() -> Zone {
    Zone::new(
        3_600,
        vec![
            Change {
                at: OSLO_SUMMER,
                offset: 7_200,
            },
            Change {
                at: OSLO_WINTER,
                offset: 3_600,
            },
        ],
    )
    .unwrap()
}

/// A zone whose clock jumps by `jump` seconds at `at`, from `before`.
fn jump(before: i64, at: i64, jump: i64) -> Zone {
    Zone::new(
        before,
        vec![Change {
            at,
            offset: before + jump,
        }],
    )
    .unwrap()
}

/// Zones whose offsets all fall on 15 minute edges.
fn zones() -> [Zone; 5] {
    [
        Zone::utc(),
        oslo(),
        // India, +05:30.
        Zone::new(19_800, Vec::new()).unwrap(),
        // Clocks skip or repeat the hour after midnight on START.
        jump(0, START, 3_600),
        jump(3_600, START, -3_600),
    ]
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
        let times = [
            TimeKey::Bucket,
            TimeKey::Hour,
            TimeKey::Day,
            TimeKey::Week,
            TimeKey::Month,
        ];
        let at = rng.below(by.len() + 1);
        by.insert(at, AskKey::Time(*rng.pick(&times)));
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
    let zone = rng.pick(&zones()).clone();
    let buckets = (END - START) / i64::from(GRAIN);
    let from = START + i64::from(GRAIN) * rng.int(-10, buckets);
    let to = from + i64::from(GRAIN) * rng.int(1, buckets + 20);
    Ask {
        fact: id("trips"),
        measures,
        by,
        filter,
        zone,
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
                AskKey::Time(tk) => Datum::Int(tk.start(t, Grain(GRAIN), &ask.zone)),
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
    Answer { rows }
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
    let mut store = Store::new(spec.clone());
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
fn time_key_start() {
    let grain = Grain(GRAIN);
    // Expected values from `date -u`.
    let cases = [
        // 2024-02-29 13:47:05, a Thursday in a leap February.
        (TimeKey::Bucket, 1_709_214_425, 1_709_214_300),
        (TimeKey::Hour, 1_709_214_425, 1_709_211_600),
        (TimeKey::Day, 1_709_214_425, 1_709_164_800),
        (TimeKey::Week, 1_709_214_425, 1_708_905_600),
        (TimeKey::Month, 1_709_214_425, 1_706_745_600),
        // 1969-12-31 23:59:59, before the epoch.
        (TimeKey::Bucket, -1, -900),
        (TimeKey::Hour, -1, -3_600),
        (TimeKey::Day, -1, -86_400),
        (TimeKey::Week, -1, -259_200),
        (TimeKey::Month, -1, -2_678_400),
        // 2000-03-01, the day after a leap day in a 400-year leap year.
        (TimeKey::Day, 951_868_800, 951_868_800),
        (TimeKey::Week, 951_868_800, 951_696_000),
        (TimeKey::Month, 951_868_800, 951_868_800),
        // 2024-01-15, a Monday.
        (TimeKey::Week, START, START),
        (TimeKey::Month, START, 1_704_067_200),
    ];
    for (key, t, start) in cases {
        assert_eq!(key.start(t, grain, &Zone::utc()), start, "{key} of {t}");
    }
}

#[test]
fn time_key_start_in_a_zone() {
    let grain = Grain(GRAIN);
    let oslo = oslo();
    // Expected values from `TZ=Europe/Oslo date`.
    let cases = [
        // 2024-03-31 10:00 CEST, the day summer time starts.
        (&oslo, TimeKey::Hour, 1_711_872_000, 1_711_872_000),
        (&oslo, TimeKey::Day, 1_711_872_000, 1_711_839_600),
        (&oslo, TimeKey::Week, 1_711_872_000, 1_711_321_200),
        (&oslo, TimeKey::Month, 1_711_872_000, 1_709_247_600),
        // 2024-04-15 12:00 CEST: April starts in summer time.
        (&oslo, TimeKey::Month, 1_713_175_200, 1_711_922_400),
        // 02:30 happens twice on 2024-10-27, first at +02:00, then at +01:00.
        // The two hours stay apart; the day is one.
        (&oslo, TimeKey::Hour, 1_729_989_000, 1_729_987_200),
        (&oslo, TimeKey::Hour, 1_729_992_600, OSLO_WINTER),
        (&oslo, TimeKey::Day, 1_729_989_000, 1_729_980_000),
        (&oslo, TimeKey::Day, 1_729_992_600, 1_729_980_000),
        // Buckets ignore the zone.
        (&oslo, TimeKey::Bucket, 1_711_872_000 + 899, 1_711_872_000),
    ];
    for (zone, key, t, start) in cases {
        assert_eq!(key.start(t, grain, zone), start, "{key} of {t}");
    }

    // Midnight skipped: the day starts when the clock jumps.
    let skipped = jump(0, START, 3_600);
    assert_eq!(TimeKey::Day.start(START + 7_200, grain, &skipped), START);
    // Midnight repeated: the day starts at the first one.
    let repeated = jump(3_600, START, -3_600);
    assert_eq!(
        TimeKey::Day.start(START + 7_200, grain, &repeated),
        START - 3_600
    );
}

#[test]
fn zone_changes_must_ascend() {
    let change = |at| Change { at, offset: 0 };
    assert!(Zone::new(0, vec![change(1), change(2)]).is_some());
    assert!(Zone::new(0, vec![change(2), change(1)]).is_none());
    assert!(Zone::new(0, vec![change(1), change(1)]).is_none());
}

#[test]
fn zone_must_fall_on_bucket_edges() {
    let spec = spec();
    let ask = |key, zone| Ask {
        fact: id("trips"),
        measures: vec![id("trips")],
        by: vec![AskKey::Time(key)],
        filter: Vec::new(),
        zone,
        from: START,
        to: END,
    };
    let unaligned = [
        // +00:10.
        Zone::new(600, Vec::new()).unwrap(),
        // A change at 01:10 UTC.
        jump(0, START + 4_200, 3_600),
    ];
    for zone in unaligned {
        assert_eq!(
            spec.check(&ask(TimeKey::Day, zone.clone())),
            Err(vec![AskError::ZoneUnaligned {
                grain: Grain(GRAIN)
            }])
        );
        assert!(spec.check(&ask(TimeKey::Bucket, zone)).is_ok());
    }
    for zone in zones() {
        assert!(spec.check(&ask(TimeKey::Month, zone)).is_ok());
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
        zone: Zone::utc(),
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
        zone: Zone::utc(),
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
