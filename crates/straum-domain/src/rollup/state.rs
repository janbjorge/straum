//! A cell state: what a cell keeps of one input column so its measures
//! can answer, such as a count of nulls, a sum, or a count per value for
//! a min or max. Each state is a column with one entry per cell of a
//! bucket. A row's value goes into one entry; a whole cell's entries move
//! between keyed and bound cells.

use crate::aggregate::Reads;
use crate::column::{Counts, Squares, Sums, put};
use crate::value::Datum;
use crate::value_counts::ValueCounts;
use crate::wide::U256;

use super::distinct::DistinctCells;
use super::row_error::RowError;

/// A form of [`State`], which [`crate::Aggregate`]'s `state_kind` picks
/// per measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateKind {
    Count,
    Sum,
    /// A sum with a sum of squares beside it, for a variance.
    Spread,
    Extent,
    Distinct,
    Bools,
}

impl StateKind {
    /// What a state of this kind reads of its column, which decides the
    /// types the column may have.
    pub(crate) fn reads(self) -> Reads {
        match self {
            StateKind::Count => Reads::Nullness,
            StateKind::Sum | StateKind::Spread | StateKind::Extent => Reads::Numbers,
            StateKind::Distinct => Reads::Values,
            StateKind::Bools => Reads::Booleans,
        }
    }
}

/// The states of one input column, one entry per cell of a bucket.
#[derive(Debug)]
pub(crate) enum State {
    /// The nulls among each cell's rows: its count is the rows that are
    /// not. Most columns hold none, and then this holds nothing.
    Count { nulls: Counts },
    /// Each cell's sum, and the nulls among its rows as for a count. With
    /// a variance on the column, each cell's sum of squares too.
    Sum {
        sums: Sums,
        nulls: Counts,
        squares: Option<Squares>,
    },
    /// Count per value, so removing the min or max reveals the next one.
    /// `mins` and `maxs` mirror each cell's ends, so an ask reads them
    /// without walking a tree. An empty cell's are `i64::MAX` and
    /// `i64::MIN`, which any min or max passes over: a merge needs no test
    /// for empty.
    Extent {
        mins: Vec<i64>,
        maxs: Vec<i64>,
        counts: Vec<ValueCounts>,
    },
    /// Count per value: per number as for `Extent` while the column
    /// holds only numbers, else in a map.
    Distinct(DistinctCells),
    /// Counts of true and of false, so removing the last false turns a
    /// `bool_and` true.
    Bools { trues: Counts, falses: Counts },
}

/// A cell named by its place in its bucket, and the rows it holds.
#[derive(Debug, Clone, Copy)]
pub(super) struct CellRows {
    pub(super) position: usize,
    pub(super) rows: u64,
}

impl State {
    /// An empty column of `kind`.
    pub(super) fn new(kind: StateKind) -> State {
        match kind {
            StateKind::Count => State::Count {
                nulls: Counts::Zero(0),
            },
            StateKind::Sum => State::Sum {
                sums: Sums::new(),
                nulls: Counts::Zero(0),
                squares: None,
            },
            StateKind::Spread => State::Sum {
                sums: Sums::new(),
                nulls: Counts::Zero(0),
                squares: Some(Squares::new()),
            },
            StateKind::Extent => State::Extent {
                mins: Vec::new(),
                maxs: Vec::new(),
                counts: Vec::new(),
            },
            StateKind::Distinct => State::Distinct(DistinctCells::Numbers(Vec::new())),
            StateKind::Bools => State::Bools {
                trues: Counts::Zero(0),
                falses: Counts::Zero(0),
            },
        }
    }

    /// Adds an empty entry for a new cell.
    pub(super) fn push(&mut self) {
        match self {
            State::Count { nulls } => nulls.push(),
            State::Sum {
                sums,
                nulls,
                squares,
            } => {
                sums.push();
                nulls.push();
                if let Some(squares) = squares {
                    squares.push();
                }
            }
            State::Extent { mins, maxs, counts } => {
                put(mins, i64::MAX);
                put(maxs, i64::MIN);
                put(counts, ValueCounts::EMPTY);
            }
            State::Distinct(cells) => cells.push(),
            State::Bools { trues, falses } => {
                trues.push();
                falses.push();
            }
        }
    }

