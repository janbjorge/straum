//! weir's domain: the aggregates it maintains and the asks it answers.
//!
//! No I/O and no dependencies. Adapters (notation, PostgreSQL, HTTP) depend
//! on this crate, never the other way round.

#![forbid(unsafe_code)]

mod ask;
mod ident;
mod rollup;
mod spec;
mod store;
mod value;

pub use ask::{Ask, AskError, AskKey, TimeKey};
pub use ident::Ident;
pub use rollup::{DimTable, Rollup, RowError};
pub use spec::{
    Agg, Cond, Dim, Fact, Grain, Key, KeyPath, Literal, Measure, Ref, Slot, Spec, SpecError, Table,
    Time,
};
pub use store::{Answer, Store};
pub use value::{Datum, Value};
