//! weir's use cases: load every table, apply the change stream, read a table
//! again when a change no longer fits it, and answer asks. PostgreSQL sits
//! behind the [`Source`] port.

#![forbid(unsafe_code)]

mod app;
mod source;

pub use app::{AnswerError, Answered, App, Error, Step};
pub use source::{Change, Commit, Lsn, Old, Source, SourceError};
