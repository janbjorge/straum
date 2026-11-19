//! Straum's use cases: load every table, apply the change stream, stop
//! when a change no longer fits or the server holds too much WAL for
//! Straum, and answer asks while that goes on.
//! PostgreSQL sits behind the [`Source`] port; the [`Writer`] is the loop
//! that reads the source, and [`App`] is what asks read.

#![forbid(unsafe_code)]

mod app;
mod lag;
mod source;

pub use app::{
    AnswerError, Answered, App, Error, Held, HeldDim, HeldKey, HeldView, Limits, Status, Step,
    Unfitting, ViewKey, Writer, format_mebibytes,
};
pub use source::{Change, Commit, Drift, Lsn, Source, SourceError, Wal};
