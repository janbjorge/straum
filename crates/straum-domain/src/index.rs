//! Where each cell of a bucket lies among its columns, found by its key.

use std::hash::{BuildHasher, Hasher};

use crate::hash::Seeded;

/// No cell.
const EMPTY: u32 = u32::MAX;

/// The keys of a bucket's cells, end to end, `len` codes each.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Keys<'a> {
    pub(crate) codes: &'a [u32],
    pub(crate) len: usize,
}

impl<'a> Keys<'a> {
    pub(crate) fn of(self, cell: usize) -> &'a [u32] {
        &self.codes[cell * self.len..][..self.len]
    }
}

/// A slot holds a cell's place, and the key column says whose it is. A
/// map from keys would hold every key a second time, each in an
/// allocation of its own behind a pointer four times its size.
#[derive(Debug, Default)]
pub(crate) struct Index {
    /// None, or a power of two of them, at most three quarters taken: a
    /// key looked for goes from its home slot on until it or no cell.
    slots: Vec<u32>,
    seed: Seeded,
}

/// Whether two keys of a bucket are one. Compared as slices, two or three
/// codes are a call.
fn same(a: &[u32], b: &[u32]) -> bool {
    a.iter().zip(b).all(|(a, b)| a == b)
}

impl Index {
    fn home(&self, key: &[u32]) -> usize {
        // A round a code. Hashed as a slice, a key has its length mixed
        // in and its bytes copied out first, and a bucket's keys are all
        // of one length.
        let mut hasher = self.seed.build_hasher();
        key.iter().for_each(|&code| hasher.write_u32(code));
        hasher.finish() as usize & (self.slots.len() - 1)
    }

    fn next(&self, slot: usize) -> usize {
        (slot + 1) & (self.slots.len() - 1)
    }

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
            (0..cell).for_each(|held| self.place(held, keys));
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
            let home = self.home(keys.of(self.slots[slot] as usize));
            if slot.wrapping_sub(home) & mask >= slot.wrapping_sub(hole) & mask {
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
        (0..cells).all(|cell| {
            let new = self.get(keys.of(cell), keys).is_none();
            if new {
                self.place(cell, keys);
            }
            new
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    /// splitmix64; the tests need no crate for randomness.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (z ^ (z >> 31)) as usize % n
        }
    }

    /// Cells come and go as a bucket's do, the last taking the place of
    /// one that leaves, against a map from key to place: every key held
    /// is found where it lies, and no other is found at all.
    #[test]
    fn places_follow_a_map_as_cells_come_and_go() {
        for (key_len, values) in [(1, 40), (1, 3_000), (3, 6), (0, 1)] {
            let mut rng = Rng(3);
            let mut index = Index::default();
            let mut codes: Vec<u32> = Vec::new();
            let mut held: HashMap<Vec<u32>, usize> = HashMap::new();
            for step in 0..20_000 {
                let at = format!("{key_len} codes of {values}, step {step}");
                let key: Vec<u32> = (0..key_len).map(|_| rng.below(values) as u32).collect();
                let keys = Keys {
                    codes: &codes,
                    len: key_len,
                };
                assert_eq!(index.get(&key, keys), held.get(&key).copied(), "{at}");
                // More come than go until most keys are held, then as many.
                let comes = rng.below(100) < if step < 8_000 { 70 } else { 45 };
                match (held.get(&key).copied(), comes) {
                    (None, true) => {
                        let cell = held.len();
                        codes.extend(&key);
                        let keys = Keys {
                            codes: &codes,
                            len: key_len,
                        };
                        index.insert(cell, keys);
                        held.insert(key, cell);
                    }
                    (Some(cell), false) => {
                        let last = held.len() - 1;
                        index.remove(cell, keys);
                        held.remove(&key);
                        if cell != last {
                            index.moved(last, cell, keys);
                            *held.get_mut(keys.of(last)).unwrap() = cell;
                            codes.copy_within(last * key_len..(last + 1) * key_len, cell * key_len);
                        }
                        codes.truncate(last * key_len);
                    }
                    _ => {}
                }
                if step % 997 == 0 {
                    let keys = Keys {
                        codes: &codes,
                        len: key_len,
                    };
                    for (key, &cell) in &held {
                        assert_eq!(index.get(key, keys), Some(cell), "{at}: {key:?}");
                    }
                }
            }
            // At most three quarters of the slots are taken.
            assert!(held.len() * 4 <= index.slots.len() * 3);
        }
    }

    /// A thawed bucket's cells are taken in at once, and a key that two of
    /// them hold is found out.
    #[test]
    fn cells_taken_in_at_once_hold_a_key_each() {
        let mut rng = Rng(9);
        for cells in [0, 1, 2, 3, 4, 100, 5_000] {
            let mut codes: Vec<u32> = (0..cells as u32).flat_map(|n| [n / 7, n % 7]).collect();
            for at in (1..cells).rev() {
                let with = rng.below(at + 1);
                codes.swap(2 * at, 2 * with);
                codes.swap(2 * at + 1, 2 * with + 1);
            }
            let mut index = Index::default();
            let keys = Keys {
                codes: &codes,
                len: 2,
            };
            assert!(index.fill(cells, keys), "{cells} cells");
            for cell in 0..cells {
                assert_eq!(index.get(keys.of(cell), keys), Some(cell), "{cells} cells");
            }
            assert_eq!(index.get(&[u32::MAX, 0], keys), None, "{cells} cells");
            if cells > 1 {
                let mut twice = codes.clone();
                let (from, to) = (rng.below(cells), rng.below(cells - 1));
                let to = to + usize::from(to >= from);
                twice.copy_within(2 * from..2 * from + 2, 2 * to);
                let keys = Keys {
                    codes: &twice,
                    len: 2,
                };
                assert!(!Index::default().fill(cells, keys), "{cells} cells");
            }
        }
    }
}
