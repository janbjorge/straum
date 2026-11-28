//! The random workload: trips and dim rows added, updated and removed,
//! now and then an update with a broken new row that must fail, the
//! trips truncated halfway; every so often the store answers random asks and each must equal the
//! reference. Also rows loaded in batches against rows added one by one,
//! and two views of the trips in one store, sharing their dims.

use crate::common::{Rng, dim_row, id, text, windows_match};
use crate::random::{
    change_dim, held, load_dims, meet_a_group, near, random_ask, random_dims, trip, trips,
};
use crate::reference::reference;
use crate::views::{spec, spec_joining, trip_types, typed};
use crate::{ASKS_PER_CHECK, CHECK_EVERY, Dims, FARE, KEYS, ROW_COLUMNS, SEEDS, STEPS, trips_ask};
use straum_domain::{AskKey, Datum, Identifier, RefKind, RowError, Spec, Store};

/// The second view of [`Views::Two`], a copy of the trips view.
const TWIN: &str = "twins";

/// The views the store holds: the trips, or the trips and a copy of
/// them, each keeping bound cells on the same dims.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Views {
    One,
    Two,
}

/// Random changes to trips and dims, and asks against the reference.
/// Asks read the bound cells, which move with the dims. Halfway the
/// trips are truncated. `vend` is how the vendors join.
fn run(seed: u64, vend: RefKind, views: Views) {
    let mut workload = Workload::new(seed, vend, views);
    for step in 0..STEPS {
        if step == STEPS / 2 {
            workload.truncate_trips();
        }
        workload.change_one();
        if step % CHECK_EVERY == CHECK_EVERY - 1 {
            workload.check(step);
        }
    }
    workload.finish();
}

/// The store under test, and the live rows and dims the reference reads.
struct Workload {
    seed: u64,
    /// The views each change and ask goes to.
    views: Vec<Identifier>,
    spec: Spec,
    store: Store,
    rng: Rng,
    rows: Vec<Vec<Datum>>,
    dims: Dims,
}

impl Workload {
    fn new(seed: u64, vend: RefKind, views: Views) -> Workload {
        let mut spec = spec_joining(vend);
        let mut names = vec![id("trips")];
        if views == Views::Two {
            let mut twin = spec.fact("trips").unwrap().clone();
            twin.name = id(TWIN);
            twin.table.name = id(TWIN);
            let mut facts = spec.facts().to_vec();
            facts.push(twin);
            spec = Spec::new(spec.dims().to_vec(), facts).unwrap();
            names.push(id(TWIN));
        }
        let mut store = typed(&spec);
        if views == Views::Two {
            store.set_types(TWIN, &trip_types()).unwrap();
        }
        let mut rng = Rng(seed);
        let dims = random_dims(&mut rng);
        load_dims(&mut store, &dims);
        Workload {
            seed,
            views: names,
            spec,
            store,
            rng,
            rows: Vec::new(),
            dims,
        }
    }

    fn truncate_trips(&mut self) {
        let seed = self.seed;
        for view in &self.views {
            let view = view.as_str();
            self.store.clear(view);
            assert_eq!(self.store.cells(view), Some(0), "seed {seed}");
            assert_eq!(self.store.bound_cells(view), Some(0), "seed {seed}");
        }
        self.rows.clear();
    }

