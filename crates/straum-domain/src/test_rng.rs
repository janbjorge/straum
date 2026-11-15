//! The random numbers the unit tests draw: splitmix64 from a fixed seed,
//! so a failing case fails the same way every run. The tests need no
//! crate for randomness. `tests/common` has its own copy, since the tests
//! there build against the crate from outside and can't reach this one.

/// A splitmix64 sequence from a seed.
pub(crate) struct Rng {
    state: u64,
}

impl Rng {
    /// The sequence that starts from `seed`.
    pub(crate) fn new(seed: u64) -> Rng {
        Rng { state: seed }
    }

    /// The next number of the sequence.
    pub(crate) fn next(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut mixed = self.state;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        mixed ^ (mixed >> 31)
    }

    /// A number from 0 up to, not including, `bound`.
    pub(crate) fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }

    /// A position in a list of `length` items.
    pub(crate) fn position_below(&mut self, length: usize) -> usize {
        usize::try_from(self.below(length as u64)).expect("below a usize")
    }
}
