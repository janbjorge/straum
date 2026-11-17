//! Straum's PostgreSQL adapter: the [`straum_app::Source`] over logical
//! replication or the physical WAL. Start at `source.rs`: it checks the
//! setup (`catalog.rs`), loads each table with one `COPY` (`load.rs`), then
//! hands a thread the stream (`stream.rs`), which turns pgoutput
//! (`pgoutput.rs`, `decode.rs`) or WAL records (`physical.rs`, `wal/`) into
//! commits, while `watchdog.rs` keeps the slot from holding too much WAL.
//! `wire.rs` is the connection under all of it.

// The test build counts allocations with the one `unsafe` it allows,
// in `wal/test_support.rs` (ADR-0013); `forbid` would refuse even that.
#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]

mod catalog;
mod decode;
mod dsn;
mod error;
mod load;
mod pgoutput;
mod physical;
mod plan;
mod problem;
mod source;
mod stream;
#[cfg(test)]
mod tests;
mod tls;
mod unexpected;
mod value;
mod wal;
mod watchdog;
// An ignored benchmark, never run by the tests that measure coverage;
// cargo-llvm-cov sets `coverage`.
#[cfg(all(test, not(coverage)))]
mod wal_bench;
mod wire;

pub use dsn::{Dsn, DsnError};
pub use plan::plan;
pub use source::{Postgres, Replication};
