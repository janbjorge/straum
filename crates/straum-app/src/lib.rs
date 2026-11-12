//! Straum's use cases: load every table, apply the change stream, start
//! over when a change no longer fits or the server holds too much WAL for
//! Straum, and answer asks while that goes on.
//! PostgreSQL sits behind the [`Source`] port, and sealed buckets behind
//! the [`Spill`] port; the [`Writer`] is the loop that reads the source,
//! and [`App`] is what asks read.

#![forbid(unsafe_code)]

mod app;
mod lag;
mod source;
mod spill;

pub use app::{
    AnswerError, Answered, App, Error, Failure, Held, HeldDim, HeldKey, HeldView, Limits, Status,
    Step, Writer, mib,
};
pub use source::{Change, Commit, Lsn, Source, SourceError, Wal};
pub use spill::{Spill, SpillError, Spilling};