    /// Drops the entry of `cell`, the last entry taking its place.
    pub(super) fn swap_remove(&mut self, cell: usize) {
        match self {
            State::Count { nulls } => nulls.swap_remove(cell),
            State::Sum {
                sums,
                nulls,
                squares,
            } => {
                sums.swap_remove(cell);
                nulls.swap_remove(cell);
                if let Some(squares) = squares {
                    squares.swap_remove(cell);
                }
            }
            State::Extent { mins, maxs, counts } => {
                mins.swap_remove(cell);
                maxs.swap_remove(cell);
                counts.swap_remove(cell);
            }
            State::Distinct(cells) => cells.swap_remove(cell),
            State::Bools { trues, falses } => {
                trues.swap_remove(cell);
                falses.swap_remove(cell);
            }
        }
    }

    /// A null is counted where a count or a sum reads the column, and
    /// passed over elsewhere, as in SQL.
    pub(super) fn add(&mut self, cell: usize, value: &Datum) {
        match (self, value) {
            (State::Count { nulls } | State::Sum { nulls, .. }, Datum::Null) => nulls.add(cell, 1),
            (_, Datum::Null) | (State::Count { .. }, _) => {}
            (State::Sum { sums, squares, .. }, &Datum::Int(number)) => {
                sums.add(cell, number.into());
                if let Some(squares) = squares {
                    squares.add(cell, square(number));
                }
            }
            (State::Extent { mins, maxs, counts }, &Datum::Int(number)) => {
                mins[cell] = mins[cell].min(number);
                maxs[cell] = maxs[cell].max(number);
                counts[cell].add(number, 1);
            }
            (State::Distinct(cells), _) => cells.add(cell, value),
            (State::Bools { trues, .. }, Datum::Bool(true)) => trues.add(cell, 1),
            (State::Bools { falses, .. }, Datum::Bool(false)) => falses.add(cell, 1),
            _ => unreachable!("Rollup::check lets through only what a state reads"),
        }
    }

    /// `rows` are the cell's rows, the one that leaves among them.
    pub(super) fn remove(&mut self, cell: usize, rows: u64, value: &Datum) -> Result<(), RowError> {
        match (self, value) {
            (State::Count { nulls } | State::Sum { nulls, .. }, Datum::Null) => {
                held(nulls.get(cell) > 0)?;
                nulls.sub(cell, 1);
            }
            (_, Datum::Null) => {}
            // A value that is not null leaves only a cell that holds one.
            (State::Count { nulls }, _) => held(nulls.get(cell) < rows)?,
            (
                State::Sum {
                    sums,
                    nulls,
                    squares,
                },
                &Datum::Int(number),
            ) => {
                held(nulls.get(cell) < rows)?;
                sums.add(cell, -i128::from(number));
                if let Some(squares) = squares {
                    squares.sub(cell, square(number));
                }
            }
            (State::Extent { mins, maxs, counts }, &Datum::Int(number)) => {
                held(counts[cell].sub(number, 1))?;
                if number == mins[cell] || number == maxs[cell] {
                    refresh_ends(cell, mins, maxs, counts);
                }
            }
            (State::Distinct(cells), _) => held(cells.remove(cell, value))?,
            (State::Bools { trues: counts, .. }, Datum::Bool(true))
            | (State::Bools { falses: counts, .. }, Datum::Bool(false)) => {
                held(counts.get(cell) > 0)?;
                counts.sub(cell, 1);
            }
            _ => unreachable!("Rollup::check lets through only what a state reads"),
        }
        Ok(())
    }

    /// Whether both columns are of one kind, so a cell of one can move
    /// into the other.
    pub(super) fn same_kind(&self, other: &State) -> bool {
        matches!(
            (self, other),
            (State::Count { .. }, State::Count { .. })
                | (
                    State::Sum { squares: None, .. },
                    State::Sum { squares: None, .. }
                )
                | (
                    State::Sum {
                        squares: Some(_),
                        ..
                    },
                    State::Sum {
                        squares: Some(_),
                        ..
                    }
                )
                | (State::Extent { .. }, State::Extent { .. })
                | (State::Distinct(_), State::Distinct(_))
                | (State::Bools { .. }, State::Bools { .. })
        )
    }

