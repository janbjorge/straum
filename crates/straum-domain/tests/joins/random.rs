//! Random runs: orders, dim rows and changes to both drawn from a seed,
//! checked against the reference every few steps. Each test runs eight
//! seeds over one shape of view, so between them every join kind meets
//! every kind of cells an ask may read: keyed and bound.

use crate::common::{Rng, condition, id, lit, text, windows_match};
use crate::reference::reference;
use crate::view::{
    AT, DIM_NAMES, Dims, FILTERED, HOUR, KEYS, Kinds, Loaded, MEASURES, START, Shape, ask, keyed,
    load, order, spec,
};
use straum_domain::{Ask, AskKey, Datum, Literal, RefKind, Width};

/// A day, the view's wider width.
const DAY: i64 = 24 * HOUR;
/// Rows a day apart.
const DAY_ROWS: Width = Width { seconds: 86_400 };

/// The kinds a seed runs with. Over eight seeds: every way the three
/// joins that read a column are left or inner, and each kind twice for
/// each of the two that may read none.
fn kinds_of(seed: u64) -> Kinds {
    let any = [RefKind::Left, RefKind::Inner, RefKind::Semi, RefKind::Anti];
    [
        left_or_inner(seed, 0),
        left_or_inner(seed, 1),
        left_or_inner(seed, 2),
        any[usize::try_from(seed % 4).unwrap()],
        any[usize::try_from((seed / 2 + 1) % 4).unwrap()],
    ]
}

/// Left or inner, as bit `bit` of `seed` is unset or set.
fn left_or_inner(seed: u64, bit: u64) -> RefKind {
    match seed >> bit & 1 {
        0 => RefKind::Left,
        _ => RefKind::Inner,
    }
}

/// A dim row a run draws: which dim, its key and its attributes.
struct DimRow {
    dim: &'static str,
    key: Vec<Datum>,
    attributes: Vec<Datum>,
}

/// An order in the two days, or now and then one without a time.
fn random_order(rng: &mut Rng) -> Vec<Datum> {
    let mut order = order(
        START + HOUR * rng.int(0, 48),
        rng.maybe(1, 6, 5),
        rng.maybe(1, 5, 5),
        rng.maybe(0, 10, 10),
    );
    if rng.chance(3) {
        order[AT] = Datum::Null;
    }
    order
}

/// A random dim row; tiers are keyed as the spec joins them. Now and then
/// the key is null, which nothing joins.
fn random_dim_row(rng: &mut Rng, shape: Shape) -> DimRow {
    let (dim, key, attributes) = match rng.below(5) {
        0 => (
            "customers",
            rng.maybe(1, 6, 5),
            vec![text(rng.pick(&["NO", "SE", "DK"])), rng.maybe(10, 13, 30)],
        ),
        1 => {
            let key = if shape == Shape::Chained {
                rng.maybe(10, 13, 5)
            } else {
                rng.maybe(1, 6, 5)
            };
            ("tiers", key, vec![text(rng.pick(&["gold", "silver"]))])
        }
        2 => (
            "tags",
            rng.maybe(1, 5, 5),
            vec![text(rng.pick(&["red", "big", "new"]))],
        ),
        3 => (
            "promos",
            rng.maybe(1, 5, 5),
            vec![text(rng.pick(&["x", "x2"]))],
        ),
        _ => (
            "blocked",
            rng.maybe(1, 6, 5),
            vec![text(rng.pick(&["y", "y2"]))],
        ),
    };
    DimRow {
        dim,
        key: vec![key],
        attributes,
    }
}

