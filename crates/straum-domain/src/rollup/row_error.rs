//! What can go wrong with a row on its way into a rollup or a
//! projection, and the two checks both make before a row goes in: that it
//! has as many columns as the table, and that rows laid end to end split
//! into whole rows.

use std::fmt;
use std::slice::ChunksExact;

use crate::identifier::Identifier;
use crate::value::Datum;

/// Why a row was turned away. The store is left as it was, except after
/// `Missing`: the rows held then no longer match the table's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowError {
    Arity {
        expected: usize,
        found: usize,
    },
    /// The time column or a sum, avg, min, max or percentile column holds
    /// a non-integer.
    NotInt {
        column: Identifier,
    },
    /// A `bool_and` or `bool_or` column holds a non-boolean.
    NotBool {
        column: Identifier,
    },
    /// The time is so early that no bucket of a kept width can start at
    /// or before it within 64 bits.
    TimeOutOfRange {
        column: Identifier,
    },
    /// A removed row was never added.
    Missing,
}

impl fmt::Display for RowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RowError::Arity { expected, found } => {
                write!(formatter, "row has {found} columns, expected {expected}")
            }
            RowError::NotInt { column } => {
                write!(formatter, "column {column} is not an integer")
            }
            RowError::NotBool { column } => {
                write!(formatter, "column {column} is not a boolean")
            }
            RowError::TimeOutOfRange { column } => {
                write!(
                    formatter,
                    "column {column} is earlier than any bucket starts"
                )
            }
            RowError::Missing => formatter.write_str("removed row was never added"),
        }
    }
}

impl std::error::Error for RowError {}

/// Turns away a row of `found` columns where the table has `expected`.
pub(crate) fn arity(expected: usize, found: usize) -> Result<(), RowError> {
    if found == expected {
        Ok(())
    } else {
        Err(RowError::Arity { expected, found })
    }
}

/// Rows laid end to end, as a load reads them, cut `columns` wide. A
/// last row cut short is an error naming the columns it has.
// `ChunksExact` borrows the rows: a load's rows are cut, never copied.
pub(crate) fn split_rows(
    rows: &[Datum],
    columns: usize,
) -> Result<ChunksExact<'_, Datum>, RowError> {
    if !rows.len().is_multiple_of(columns) {
        return Err(RowError::Arity {
            expected: columns,
            found: rows.len() % columns,
        });
    }
    Ok(rows.chunks_exact(columns))
}
