//! Straum's use cases: load every table, apply the change stream, stop
//! when a change no longer fits or the server holds too much WAL for
//! Straum, and answer asks while that goes on.
//! PostgreSQL sits behind the [`Source`] port; the [`Writer`] is the loop
//! that reads the source, and [`App`] is what asks read.

#![forbid(unsafe_code)]

mod app;
mod held;
mod lag;
mod loading;
mod source;
mod writer;

pub use app::{AnswerError, Answered, App, Limits, Status};
pub use held::{Held, HeldDim, HeldKey, HeldView, ViewKey};
pub use loading::Loading;
pub use source::{
    Change, Commit, Drift, Estimate, LoadCounts, Lsn, Opened, Source, SourceError, Wal,
};
pub use writer::{Error, Step, Unfitting, Writer, format_mebibytes};
