//! straum's use cases: load every table, apply the change stream, start
//! over when a change no longer fits, and answer asks while that goes on.
//! PostgreSQL sits behind the [`Source`] port; the [`Writer`] is the loop
//! that reads it, and [`App`] is what asks read.

#![forbid(unsafe_code)]

mod app;
mod source;

pub use app::{AnswerError, Answered, App, Error, Step, Writer};
pub use source::{Change, Commit, Lsn, Old, Source, SourceError};
