//! Finds a cell of a bucket by its key. A bucket keeps its cells side by
//! side in columns; this index maps a cell key (a few codes) to the
//! cell's place among them. A row added looks its cell up here, a new cell
//! is put in, and a removed cell's place is taken by the last one.

use std::hash::{BuildHasher, Hasher};

use crate::hash::Seeded;

/// No cell.
const EMPTY: u32 = u32::MAX;

/// The keys of a bucket's cells, end to end, `codes_per_key` codes each.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Keys<'bucket> {
    // Borrowed from the bucket, which keeps every key once.
    pub(crate) codes: &'bucket [u32],
    pub(crate) codes_per_key: usize,
}

impl<'bucket> Keys<'bucket> {
    /// The key of the cell at `cell`.
    pub(crate) fn of(self, cell: usize) -> &'bucket [u32] {
        &self.codes[cell * self.codes_per_key..][..self.codes_per_key]
    }
}

/// Open addressing over the cells' places: a slot holds a cell's place,
/// and the key column says whose it is. A map from keys would hold every
/// key a second time, each in an allocation of its own behind a pointer
/// four times its size.
#[derive(Debug, Default)]
pub(crate) struct Index {
    /// None, or a power of two of them, at most three quarters taken: a
    /// key looked for goes from its home slot on until it or no cell.
    slots: Vec<u32>,
    seed: Seeded,
}

/// Whether two keys of a bucket are one. Compared as slices, two or three
/// codes are a call.
fn same(left: &[u32], right: &[u32]) -> bool {
    for (left_code, right_code) in left.iter().zip(right) {
        if left_code != right_code {
            return false;
        }
    }
    true
}

