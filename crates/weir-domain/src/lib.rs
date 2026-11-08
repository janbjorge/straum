//! weir's domain: the aggregates it maintains and the asks it answers.
//!
//! No I/O and no dependencies. Adapters (notation, PostgreSQL, HTTP) depend
//! on this crate, never the other way round.

mod ask;
mod ident;
mod spec;

pub use ask::{Ask, AskError, AskKey, TimeKey};
pub use ident::Ident;
pub use spec::{
    Agg, Cond, Dim, Fact, Grain, Key, KeyPath, Literal, Measure, Ref, Slot, Spec, SpecError, Table,
    Time,
};
