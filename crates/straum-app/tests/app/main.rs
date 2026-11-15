//! `App` and `Writer` against an in-memory PostgreSQL. One test binary:
//! `fake` is the database, `spill` an in-memory spill, `common` the spec
//! and the helpers every case uses. The cases come by subject: `start`
//! (loading), `changes` (applying commits), `status`, `spilling`, then
//! `random`, which checks random writes, dropped connections and restarts
//! against a fresh load of the database at the same LSN.

mod common;
mod fake;
mod spill;

mod changes;
mod random;
mod spilling;
mod start;
mod status;
