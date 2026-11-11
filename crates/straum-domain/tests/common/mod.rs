//! Helpers the store and join tests share.

use straum_domain::{Condition, Datum, Identifier, Literal, Table};

pub fn id(s: &str) -> Identifier {
    Identifier::new(s).unwrap()
}

pub fn text(s: &str) -> Datum {
    Datum::Text(s.into())
}

pub fn lit(s: &str) -> Literal {
    Literal::Text(s.into())
}

pub fn cond(key: &str, values: Vec<Literal>) -> Condition {
    Condition {
        key: id(key),
        values,
    }
}

pub fn table(name: &str) -> Table {
    Table {
        schema: id("public"),
        name: id(name),
    }
}

/// A dim row as a load reads it: the key, then the attributes.
pub fn dim_row(key: &[Datum], attrs: &[Datum]) -> Vec<Datum> {
    [key, attrs].concat()
}

/// SQL `=`: null matches nothing.
pub fn holds(value: &Datum, values: &[Literal]) -> bool {
    values.iter().any(|l| match (value, l) {
        (Datum::Int(a), Literal::Int(b)) => a == b,
        (Datum::Text(a), Literal::Text(b)) => **a == **b,
        (Datum::Bool(a), Literal::Bool(b)) => a == b,
        _ => false,
    })
}

/// splitmix64; the tests need no crate for randomness.
pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    pub fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    pub fn int(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() % (hi - lo) as u64) as i64
    }

    /// An integer in `lo..hi`, or null `percent` percent of the time.
    pub fn maybe(&mut self, lo: i64, hi: i64, percent: usize) -> Datum {
        if self.chance(percent) {
            Datum::Null
        } else {
            Datum::Int(self.int(lo, hi))
        }
    }

    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }
}
