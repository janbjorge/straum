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
mod closest;
mod codes;
mod column;
mod date;
mod gathered;
mod hash;
mod identifier;
mod index;
mod projection;
mod rollup;
mod spec;
mod spec_error;
mod spread;
mod store;
mod tally;
#[cfg(test)]
mod test_rng;
mod uuid;
mod value;
mod value_counts;
mod wide;
mod width;

pub use aggregate::{Aggregate, Fraction, Reads};
pub use ask::{Ask, AskError, AskKey, Comparison, Instead, MeasureCondition};
pub use closest::closest;
pub use date::{Era, days_from_civil, format_date, parse_calendar_day, parse_date};
pub use hash::{FastMap, FastSet, Seeded};
pub use identifier::Identifier;
pub use rollup::RowError;
pub use spec::{
    Attribute, ByKey, Condition, Definition, Dim, Fact, Key, KeyPart, Literal, Measure, Ref,
    RefKind, Spec, Table, Time, Via,
};
pub use spec_error::{JoinError, SpecError, ViaColumn};
pub use store::{Answer, AnswerColumn, AnswerRow, Cut, Start, Store, Window, WindowAnswer};
pub use uuid::{format_uuid, parse_uuid};
pub use value::{ColumnType, Datum, MAX_SCALE, Value, decimal, push_decimal, push_wide_decimal};
pub use wide::U256;
pub use width::{UNITS, Unit, Width};
