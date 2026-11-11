//! No I/O and no dependencies. Adapters (straum SQL, PostgreSQL, HTTP) depend
//! on this crate, never the other way round.

#![forbid(unsafe_code)]

mod agg;
mod ask;
mod codes;
mod hash;
mod ident;
mod projection;
mod rollup;
mod sealed;
mod spec;
mod store;
mod value;

pub use agg::Agg;
pub use ask::{Ask, AskError, AskKey, TimeKey};
pub use ident::Ident;
pub use rollup::RowError;
pub use sealed::{Sealed, SealedColumn, SealedError, SealedId, Thawed};
pub use spec::{
    Attr, By, Cond, Dim, Fact, Grain, Key, Literal, Measure, Origin, Ref, RefKind, Slot, Spec,
    SpecError, Table, Time, Via,
};
pub use store::{Answer, AnswerColumn, AnswerRow, Store};
pub use value::{ColumnType, Datum, Value, decimal};
