//! The random workload: trips and dim rows added, updated and removed, the
//! trips truncated halfway, and buckets sealed and unsealed at random; every
//! so often the store answers random asks and each must equal the
//! reference. Also rows loaded in batches against rows added one by one.

use crate::common::{Rng, Spilled, dim_row, id};
use crate::random::{
    change_dim, held, load_dims, meet_a_group, near, random_ask, random_dims, trip, trips,
};
use crate::reference::reference;
use crate::views::{spec, spec_joining, typed};
use crate::{ASKS_PER_CHECK, CHECK_EVERY, Dims, END, KEYS, ROW_COLUMNS, SEEDS, START, STEPS, WEEK};
use straum_domain::{Ask, AskKey, Datum, RefKind, Spec, Store};

/// Whether the workload seals buckets as it goes.
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

/// The store under test, and the live rows and dims the reference reads.
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

    /// Seals some of the buckets that end before a random time.
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
            assert_eq!(
                self.spilled.ask(&self.store, &ask),
                reference(&self.spec, &ask, &self.rows, &self.dims),
                "seed {seed}, step {step}, {ask:?}"
            );
        }
    }

    /// Takes every trip out again, and with it every cell; the sealed
    /// buckets asks read.
    fn finish(mut self, sealing: Sealing) -> usize {
        let seed = self.seed;
        assert!(self.store.cells("trips").unwrap() > 0);
        // Home zones of one borough share bound cells, so these are fewer.
        if sealing == Sealing::Never {
            let keyed = self.store.cells("trips");
            let bound = self.store.bound_cells("trips");
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

/// The sealed buckets asks read over every seed, sealing at random, the
/// vendors joined as `vend` says.
fn sealed_buckets_read(vend: RefKind) -> usize {
    let mut read = 0;
    for seed in 0..SEEDS {
        read += run(seed, Sealing::AtRandom, vend);
    }
    read
}

/// A sealed bucket has no bound cells, so an ask over one reads the keyed
/// cells, thawed; the rest read the bound cells.
#[test]
fn sealed_store_matches_naive_reference() {
    let read = sealed_buckets_read(RefKind::Left);
    assert!(read > 1_000, "asks read {read} sealed buckets");
}

/// With an inner join a bucket whose trips name no vendor holds no bound
/// cells when it is sealed, and gains them when the vendor's row comes.
#[test]
fn sealed_inner_store_matches_naive_reference() {
    let read = sealed_buckets_read(RefKind::Inner);
    assert!(read > 1_000, "asks read {read} sealed buckets");
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
    let ask = Ask {
        view: id("trips"),
        measures: vec![id("trips"), id("riders")],
        keys,
        filter: Vec::new(),
        having: Vec::new(),
        from: START,
        to: END,
    };
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
