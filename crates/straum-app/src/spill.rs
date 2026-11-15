//! The port to the spill: where sealed buckets go while they are out of
//! memory (ADR-0033). The writer puts a bucket in when it seals it and
//! takes it out when a change reaches it; asks read it back to answer.
//! Asks get buckets from many threads at once, so every method takes
//! `&self`.

use std::error;
use std::sync::Arc;
use std::time::Duration;

use straum_domain::{Sealed, SealedId};

/// Whatever the spill can't do: a full disk, a file gone or unreadable.
/// An ask that meets one fails; the writer starts over after one.
pub type SpillError = Box<dyn error::Error + Send + Sync>;

/// Somewhere to keep sealed buckets: files in the binary, memory in tests.
pub trait Spill: Send + Sync {
    /// Keeps a bucket, replacing any kept under its id.
    fn put(&self, sealed: &Sealed) -> Result<(), SpillError>;

    /// Reads a bucket back; one never kept is an error.
    fn get(&self, id: &SealedId) -> Result<Sealed, SpillError>;

    /// Forgets a bucket. One never kept is no error.
    fn remove(&self, id: &SealedId) -> Result<(), SpillError>;

    /// Forgets every bucket, as each start does: spilled buckets hold codes
    /// only the store that sealed them knows.
    fn clear(&self) -> Result<(), SpillError>;
}

/// Where sealed buckets go, and when.
#[derive(Clone)]
pub struct Spilling {
    pub spill: Arc<dyn Spill>,
    /// A bucket is sealed once it ended this long ago, by Straum's clock.
    pub seal_after: Duration,
}
