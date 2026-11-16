//! What a view is and how its cells add up: the spec, the store that
//! keeps every view's cells as rows come and go, and the asks that read
//! them. No I/O and no dependencies. Adapters (Straum SQL, PostgreSQL,
//! HTTP) depend on this crate, never the other way round.
//!
//! Read `spec.rs` first (the views, as data), then `ask.rs` (what an ask
//! may say), then `store/` (where rows land and asks read), which keeps a
//! `rollup/` per view (its cells, by bucket) and a `projection.rs` per
//! dim. `aggregate.rs` merges cells into answers; the other files are the
//! compact columns and maps the cells are kept in.

#![forbid(unsafe_code)]

mod aggregate;
mod ask;
mod codes;
mod column;
mod hash;
mod identifier;
mod index;
mod projection;
mod rollup;
mod sealed;
mod spec;
mod spec_error;
mod store;
#[cfg(test)]
mod test_rng;
mod value;
mod value_counts;

pub use aggregate::{Aggregate, Fraction, Reads};
pub use ask::{Ask, AskError, AskKey, Comparison, MeasureCondition};
pub use hash::{FastMap, FastSet, Seeded};
pub use identifier::Identifier;
pub use rollup::RowError;
pub use sealed::{DatumCount, Sealed, SealedColumn, SealedError, SealedId, Thawed, ValueCount};
pub use spec::{
    Attribute, ByKey, Condition, Dim, Fact, Key, KeyPart, Literal, Measure, Origin, Ref, RefKind,
    Spec, Table, Time, Via, Width,
};
pub use spec_error::{JoinError, SpecError};
pub use store::{Answer, AnswerColumn, AnswerRow, Store};
pub use value::{ColumnType, Datum, Value, decimal, push_decimal};
