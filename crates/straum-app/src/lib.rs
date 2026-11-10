//! straum's use cases: load every table, apply the change stream, start
//! over when a change no longer fits or the server holds too much WAL for
//! straum, and answer asks while that goes on.
//! PostgreSQL sits behind the [`Source`] port; the [`Writer`] is the loop
//! that reads it, and [`App`] is what asks read.

#![forbid(unsafe_code)]

mod app;
mod lag;
mod source;

pub use app::{AnswerError, Answered, App, Error, Limits, Status, Step, Writer};
pub use source::{Change, Commit, Lsn, Source, SourceError, Wal};
