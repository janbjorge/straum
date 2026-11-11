//! Sealed buckets as plain data (ADR-0033): what a spill keeps, and what
//! the store checks before it reads one back.

use std::fmt;

use crate::ident::Identifier;
use crate::rollup::Cells;
use crate::value::Datum;

/// Names one bucket of one width of a fact.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SealedId {
    pub fact: Identifier,
    /// Bucket width in seconds.
    pub width: i64,
    /// Unix seconds.
    pub bucket_start: i64,
}

/// A bucket's cells as plain data, for a spill to keep: cell `i` has key
/// `keys[i * len..][..len]` with `len = keys.len() / counts.len()`,
/// `counts[i]` rows and entry `i` of each column. Keys are codes, which mean
/// something only to the store that sealed them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
    pub id: SealedId,
    pub keys: Vec<u32>,
    pub counts: Vec<u64>,
    /// One per input column of the fact's measures, in the store's order.
    pub columns: Vec<SealedColumn>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SealedColumn {
    /// Sum and count of the non-null values.
    Sum { sums: Vec<i128>, counts: Vec<u64> },
    /// Count per value, ascending by value.
    Extent { values: Vec<Vec<(i64, u64)>> },
    /// Count per value, ascending by value.
    Distinct { values: Vec<Vec<(Datum, u64)>> },
}

/// A sealed bucket checked against the store, ready for an ask to read.
#[derive(Debug)]
pub struct Thawed {
    pub(crate) id: SealedId,
    pub(crate) cells: Cells,
}

/// A sealed bucket that doesn't fit the store it is handed to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedError {
    pub id: SealedId,
    pub reason: &'static str,
}

impl fmt::Display for SealedId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} bucket of {}s at {}",
            self.fact, self.width, self.bucket_start
        )
    }
}

impl fmt::Display for SealedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sealed {}: {}", self.id, self.reason)
    }
}

impl std::error::Error for SealedError {}
