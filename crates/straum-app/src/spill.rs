//! The port to where sealed buckets go while they are out of memory
//! (ADR-0033). Asks get them from many threads at once, so every method
//! takes `&self`.

use std::error;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use straum_domain::{Sealed, SealedId};

/// Whatever the spill can't do: a full disk, a file gone or unreadable.
/// An ask that meets one fails; the writer starts over after one.
pub type SpillError = Box<dyn error::Error + Send + Sync>;

pub trait Spill: fmt::Debug + Send + Sync {
    /// Keeps a bucket, replacing any kept under its id.
    fn put(&self, sealed: &Sealed) -> Result<(), SpillError>;

    fn get(&self, id: &SealedId) -> Result<Sealed, SpillError>;

    /// Forgets a bucket. One never kept is no error.
    fn remove(&self, id: &SealedId) -> Result<(), SpillError>;

    /// Forgets every bucket, as each start does: spilled buckets hold codes
    /// only the store that sealed them knows.
    fn clear(&self) -> Result<(), SpillError>;
}

/// Where sealed buckets go, and when.
#[derive(Debug, Clone)]
pub struct Spilling {
    pub spill: Arc<dyn Spill>,
    /// A bucket is sealed once it ended this long ago, by straum's clock.
    pub seal_after: Duration,
}