    /// Whether the `held` cell holds the `given` cell of `source`, so
    /// [`State::sub_from`] can take it out.
    pub(super) fn covers(&self, held: CellRows, source: &State, given: CellRows) -> bool {
        let (cell, rows) = (held.position, held.rows);
        let (source_cell, source_rows) = (given.position, given.rows);
        match (self, source) {
            (
                State::Count { nulls },
                State::Count {
                    nulls: source_nulls,
                },
            )
            | (
                State::Sum { nulls, .. },
                State::Sum {
                    nulls: source_nulls,
                    ..
                },
            ) => {
                // As many nulls, and as many values that are not.
                let nulls = nulls.get(cell);
                let source_nulls = source_nulls.get(source_cell);
                nulls >= source_nulls && rows - nulls >= source_rows - source_nulls
            }
            (
                State::Extent { counts, .. },
                State::Extent {
                    counts: source_counts,
                    ..
                },
            ) => counts[cell].covers(&source_counts[source_cell]),
            (State::Distinct(cells), State::Distinct(source_cells)) => {
                cells.covers(cell, source_cells, source_cell)
            }
            (
                State::Bools { trues, falses },
                State::Bools {
                    trues: source_trues,
                    falses: source_falses,
                },
            ) => {
                trues.get(cell) >= source_trues.get(source_cell)
                    && falses.get(cell) >= source_falses.get(source_cell)
            }
            _ => false,
        }
    }

    /// Adds cell `source_cell` of `source` into `cell`.
    pub(super) fn add_from(&mut self, cell: usize, source: &State, source_cell: usize) {
        match (self, source) {
            (
                State::Count { nulls },
                State::Count {
                    nulls: source_nulls,
                },
            ) => {
                nulls.add(cell, source_nulls.get(source_cell));
            }
            (
                State::Sum {
                    sums,
                    nulls,
                    squares,
                },
                State::Sum {
                    sums: source_sums,
                    nulls: source_nulls,
                    squares: source_squares,
                },
            ) => {
                sums.add(cell, source_sums.get(source_cell));
                nulls.add(cell, source_nulls.get(source_cell));
                if let (Some(squares), Some(source_squares)) = (squares, source_squares) {
                    squares.add(cell, source_squares.get(source_cell));
                }
            }
            (
                State::Extent { mins, maxs, counts },
                State::Extent {
                    counts: source_counts,
                    ..
                },
            ) => {
                counts[cell].add_counts(&source_counts[source_cell]);
                refresh_ends(cell, mins, maxs, counts);
            }
            (State::Distinct(cells), State::Distinct(source_cells)) => {
                cells.add_from(cell, source_cells, source_cell);
            }
            (
                State::Bools { trues, falses },
                State::Bools {
                    trues: source_trues,
                    falses: source_falses,
                },
            ) => {
                trues.add(cell, source_trues.get(source_cell));
                falses.add(cell, source_falses.get(source_cell));
            }
            _ => unreachable!("Cells::absorb checks the column kinds"),
        }
    }

    /// Takes cell `source_cell` of `source` back out of `cell`.
    pub(super) fn sub_from(&mut self, cell: usize, source: &State, source_cell: usize) {
        match (self, source) {
            (
                State::Count { nulls },
                State::Count {
                    nulls: source_nulls,
                },
            ) => {
                nulls.sub(cell, source_nulls.get(source_cell));
            }
            (
                State::Sum {
                    sums,
                    nulls,
                    squares,
                },
                State::Sum {
                    sums: source_sums,
                    nulls: source_nulls,
                    squares: source_squares,
                },
            ) => {
                sums.add(cell, -source_sums.get(source_cell));
                nulls.sub(cell, source_nulls.get(source_cell));
                if let (Some(squares), Some(source_squares)) = (squares, source_squares) {
                    squares.sub(cell, source_squares.get(source_cell));
                }
            }
            (
                State::Extent { mins, maxs, counts },
                State::Extent {
                    counts: source_counts,
                    ..
                },
            ) => {
                counts[cell].sub_counts(&source_counts[source_cell]);
                refresh_ends(cell, mins, maxs, counts);
            }
            (State::Distinct(cells), State::Distinct(source_cells)) => {
                cells.sub_from(cell, source_cells, source_cell);
            }
            (
                State::Bools { trues, falses },
                State::Bools {
                    trues: source_trues,
                    falses: source_falses,
                },
            ) => {
                trues.sub(cell, source_trues.get(source_cell));
                falses.sub(cell, source_falses.get(source_cell));
            }
            _ => unreachable!("Cells::release checks the column kinds"),
        }
    }
}

/// Sets an `Extent` cell's mirrored min and max to the ends its counts
/// hold now.
fn refresh_ends(cell: usize, mins: &mut [i64], maxs: &mut [i64], counts: &[ValueCounts]) {
    let ends = counts[cell].ends();
    mins[cell] = ends.least;
    maxs[cell] = ends.greatest;
}

/// `number` squared, below 2^126 for any `i64`.
fn square(number: i64) -> U256 {
    let magnitude = u128::from(number.unsigned_abs());
    U256::from(magnitude * magnitude)
}

fn held(present: bool) -> Result<(), RowError> {
    if present {
        Ok(())
    } else {
        Err(RowError::Missing)
    }
}