    /// One random change: to a dim, or a trip removed, updated or added.
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
        for view in &self.views {
            self.store.remove_row(view.as_str(), &row).unwrap();
        }
    }

    fn update_trip(&mut self) {
        // Often into the same cell, or the one beside it.
        let index = self.rng.below(self.rows.len());
        let mut new = trip(&mut self.rng);
        if self.rng.chance(50) {
            near(&mut self.rng, &mut new, &self.rows[index]);
        }
        // Now and then a broken new row. The update fails, and the old
        // row stays in the store and in the reference.
        if self.rng.chance(5) {
            let (broken, error) = if self.rng.chance(50) {
                new[FARE] = text("dear");
                (&new[..], RowError::NotInt { column: id("fare") })
            } else {
                let arity = RowError::Arity {
                    expected: ROW_COLUMNS,
                    found: ROW_COLUMNS - 1,
                };
                (&new[..ROW_COLUMNS - 1], arity)
            };
            for view in &self.views {
                let old = &self.rows[index];
                let updated = self.store.update_row(view.as_str(), old, broken);
                assert_eq!(updated, Err(error.clone()), "seed {}", self.seed);
            }
            return;
        }
        for view in &self.views {
            let old = &self.rows[index];
            self.store.update_row(view.as_str(), old, &new).unwrap();
        }
        self.rows[index] = new;
    }

    fn add_trip(&mut self) {
        let mut row = trip(&mut self.rng);
        // Often into a cell that already holds rows, or beside it.
        if !self.rows.is_empty() && self.rng.chance(40) {
            let other = self.rng.below(self.rows.len());
            near(&mut self.rng, &mut row, &self.rows[other]);
        }
        for view in &self.views {
            self.store.add_row(view.as_str(), &row).unwrap();
        }
        self.rows.push(row);
    }

    /// The dims hold the rows they should, and random asks answer as the
    /// reference does.
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
            let mut ask = random_ask(&mut self.rng, &self.spec);
            let mut every_group = ask.clone();
            every_group.having.clear();
            let groups = reference(&self.spec, &every_group, &self.rows, &self.dims);
            meet_a_group(&mut self.rng, &mut ask, &groups);
            let expected = reference(&self.spec, &ask, &self.rows, &self.dims);
            for view in &self.views {
                ask.view = view.clone();
                assert_eq!(
                    self.store.ask(&ask).unwrap(),
                    expected,
                    "seed {seed}, step {step}, {ask:?}"
                );
                windows_match(&self.store, &ask, &expected);
            }
        }
    }

    /// Takes every trip out of each view again, and with it every cell.
    fn finish(mut self) {
        let seed = self.seed;
        for view in &self.views {
            let view = view.as_str();
            assert!(self.store.cells(view).unwrap() > 0);
            // Home zones of one borough share bound cells, so these are
            // fewer.
            let keyed = self.store.cells(view);
            let bound = self.store.bound_cells(view);
            assert!(bound < keyed, "seed {seed}: {bound:?} bound of {keyed:?}");
            for row in &self.rows {
                self.store.remove_row(view, row).unwrap();
            }
            assert_eq!(
                self.store.cells(view).unwrap(),
                0,
                "seed {seed}: a cell of {view} outlived its rows"
            );
            assert_eq!(
                self.store.bound_cells(view),
                Some(0),
                "seed {seed}: a bound cell of {view} outlived its rows"
            );
        }
    }
}

#[test]
fn store_matches_naive_reference() {
    for seed in 0..SEEDS {
        run(seed, RefKind::Left, Views::One);
    }
}

/// With an inner join a bucket whose trips name no vendor holds no bound
/// cells, and gains them when the vendor's row comes.
#[test]
fn inner_store_matches_naive_reference() {
    for seed in 0..SEEDS {
        run(seed, RefKind::Inner, Views::One);
    }
}

/// Two views keep bound cells on the same dims: a dim's change moves the
/// bound cells of each.
#[test]
fn two_views_on_shared_dims_match_naive_reference() {
    // Fewer seeds: each step changes and asks twice as much.
    for seed in 0..SEEDS / 4 {
        run(seed, RefKind::Left, Views::Two);
    }
}

/// Adds `rows` laid end to end, as a load does, in batches of random
/// sizes.
fn add_in_batches(rng: &mut Rng, store: &mut Store, rows: &[Vec<Datum>]) {
    let flat: Vec<Datum> = rows.concat();
    let mut rest = &flat[..];
    while !rest.is_empty() {
        let batch_rows = 1 + rng.below(300);
        let (batch, after) = rest.split_at(rest.len().min(batch_rows * ROW_COLUMNS));
        store.add_rows("trips", batch).unwrap();
        rest = after;
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
        let rows = trips(&mut rng, 2_000);
        for row in &rows {
            by_row.add_row("trips", row).unwrap();
        }
        add_in_batches(&mut rng, &mut together, &rows);
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

/// Every key at once, so the group table is sparse and a cell's group is
/// numbered as it is met.
#[test]
fn every_key_at_once_uses_the_sparse_index() {
    let spec = spec();
    let mut store = typed(&spec);
    let mut rng = Rng(9);
    let dims = random_dims(&mut rng);
    load_dims(&mut store, &dims);
    let rows = trips(&mut rng, 500);
    for row in &rows {
        store.add_row("trips", row).unwrap();
    }
    let mut keys = Vec::with_capacity(KEYS.len());
    for key in KEYS {
        keys.push(AskKey::Key(id(key)));
    }
    let ask = trips_ask(&["trips", "riders"], keys);
    assert_eq!(
        store.ask(&ask).unwrap(),
        reference(&spec, &ask, &rows, &dims)
    );
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
    let rows = trips(&mut rng, 300);
    for row in &rows {
        together.add_row("trips", row).unwrap();
        by_row.add_row("trips", row).unwrap();
    }
    for _ in 0..20 {
        let ask = random_ask(&mut rng, &spec);
        assert_eq!(together.ask(&ask).unwrap(), by_row.ask(&ask).unwrap());
    }
}
