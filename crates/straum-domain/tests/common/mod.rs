//! Helpers the test binaries share: builders for spec pieces, the SQL
//! rules a reference needs (`=` and `percentile_disc`), a seeded random
//! number source, and `Spilled`, which seals and thaws buckets beside a
//! store the way the writer and a spill do.

// Every test binary compiles this module and uses only some of it.
// `#[expect(dead_code)]` would fail in a binary that uses all of it,
// so this is the one `allow` AGENTS.md permits.
#![allow(dead_code)]

use std::collections::HashMap;

use straum_domain::{
    Aggregate, Answer, Ask, Attribute, ByKey, Condition, Datum, Fraction, Identifier, Literal,
    Sealed, SealedId, Store, Table, Value,
};

/// `percentile_disc(units / 10^places)` over `column`.
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

/// An identifier known to be valid.
pub fn id(name: &str) -> Identifier {
    Identifier::new(name).unwrap()
}

/// A by key that reads the column of the same name.
pub fn by_key(name: &str) -> ByKey {
    ByKey::Column {
        column: id(name),
        name: id(name),
    }
}

/// A dim column a join reads, named `name` in the view.
pub fn attribute(name: &str, column: &str) -> Attribute {
    Attribute {
        name: id(name),
        column: id(column),
    }
}

/// A text value.
pub fn text(value: &str) -> Datum {
    Datum::Text(value.into())
}

/// A text literal.
pub fn lit(value: &str) -> Literal {
    Literal::Text(value.into())
}

/// `key IN (values)`.
pub fn condition(key: &str, values: Vec<Literal>) -> Condition {
    Condition {
        key: id(key),
        values,
    }
}

/// `public.name`.
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
    for literal in values {
        if equals(value, literal) {
            return true;
        }
    }
    false
}

fn equals(value: &Datum, literal: &Literal) -> bool {
    match (value, literal) {
        (Datum::Int(found), Literal::Int(wanted)) => found == wanted,
        (Datum::Text(found), Literal::Text(wanted)) => **found == **wanted,
        (Datum::Bool(found), Literal::Bool(wanted)) => found == wanted,
        _ => false,
    }
}

/// Of the buckets [`Spilled::seal`] may seal, the share it does seal.
const SEALED_PERCENT: usize = 70;

/// A splitmix64 sequence from its seed, the one field; the tests need no
/// crate for randomness. The unit tests in `src/` have their own copy,
/// which these tests can't reach.
pub struct Rng(pub u64);

impl Rng {
    /// The next number of the sequence.
    pub fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut mixed = self.0;
        mixed = (mixed ^ (mixed >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        mixed ^ (mixed >> 31)
    }

    /// A number from 0 up to, not including, `bound`.
    pub fn below(&mut self, bound: usize) -> usize {
        usize::try_from(self.next() % bound as u64).unwrap()
    }

    /// True `percent` percent of the time.
    pub fn chance(&mut self, percent: usize) -> bool {
        self.below(100) < percent
    }

    /// An integer in `low..high`.
    pub fn int(&mut self, low: i64, high: i64) -> i64 {
        let offset = self.next() % high.abs_diff(low);
        low.checked_add_unsigned(offset).unwrap()
    }

    /// An integer in `low..high`, or null `percent` percent of the time.
    pub fn maybe(&mut self, low: i64, high: i64, percent: usize) -> Datum {
        if self.chance(percent) {
            Datum::Null
        } else {
            Datum::Int(self.int(low, high))
        }
    }

    /// One item of `items`.
    // Generic, and borrowing the item: the tests pick from lists of
    // names, rows and kinds alike, and use the pick in place.
    pub fn pick<'a, Item>(&mut self, items: &'a [Item]) -> &'a Item {
        &items[self.below(items.len())]
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
        let mut thawed = Vec::new();
        for id in store.sealed_read(ask).unwrap() {
            thawed.push(store.thaw(self.buckets[&id].clone()).unwrap());
        }
        self.read += thawed.len();
        store.ask_with(ask, &thawed).unwrap()
    }
}
