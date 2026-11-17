//! `App` and `Writer` against an in-memory PostgreSQL. One test binary:
//! `fake` is the database, `common` the spec and the helpers every case
//! uses. The cases come by subject: `start` (loading), `changes`
//! (applying commits), `status`, then `random`, which checks random
//! writes, dropped connections and new writers after a failure against a
//! fresh load of the database at the same LSN.

mod common;
mod fake;

mod changes;
mod random;
mod start;
mod status;
