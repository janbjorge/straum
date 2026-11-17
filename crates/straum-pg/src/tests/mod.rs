//! straum-pg against PostgreSQL in Docker, one Compose project per test:
//! 18, or the major `STRAUM_TEST_POSTGRES` names. Every answer is checked
//! against the view body of the spec, run in PostgreSQL. `compose`,
//! `support` and `references` hold what the subject files share.

pub(crate) mod compose;
mod decoding;
mod followed;
mod heap_fixtures;
mod joins;
mod load;
mod logical;
mod physical;
mod physical_setup;
mod protocol;
mod records;
mod references;
mod setup;
pub(crate) mod support;
mod taxi_month;
mod tls;
mod transactions;
mod types;
mod watchdog;
