//! A level: the cells of one bucket width, bucket by bucket in time
//! order. A row goes into the cell of its key in the bucket its time
//! falls in, at every level. An ask reads a range as a cover: the widest
//! buckets that fit inside it, narrower ones at its ends.

use std::collections::BTreeMap;

use crate::value::Datum;

use super::cells::Cells;
use super::row_error::RowError;
use super::state::State;
use super::{BucketId, Cover, CoveredBucket, DATE_BIN, Direction, Input, LevelIndex, Rollup, TIME};

/// The cells of one bucket width.
#[derive(Debug)]
pub(super) struct Level {
    /// Bucket width in seconds.
    pub(super) width: i64,
    /// Oldest first. `None` holds the rows without a time, which no ask reads.
    pub(super) buckets: BTreeMap<Option<i64>, Cells>,
}

/// Rows a load adds at once, each with its key codes beside it.
#[derive(Clone, Copy)]
pub(super) struct CodedRows<'a> {
    // Borrowed from the load, so each level's thread reads the one copy.
    pub(super) rows: &'a [&'a [Datum]],
    /// `key_parts` codes a row, end to end.
    pub(super) keys: &'a [u32],
    pub(super) key_parts: usize,
}

/// What moving a cell did to the cells of its bucket.
pub(super) enum Shift {
    Same,
    New,
    Removed,
}

/// One keyed cell to move into or out of a bound cell.
#[derive(Clone, Copy)]
pub(super) struct MoveCell<'a> {
    // Borrowed from the keyed cells, which copying would cost per cell.
    pub(super) key: &'a [u32],
    pub(super) count: u64,
    pub(super) source: &'a [State],
    pub(super) source_cell: usize,
    pub(super) direction: Direction,
}

/// Whole buckets of one level from `from` to `to`.
#[derive(Debug, Clone, Copy)]
struct Run {
    level: LevelIndex,
    from: i64,
    to: i64,
}

impl Level {
    /// The start of the bucket `time` falls in; none for a null time.
    pub(super) fn bucket(&self, time: &Datum) -> Option<i64> {
        match *time {
            Datum::Int(seconds) => Some(seconds.div_euclid(self.width) * self.width),
            _ => None,
        }
    }

    /// Adds a checked row under its key codes; whether its cell is new.
    pub(super) fn add(&mut self, row: &[Datum], key: &[u32], inputs: &[Input]) -> bool {
        let cells = self.cells_of(&row[TIME], key.len(), inputs);
        let entry = cells.entry(key);
        let cell = entry.position;
        cells.rows.add(cell, 1);
        for (state, input) in cells.states.iter_mut().zip(inputs) {
            state.add(cell, &row[input.column.0]);
        }
        entry.new
    }

    /// The cells of the bucket `time` falls in, made if it has none.
    fn cells_of(&mut self, time: &Datum, key_parts: usize, inputs: &[Input]) -> &mut Cells {
        let start = self.bucket(time);
        (self.buckets.entry(start)).or_insert_with(|| Cells::new(key_parts, new_states(inputs)))
    }

    /// Adds every row of a load; how many cells are new.
    pub(super) fn add_coded(&mut self, coded: CodedRows<'_>, inputs: &[Input]) -> usize {
        let parts = coded.key_parts;
        let mut new_cells = 0;
        for (row_index, row) in coded.rows.iter().enumerate() {
            let key = &coded.keys[row_index * parts..][..parts];
            new_cells += usize::from(self.add(row, key, inputs));
        }
        new_cells
    }

    /// Takes a row out of the cell of `key`; whether the cell went with
    /// it.
    pub(super) fn remove(
        &mut self,
        row: &[Datum],
        key: &[u32],
        inputs: &[Input],
    ) -> Result<bool, RowError> {
        let bucket = self.bucket(&row[TIME]);
        let Some(cells) = self.buckets.get_mut(&bucket) else {
            return Err(RowError::Missing);
        };
        let cell = cells.find(key).ok_or(RowError::Missing)?;
        let rows = cells.rows.get(cell);
        for (state, input) in cells.states.iter_mut().zip(inputs) {
            state.remove(cell, rows, &row[input.column.0])?;
        }
        cells.rows.sub(cell, 1);
        if rows > 1 {
            return Ok(false);
        }
        cells.swap_remove(cell);
        if cells.len() == 0 {
            self.buckets.remove(&bucket);
        }
        Ok(true)
    }