/// An ask of the keys the view has: the product is one unless the view
/// is [`Shape::Bound`].
fn random_ask(rng: &mut Rng, shape: Shape) -> Ask {
    let mut by = Vec::new();
    for key in KEYS {
        // Drawn first, so every key draws whether the view has it or not.
        if rng.chance(30) && keyed(shape, key) {
            by.push(key);
        }
    }
    let mut measures = Vec::new();
    for measure in MEASURES.into_iter().chain([FILTERED]) {
        if rng.chance(50) {
            measures.push(measure);
        }
    }
    if measures.is_empty() {
        measures.push("n");
    }
    let pool = [
        condition("country", vec![lit("NO")]),
        condition("label", vec![lit("gold"), lit("silver")]),
        condition("tag", vec![lit("red")]),
        condition("customer", vec![Literal::Int(1), Literal::Int(2)]),
        condition("product", vec![Literal::Int(3)]),
    ];
    let mut filter = Vec::new();
    for condition in pool {
        if rng.chance(20) && keyed(shape, condition.key.as_str()) {
            filter.push(condition);
        }
    }
    let mut ask = ask(&measures, &by, filter);
    if rng.chance(30) {
        let from = START + HOUR * rng.int(0, 24);
        ask.from = Some(from);
        ask.to = Some(from + HOUR * rng.int(1, 24));
    }
    // Now and then rows a day apart in place of the hour's, with bounds
    // on day edges, as a day ask's must be.
    if rng.chance(25) {
        let mut keys = vec![AskKey::WiderBucket {
            name: id("bucket"),
            width: DAY_ROWS,
        }];
        for key in ask.keys {
            if key.name().as_str() != "bucket" {
                keys.push(key);
            }
        }
        ask.keys = keys;
        let from = START + DAY * rng.int(0, 2);
        ask.from = Some(from);
        ask.to = Some(from + DAY * rng.int(1, 3));
    }
    // An open end reads to the buckets held.
    if rng.chance(15) {
        ask.from = None;
    }
    if rng.chance(15) {
        ask.to = None;
    }
    ask
}

/// The rows of a dim the store holds: those under a key that is not
/// null.
fn held(dims: &Dims, dim: &str) -> usize {
    let Some(by_key) = dims.get(dim) else {
        return 0;
    };
    let mut held = 0;
    for (key, rows) in by_key {
        if !key.contains(&Datum::Null) {
            held += rows.len();
        }
    }
    held
}

/// What a random run goes through.
#[derive(Clone, Copy)]
struct Run {
    kinds: Kinds,
    shape: Shape,
    rows_a_key: RowsAKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowsAKey {
    Any,
    /// No dim key holds a second row, so no join repeats a cell.
    One,
}

/// Seeds a test runs, each with the kinds [`kinds_of`] gives it.
const SEEDS: u64 = 8;
/// Dim rows a run starts with, at most.
const FIRST_DIM_ROWS: usize = 30;
const STEPS: u32 = 400;
/// Steps between checks against the reference.
const CHECK_EVERY: u32 = 50;
const ASKS_A_CHECK: usize = 6;

/// Random rows, dims and changes to both, checked against the reference
/// every few steps. Only a
/// [`Shape::Bound`] fact answers from bound cells; the others read the
/// keyed cells through the projections.
fn run(seed: u64, run: Run) {
    let mut walk = Walk::new(seed, run);
    for _ in 0..walk.rng.below(FIRST_DIM_ROWS) {
        walk.add_random_dim_row();
    }
    for step in 0..STEPS {
        walk.take_a_step();
        if step % CHECK_EVERY == CHECK_EVERY - 1 {
            walk.check(step);
        }
    }
    walk.drain();
}

/// A random run under way: the store and, beside it, the rows and dims
/// the reference joins.
struct Walk {
    seed: u64,
    run: Run,
    rng: Rng,
    loaded: Loaded,
    rows: Vec<Vec<Datum>>,
}

impl Walk {
    fn new(seed: u64, run: Run) -> Walk {
        let mut rng = Rng(seed);
        let filter = if rng.chance(50) {
            vec![condition("country", vec![lit("NO"), lit("SE")])]
        } else {
            Vec::new()
        };
        let spec = spec(run.kinds, filter, run.shape);
        Walk {
            seed,
            run,
            rng,
            loaded: load(spec, Dims::new(), &[]),
            rows: Vec::new(),
        }
    }

    /// One change to the rows or the dims.
    fn take_a_step(&mut self) {
        match self.rng.below(40) {
            0..=7 => self.add_random_dim_row(),
            8..=11 => self.remove_random_dim_row(),
            // A dim or the orders truncated.
            12 if self.rng.chance(20) => {
                let row = random_dim_row(&mut self.rng, self.run.shape);
                self.loaded.clear_dim(row.dim);
            }
            13 if self.rng.chance(10) => {
                self.loaded.store.clear("orders");
                self.rows.clear();
            }
            14..=17 if !self.rows.is_empty() => self.remove_random_order(),
            18..=21 if !self.rows.is_empty() => self.update_random_order(),
            _ => self.add_random_order(),
        }
    }

    /// A random dim row, unless every key holds one row at most and its
    /// key holds one already.
    fn add_random_dim_row(&mut self) {
        let row = random_dim_row(&mut self.rng, self.run.shape);
        if self.run.rows_a_key == RowsAKey::One && self.holds_key(&row) {
            return;
        }
        self.loaded.add_dim(row.dim, row.key, row.attributes);
    }

