//! Helpers the tests share.

// Every test binary compiles this module and uses only some of it.
#![allow(dead_code)]

use std::collections::HashMap;

use straum_domain::{
    Aggregate, Answer, Ask, Attribute, ByKey, Condition, Datum, Fraction, Identifier, Literal,
    Sealed, SealedId, Store, Table, Value,
};

pub fn percentile(units: u64, places: u32, column: &str) -> Aggregate {
    Aggregate::PercentileDisc {
        fraction: Fraction::new(units, places).unwrap(),
        column: id(column),
    }
}

/// PostgreSQL's `percentile_disc`: of the values sorted, the one at
/// `ceil(fraction * n)` counting from 1, the product in float8.
#[expect(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "PostgreSQL takes the product in float8 too, and the rank is from 0 to the count"
)]
pub fn percentile_disc(mut values: Vec<i128>, fraction: Fraction) -> Value {
    values.sort_unstable();
    let fraction = fraction.units as f64 / 10f64.powi(i32::try_from(fraction.places).unwrap());
    let rank = (fraction * values.len() as f64).ceil() as usize;
    match values.get(rank.max(1) - 1) {
        Some(&value) => Value::Int(value),
        None => Value::Null,
    }
}

pub fn id(name: &str) -> Identifier {
    Identifier::new(name).unwrap()
}

pub fn by_key(name: &str) -> ByKey {
    ByKey::Column {
        column: id(name),
        name: id(name),
    }
}

pub fn attribute(name: &str, column: &str) -> Attribute {
    Attribute {
        name: id(name),
        column: id(column),
    }
}

pub fn text(value: &str) -> Datum {
    Datum::Text(value.into())
}

pub fn lit(value: &str) -> Literal {
    Literal::Text(value.into())
}

pub fn condition(key: &str, values: Vec<Literal>) -> Condition {
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
pub fn dim_row(key: &[Datum], attributes: &[Datum]) -> Vec<Datum> {
    [key, attributes].concat()
}

/// SQL `=`: null matches nothing.
pub fn holds(value: &Datum, values: &[Literal]) -> bool {
    values.iter().any(|literal| match (value, literal) {
        (Datum::Int(found), Literal::Int(wanted)) => found == wanted,
        (Datum::Text(found), Literal::Text(wanted)) => **found == **wanted,
        (Datum::Bool(found), Literal::Bool(wanted)) => found == wanted,
        _ => false,
    })
}

/// Of the buckets [`Spilled::seal`] may seal, the share it does seal.
const SEALED_PERCENT: usize = 70;

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
        usize::try_from(self.next() % n as u64).unwrap()
    }

    pub fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    pub fn int(&mut self, lo: i64, hi: i64) -> i64 {
        let offset = self.next() % hi.abs_diff(lo);
        lo.checked_add_unsigned(offset).unwrap()
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

/// The sealed buckets of one fact kept aside, as a spill keeps them.
pub struct Spilled {
    fact: &'static str,
    buckets: HashMap<SealedId, Sealed>,
    /// Sealed buckets asks read.
    pub read: usize,
}

impl Spilled {
    pub fn new(fact: &'static str) -> Spilled {
        Spilled {
            fact,
            buckets: HashMap::new(),
            read: 0,
        }
    }

    /// Seals every bucket that ends by `before`, or some of them.
    pub fn seal(&mut self, rng: &mut Rng, store: &mut Store, before: i64) {
        for id in store.sealable(before) {
            if rng.chance(SEALED_PERCENT) {
                let sealed = store.sealed_copy(&id).unwrap();
                store.seal(&id).unwrap();
                self.buckets.insert(id, sealed);
            }
        }
    }

    /// Unseals the buckets a row falls in, as the writer does before it
    /// applies a change.
    pub fn unseal(&mut self, store: &mut Store, row: &[Datum]) {
        for id in store.sealed_touched(self.fact, row) {
            let thawed = store.thaw(self.buckets.remove(&id).unwrap()).unwrap();
            store.unseal(thawed).unwrap();
        }
    }

    /// Forgets every bucket, as the writer does when the fact is
    /// truncated.
    pub fn forget(&mut self) {
        self.buckets.clear();
    }

    /// The answer, and that the buckets kept are the ones the store has
    /// sealed and no more.
    pub fn ask(&mut self, store: &Store, ask: &Ask) -> Answer {
        let mut kept: Vec<&SealedId> = self.buckets.keys().collect();
        kept.sort();
        assert_eq!(kept, store.sealed(self.fact).iter().collect::<Vec<_>>());
        let thawed: Vec<_> = (store.sealed_read(ask).unwrap().iter())
            .map(|id| store.thaw(self.buckets[id].clone()).unwrap())
            .collect();
        self.read += thawed.len();
        store.ask_with(ask, &thawed).unwrap()
    }
}