    /// Moves one keyed cell into or out of the bound cell `key` names, in
    /// the bucket of `time`.
    pub(super) fn adjust(
        &mut self,
        inputs: &[Input],
        time: &Datum,
        move_cell: MoveCell<'_>,
    ) -> Result<Shift, RowError> {
        let MoveCell {
            key,
            count,
            source,
            source_cell,
            direction,
        } = move_cell;
        if direction == Direction::Add {
            let cells = self.cells_of(time, key.len(), inputs);
            let entry = cells.entry(key);
            if !cells.absorb(entry.position, count, source, source_cell) {
                return Err(RowError::Missing);
            }
            return Ok(if entry.new { Shift::New } else { Shift::Same });
        }
        let start = self.bucket(time);
        let Some(cells) = self.buckets.get_mut(&start) else {
            return Err(RowError::Missing);
        };
        let Some(cell) = cells.find(key) else {
            return Err(RowError::Missing);
        };
        if !cells.release(cell, count, source, source_cell) {
            return Err(RowError::Missing);
        }
        if cells.rows.get(cell) != 0 {
            return Ok(Shift::Same);
        }
        cells.swap_remove(cell);
        if cells.len() == 0 {
            self.buckets.remove(&start);
        }
        Ok(Shift::Removed)
    }

    // An iterator, not a `Vec`: an ask allocates per group, never per
    // bucket (`an_ask_allocates_per_group_not_per_cell` in tests/cost.rs).
    fn buckets_in(&self, from: i64, to: i64) -> impl Iterator<Item = CoveredBucket<'_>> {
        let width = self.width;
        let buckets = self.buckets.range(Some(from)..Some(to));
        buckets.map(move |(start, cells)| covered(width, *start, cells))
    }
}

fn new_states(inputs: &[Input]) -> Vec<State> {
    let mut states = Vec::with_capacity(inputs.len());
    for input in inputs {
        states.push(State::new(input.kind));
    }
    states
}

fn covered(width: i64, start: Option<i64>, cells: &Cells) -> CoveredBucket<'_> {
    CoveredBucket {
        id: BucketId {
            width,
            start: start.expect("a range of starts holds no null-time bucket"),
        },
        cells,
    }
}

impl Rollup {
    /// Buckets that cover `from..to` once, in time order, picked as
    /// `cover` says. `from` and `to` are on `date_bin` edges.
    // An iterator, not a `Vec`: an ask allocates per group, never per
    // bucket (`an_ask_allocates_per_group_not_per_cell` in tests/cost.rs).
    // So the runs' buckets are chained with `flat_map` rather than
    // gathered in a loop.
    pub(crate) fn cover(
        &self,
        from: i64,
        to: i64,
        cover: Cover,
    ) -> impl Iterator<Item = CoveredBucket<'_>> {
        let widest = match cover {
            Cover::EachDateBin => DATE_BIN,
            Cover::WidestFit => LevelIndex(self.levels.len() - 1),
        };
        let mut runs = Vec::new();
        self.runs(widest, from, to, &mut runs);
        runs.into_iter()
            .flat_map(move |run| self.levels[run.level.0].buckets_in(run.from, run.to))
    }

    /// Splits `from..to` into runs of whole buckets of `level` and narrower,
    /// in time order: the run of `level` buckets inside it, then the ends.
    fn runs(&self, level: LevelIndex, from: i64, to: i64, runs: &mut Vec<Run>) {
        if from >= to {
            return;
        }
        if level == DATE_BIN {
            runs.push(Run { level, from, to });
            return;
        }
        let narrower = LevelIndex(level.0 - 1);
        let width = self.levels[level.0].width;
        // A bucket boundary past either end of `i64` has no whole bucket
        // of this width inside the range on that side.
        let (Some(first), Some(last)) = (ceiling_multiple(from, width), floor_multiple(to, width))
        else {
            self.runs(narrower, from, to, runs);
            return;
        };
        if first >= last {
            self.runs(narrower, from, to, runs);
            return;
        }
        self.runs(narrower, from, first, runs);
        runs.push(Run {
            level,
            from: first,
            to: last,
        });
        self.runs(narrower, last, to, runs);
    }
}

/// The least multiple of `width` at or above `at`, if `i64` holds it.
fn ceiling_multiple(at: i64, width: i64) -> Option<i64> {
    let mut quotient = at.div_euclid(width);
    if at.rem_euclid(width) != 0 {
        quotient += 1;
    }
    quotient.checked_mul(width)
}

/// The greatest multiple of `width` at or below `at`, if `i64` holds it.
fn floor_multiple(at: i64, width: i64) -> Option<i64> {
    at.div_euclid(width).checked_mul(width)
}
