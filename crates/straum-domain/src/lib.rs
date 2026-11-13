//! No I/O and no dependencies. Adapters (Straum SQL, PostgreSQL, HTTP) depend
//! on this crate, never the other way round.

#![forbid(unsafe_code)]

mod aggregate;
mod ask;
mod codes;
mod column;
mod hash;
mod ident;
mod index;
mod projection;
mod rollup;
mod sealed;
mod spec;
mod store;
mod value;
mod value_counts;

pub use aggregate::{Aggregate, Reads};
pub use ask::{Ask, AskError, AskKey, TimeKey};
pub use ident::Identifier;
pub use rollup::RowError;
pub use sealed::{Sealed, SealedColumn, SealedError, SealedId, Thawed};
pub use spec::{
    Attribute, ByKey, Condition, Dim, Fact, Key, KeyPart, Literal, Measure, Origin, Ref, RefKind,
    Spec, SpecError, Table, Time, Via, Width,
};
pub use store::{Answer, AnswerColumn, AnswerRow, Store};
pub use value::{ColumnType, Datum, Value, decimal};
