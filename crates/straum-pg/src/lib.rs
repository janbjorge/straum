//! straum's PostgreSQL adapter: the [`straum_app::Source`] over logical
//! replication.
//!
//! One replication connection checks the setup, creates a temporary slot,
//! loads every fact in the slot's snapshot (one `COPY` per fact, joined to
//! its dims), then streams pgoutput from the slot's consistent point. A
//! thread owns the stream: it decodes commits and answers PostgreSQL while
//! straum applies them. A plain connection on the side looks up the dim
//! rows of each commit's fact rows. Connections are TLS unless the DSN
//! says `sslmode=disable`.

#![forbid(unsafe_code)]

mod catalog;
mod decode;
mod dsn;
mod error;
mod load;
mod pgoutput;
mod plan;
mod resolve;
mod source;
mod stream;
#[cfg(test)]
mod tests;
mod tls;
mod value;
mod wire;

pub use dsn::{Dsn, DsnError};
pub use error::Error;
pub use pgoutput::DecodeError;
pub use plan::plan;
pub use source::Pg;
pub use value::ValueError;
