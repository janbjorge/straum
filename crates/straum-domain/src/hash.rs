//! The hasher of the maps a row probes on its way into a rollup. Std's
//! SipHash was a fifth of a load. A folded multiply, as `foldhash` does,
//! with a seed per map from std's random keys, so values from the source
//! can't be picked to collide without knowing it.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher, RandomState};

/// From the fractional digits of pi, as `foldhash` takes them.
const MULTIPLE: u64 = 0x243f_6a88_85a3_08d3;

pub(crate) type FastMap<K, V> = HashMap<K, V, Seeded>;

pub(crate) struct Seeded(u64);

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

pub(crate) struct Folded(u64);

fn fold(a: u64, b: u64) -> u64 {
    let product = u128::from(a) * u128::from(b);
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

    fn write_u8(&mut self, n: u8) {
        self.write_u64(n.into());
    }

    fn write_u64(&mut self, n: u64) {
        self.0 = fold(self.0 ^ n, MULTIPLE);
    }

    fn write_usize(&mut self, n: usize) {
        self.write_u64(n as u64);
    }

    fn finish(&self) -> u64 {
        // A last round, so the low bits hashbrown buckets by mix in the
        // high ones.
        fold(self.0, MULTIPLE).rotate_left(26)
    }
}
