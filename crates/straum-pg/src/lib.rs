//! straum's PostgreSQL adapter: the [`straum_app::Source`] over logical
//! replication.
//!
//! One replication connection checks the setup, creates a temporary slot,
//! loads every table in the slot's snapshot (one `GROUP BY` per fact), then
//! streams pgoutput from the slot's consistent point. A thread owns the
//! stream: it decodes commits and answers PostgreSQL while straum applies
//! them. A table read again gets a second connection with its own
//! temporary slot. Plain TCP only for now; TLS comes with the deploy.

#![forbid(unsafe_code)]

mod catalog;
mod dsn;
mod error;
mod load;
mod pgoutput;
mod plan;
mod source;
mod stream;
#[cfg(test)]
mod tests;
mod value;
mod wire;

pub use dsn::{Dsn, DsnError};
pub use error::Error;
pub use pgoutput::DecodeError;
pub use plan::plan;
pub use source::Pg;
pub use value::ValueError;