impl Index {
    fn home(&self, key: &[u32]) -> usize {
        // A round a code. Hashed as a slice, a key has its length mixed
        // in and its bytes copied out first, and a bucket's keys are all
        // of one length.
        let mut hasher = self.seed.build_hasher();
        for &code in key {
            hasher.write_u32(code);
        }
        #[expect(
            clippy::cast_possible_truncation,
            reason = "the mask keeps only low bits"
        )]
        let hash = hasher.finish() as usize;
        hash & (self.slots.len() - 1)
    }

    fn next(&self, slot: usize) -> usize {
        (slot + 1) & (self.slots.len() - 1)
    }

    /// The place of the cell with `key`, if any cell has it.
    pub(crate) fn get(&self, key: &[u32], keys: Keys<'_>) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let mut slot = self.home(key);
        loop {
            match self.slots[slot] {
                EMPTY => return None,
                cell if same(keys.of(cell as usize), key) => return Some(cell as usize),
                _ => slot = self.next(slot),
            }
        }
    }

    /// Takes in the newest cell, the one at `cell`, whose key no other
    /// holds.
    pub(crate) fn insert(&mut self, cell: usize, keys: Keys<'_>) {
        if (cell + 1) * 4 > self.slots.len() * 3 {
            self.slots = vec![EMPTY; (self.slots.len() * 2).max(4)];
            for held in 0..cell {
                self.place(held, keys);
            }
        }
        self.place(cell, keys);
    }

    fn place(&mut self, cell: usize, keys: Keys<'_>) {
        let mut slot = self.home(keys.of(cell));
        while self.slots[slot] != EMPTY {
            slot = self.next(slot);
        }
        self.slots[slot] = u32::try_from(cell).expect("a bucket's cells are counted in four bytes");
    }

    fn slot_of(&self, cell: usize, keys: Keys<'_>) -> usize {
        let mut slot = self.home(keys.of(cell));
        while self.slots[slot] as usize != cell {
            slot = self.next(slot);
        }
        slot
    }

    /// Forgets the cell at `cell`, whose key is still among `keys`.
    pub(crate) fn remove(&mut self, cell: usize, keys: Keys<'_>) {
        let mask = self.slots.len() - 1;
        let mut hole = self.slot_of(cell, keys);
        let mut slot = self.next(hole);
        // A cell is looked for from its home on, so none may lie past an
        // empty slot: each that the hole would cut off moves into it.
        while self.slots[slot] != EMPTY {
            let home_slot = self.home(keys.of(self.slots[slot] as usize));
            if slot.wrapping_sub(home_slot) & mask >= slot.wrapping_sub(hole) & mask {
                self.slots[hole] = self.slots[slot];
                hole = slot;
            }
            slot = self.next(slot);
        }
        self.slots[hole] = EMPTY;
    }

    /// The cell at `from` lies at `to` from now on.
    pub(crate) fn moved(&mut self, from: usize, to: usize, keys: Keys<'_>) {
        let slot = self.slot_of(from, keys);
        self.slots[slot] = u32::try_from(to).expect("less than `from`");
    }

    /// Takes in `cells` cells at once. False if two of them hold one key.
    pub(crate) fn fill(&mut self, cells: usize, keys: Keys<'_>) -> bool {
        self.slots = vec![EMPTY; (cells * 4).div_ceil(3).next_power_of_two().max(4)];
        for cell in 0..cells {
            if self.get(keys.of(cell), keys).is_some() {
                return false;
            }
            self.place(cell, keys);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    use crate::test_rng::Rng;

    /// A key of `codes_per_key` codes, each below `values`.
    fn key_below(rng: &mut Rng, codes_per_key: usize, values: usize) -> Vec<u32> {
        let mut key = Vec::with_capacity(codes_per_key);
        for _ in 0..codes_per_key {
            key.push(u32::try_from(rng.position_below(values)).expect("codes fit u32"));
        }
        key
    }

    /// A bucket's keys and their index, beside a map from key to place.
    struct Followed {
        codes_per_key: usize,
        index: Index,
        codes: Vec<u32>,
        held: HashMap<Vec<u32>, usize>,
    }

    impl Followed {
        fn keys(&self) -> Keys<'_> {
            Keys {
                codes: &self.codes,
                codes_per_key: self.codes_per_key,
            }
        }

        /// A new cell takes the next place.
        fn arrive(&mut self, key: Vec<u32>) {
            let cell = self.held.len();
            self.codes.extend(&key);
            let keys = Keys {
                codes: &self.codes,
                codes_per_key: self.codes_per_key,
            };
            self.index.insert(cell, keys);
            self.held.insert(key, cell);
        }

        /// The cell at `cell` leaves, and the last cell takes its place.
        fn leave(&mut self, key: &[u32], cell: usize) {
            let codes_per_key = self.codes_per_key;
            let last = self.held.len() - 1;
            let keys = Keys {
                codes: &self.codes,
                codes_per_key,
            };
            self.index.remove(cell, keys);
            self.held.remove(key);
            if cell != last {
                self.index.moved(last, cell, keys);
                let moved = self
                    .held
                    .get_mut(keys.of(last))
                    .expect("the last key is held");
                *moved = cell;
                let last_codes = last * codes_per_key..(last + 1) * codes_per_key;
                self.codes.copy_within(last_codes, cell * codes_per_key);
            }
            self.codes.truncate(last * codes_per_key);
        }
    }

    /// Cells come and go as a bucket's do, the last taking the place of
    /// one that leaves, against a map from key to place: every key held
    /// is found where it lies, and no other is found at all.
    #[test]
    fn places_follow_a_map_as_cells_come_and_go() {
        for (codes_per_key, values) in [(1, 40), (1, 3_000), (3, 6), (0, 1)] {
            let mut rng = Rng::new(3);
            let mut followed = Followed {
                codes_per_key,
                index: Index::default(),
                codes: Vec::new(),
                held: HashMap::new(),
            };
            for step in 0..20_000 {
                let at = format!("{codes_per_key} codes of {values}, step {step}");
                let key = key_below(&mut rng, codes_per_key, values);
                let found = followed.index.get(&key, followed.keys());
                let place = followed.held.get(&key).copied();
                assert_eq!(found, place, "{at}");
                // More come than go until most keys are held, then as many.
                let arrives = rng.position_below(100) < if step < 8_000 { 70 } else { 45 };
                match (place, arrives) {
                    (None, true) => followed.arrive(key),
                    (Some(cell), false) => followed.leave(&key, cell),
                    _ => {}
                }
                if step % 997 == 0 {
                    for (key, &cell) in &followed.held {
                        let found = followed.index.get(key, followed.keys());
                        assert_eq!(found, Some(cell), "{at}: {key:?}");
                    }
                }
            }
            // At most three quarters of the slots are taken.
            assert!(followed.held.len() * 4 <= followed.index.slots.len() * 3);
        }
    }

    /// A thawed bucket's cells are taken in at once, and a key that two of
    /// them hold is found out.
    #[test]
    fn cells_taken_in_at_once_hold_a_key_each() {
        let mut rng = Rng::new(9);
        for cells in [0, 1, 2, 3, 4, 100, 5_000] {
            let cell_count = u32::try_from(cells).expect("a bucket's cells fit u32");
            let mut codes: Vec<u32> = Vec::with_capacity(2 * cells);
            for cell in 0..cell_count {
                codes.extend([cell / 7, cell % 7]);
            }
            for at in (1..cells).rev() {
                let with = rng.position_below(at + 1);
                codes.swap(2 * at, 2 * with);
                codes.swap(2 * at + 1, 2 * with + 1);
            }
            let mut index = Index::default();
            let keys = Keys {
                codes: &codes,
                codes_per_key: 2,
            };
            assert!(index.fill(cells, keys), "{cells} cells");
            for cell in 0..cells {
                assert_eq!(index.get(keys.of(cell), keys), Some(cell), "{cells} cells");
            }
            assert_eq!(index.get(&[u32::MAX, 0], keys), None, "{cells} cells");
            if cells > 1 {
                let mut twice = codes.clone();
                let (from, to) = (rng.position_below(cells), rng.position_below(cells - 1));
                let to = to + usize::from(to >= from);
                twice.copy_within(2 * from..2 * from + 2, 2 * to);
                let keys = Keys {
                    codes: &twice,
                    codes_per_key: 2,
                };
                assert!(!Index::default().fill(cells, keys), "{cells} cells");
            }
        }
    }
}
