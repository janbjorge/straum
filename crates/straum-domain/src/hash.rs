//! The hash function behind Straum's maps: the codes a row's values get
//! on their way into a rollup, a bucket's cell index, and the physical
//! stream's pages and transactions. Each word of a key is folded into the
//! hash by one multiply, as the `foldhash` crate does; std's `SipHash`
//! was a fifth of a load. Each map takes a seed from std's random keys,
//! so values from the source can't be picked to collide without knowing
//! it.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasher, Hash, Hasher, RandomState};

/// From the fractional digits of pi, as `foldhash` takes them.
const MULTIPLE: u64 = 0x243f_6a88_85a3_08d3;

/// A `HashMap` hashed with [`Folded`].
pub type FastMap<Key, Value> = HashMap<Key, Value, Seeded>;
/// A `HashSet` hashed with [`Folded`].
pub type FastSet<Item> = HashSet<Item, Seeded>;

/// The room a map keeps however few entries it holds.
const KEPT_ROOM: usize = 128;

/// Gives back the room of `map` once it holds under a quarter of it. Half
/// the room stays free, so a map that comes and goes near the edge does
/// not shrink and grow each time.
pub(crate) fn give_back_room<Key: Eq + Hash, Value>(map: &mut FastMap<Key, Value>) {
    if map.capacity() > KEPT_ROOM && map.len() * 4 < map.capacity() {
        map.shrink_to(map.len() * 2);
    }
}

/// Whether `map` keeps no more room than [`give_back_room`] leaves it.
#[cfg(test)]
pub(crate) fn keeps_its_room<Key, Value>(map: &FastMap<Key, Value>) -> bool {
    map.capacity() <= KEPT_ROOM || map.len() * 4 >= map.capacity()
}

/// The seed of one map, from which each of its hashes starts.
#[derive(Debug)]
pub struct Seeded(u64);

impl Default for Seeded {
    fn default() -> Seeded {
        Seeded(RandomState::new().hash_one(MULTIPLE))
    }
}

impl BuildHasher for Seeded {
    type Hasher = Folded;

    fn build_hasher(&self) -> Folded {
        Folded(self.0)
    }
}

/// One hash being worked out: the seed with each word folded in.
pub struct Folded(u64);

/// The low and high halves of `left` times `right`, folded together.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the product's low and high halves are each wanted"
)]
fn fold(left: u64, right: u64) -> u64 {
    let product = u128::from(left) * u128::from(right);
    (product as u64) ^ ((product >> 64) as u64)
}

impl Hasher for Folded {
    fn write(&mut self, bytes: &[u8]) {
        let (words, rest) = bytes.as_chunks::<8>();
        for &word in words {
            self.write_u64(u64::from_le_bytes(word));
        }
        let mut last = [0; 8];
        last[..rest.len()].copy_from_slice(rest);
        // The length keeps "a" and "a\0" apart.
        self.write_u64(u64::from_le_bytes(last) ^ (rest.len() as u64) << 56);
    }

    fn write_u8(&mut self, byte: u8) {
        self.write_u64(byte.into());
    }

    fn write_u32(&mut self, word: u32) {
        self.write_u64(word.into());
    }

    fn write_u64(&mut self, word: u64) {
        self.0 = fold(self.0 ^ word, MULTIPLE);
    }

    fn write_usize(&mut self, word: usize) {
        self.write_u64(word as u64);
    }

    fn finish(&self) -> u64 {
        // A last round, so the low bits hashbrown buckets by mix in the
        // high ones.
        fold(self.0, MULTIPLE).rotate_left(26)
    }
}
