//! weir's use cases: load every table, apply the change stream, read a table
//! again when a change no longer fits it, and answer asks while that goes
//! on. PostgreSQL sits behind the [`Source`] port; the [`Writer`] is the
//! loop that reads it, and [`App`] is what asks read.

#![forbid(unsafe_code)]

mod app;
mod source;

pub use app::{AnswerError, Answered, App, Error, Status, Step, TableStatus, Writer};
pub use source::{Change, Commit, Lsn, Old, Source, SourceError};