    fn holds_key(&self, row: &DimRow) -> bool {
        match self.loaded.dims.get(row.dim) {
            Some(by_key) => by_key.contains_key(&row.key),
            None => false,
        }
    }

    fn holds_row(&self, row: &DimRow) -> bool {
        let Some(by_key) = self.loaded.dims.get(row.dim) else {
            return false;
        };
        match by_key.get(&row.key) {
            Some(rows) => rows.contains(&row.attributes),
            None => false,
        }
    }

    /// A random dim row removed, if it is held.
    fn remove_random_dim_row(&mut self) {
        let row = random_dim_row(&mut self.rng, self.run.shape);
        if self.holds_row(&row) {
            self.loaded.remove_dim(row.dim, &row.key, &row.attributes);
        }
    }

    fn add_random_order(&mut self) {
        let row = random_order(&mut self.rng);
        self.loaded.store.add_row("orders", &row).unwrap();
        self.rows.push(row);
    }

    fn remove_random_order(&mut self) {
        let row = self.rows.swap_remove(self.rng.below(self.rows.len()));
        self.loaded.store.remove_row("orders", &row).unwrap();
    }

    fn update_random_order(&mut self) {
        let position = self.rng.below(self.rows.len());
        let new = random_order(&mut self.rng);
        self.loaded
            .store
            .update_row("orders", &self.rows[position], &new)
            .unwrap();
        self.rows[position] = new;
    }

    /// The cells the store keeps, the dim rows it holds, and random asks
    /// against the reference.
    fn check(&mut self, step: u32) {
        let Run { kinds, shape, .. } = self.run;
        let seed = self.seed;
        let store = &self.loaded.store;
        let bound = store.bound_cells("orders").is_some();
        assert_eq!(bound, shape == Shape::Bound, "seed {seed}, step {step}");
        for dim in DIM_NAMES {
            assert_eq!(
                store.dim_rows(dim),
                Some(held(&self.loaded.dims, dim)),
                "seed {seed}, step {step}: {dim} holds a row with a null key"
            );
        }
        for _ in 0..ASKS_A_CHECK {
            let ask = random_ask(&mut self.rng, shape);
            let loaded = &self.loaded;
            let expected = reference(&loaded.spec, &ask, &self.rows, &loaded.dims);
            assert_eq!(
                loaded.store.ask(&ask).unwrap(),
                expected,
                "seed {seed}, step {step}, kinds {kinds:?}, {ask:?}"
            );
            windows_match(&loaded.store, &ask, &expected);
        }
    }

    /// Every row removed, and no cell left behind.
    fn drain(self) {
        let Walk {
            seed,
            run,
            loaded,
            rows,
            ..
        } = self;
        let mut store = loaded.store;
        for row in rows {
            store.remove_row("orders", &row).unwrap();
        }
        assert_eq!(
            store.cells("orders"),
            Some(0),
            "seed {seed}: a cell outlived its rows"
        );
        if run.shape == Shape::Bound {
            assert_eq!(
                store.bound_cells("orders"),
                Some(0),
                "seed {seed}: a bound cell outlived its rows"
            );
        }
    }
}

/// [`SEEDS`] seeds of [`run`], each with the kinds [`kinds_of`] gives it.
fn runs(shape: Shape, rows_a_key: RowsAKey) {
    for seed in 0..SEEDS {
        let kinds = kinds_of(seed);
        run(
            seed,
            Run {
                kinds,
                shape,
                rows_a_key,
            },
        );
    }
}

#[test]
fn joins_match_the_naive_reference() {
    runs(Shape::Chained, RowsAKey::Any);
}

#[test]
fn by_key_joins_match_the_naive_reference() {
    runs(Shape::ByKeys, RowsAKey::Any);
}

/// With one row a key no join repeats a cell, so an ask over the keyed
/// cells walks them once each and passes over those an inner, semi or
/// anti join drops.
#[test]
fn one_row_a_key_joins_match_the_naive_reference() {
    runs(Shape::ByKeys, RowsAKey::One);
}

#[test]
fn bound_joins_match_the_naive_reference() {
    runs(Shape::Bound, RowsAKey::Any);
}

/// Left joins alone count a cell once while every dim key holds one
/// row. A key with several rows repeats the cell in the bound cells.
#[test]
fn left_joins_match_the_naive_reference() {
    for seed in 0..SEEDS {
        let left = Run {
            kinds: [RefKind::Left; 5],
            shape: Shape::Bound,
            rows_a_key: RowsAKey::Any,
        };
        run(seed, left);
    }
}
