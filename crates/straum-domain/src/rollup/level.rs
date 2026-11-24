//! A level: the cells of one bucket width, bucket by bucket in time
//! order. A row goes into the cell of its key in the bucket its time
//! falls in, at every level. An ask reads a range as a cover: the widest
//! buckets that fit inside it, narrower ones at its ends. A level kept
//! only for a time has a horizon: `advance` moves it on and drops the
//! buckets before it, a row before it goes into the wider levels only,
//! and a cover that would read before it is refused.

use std::collections::BTreeMap;
use std::collections::btree_map::{Entry, OccupiedEntry};
use std::mem;
use std::ops::Range;

use crate::ask::AskError;
use crate::value::Datum;
use crate::width::Width;

use super::cells::Cells;
use super::row_error::RowError;
use super::state::State;
use super::{BucketId, Cover, CoveredBucket, DATE_BIN, Direction, Input, LevelIndex, Rollup, TIME};

/// The cells of one bucket width.
#[derive(Debug)]
pub(super) struct Level {
    pub(super) width: Width,
    /// Oldest first. `None` holds the rows without a time, which no ask reads.
    pub(super) buckets: BTreeMap<Option<i64>, Cells>,
    /// Set for a width kept only for a time.
    pub(super) horizon: Option<Horizon>,
}

/// How long a level keeps its buckets, and where it keeps them from.
#[derive(Debug)]
pub(super) struct Horizon {
    pub(super) kept_for: Width,
    /// The next wider width: the horizon lands on its edges, so a wider
    /// bucket is either wholly kept here or wholly dropped.
    pub(super) next_wider: Width,
    /// The start of the first bucket kept; none before any `advance`.
    pub(super) kept_from: Option<i64>,
}

/// A width kept only for a time, and the start of its first bucket kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeptFrom {
    pub width: Width,
    /// `None` before the first `advance`.
    pub kept_from: Option<i64>,
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
#[derive(Clone, Copy)]
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
            // Unchecked: `Rollup::check_time` turned away every time whose
            // bucket start `i64` can't hold before the row got here.
            Datum::Int(seconds) => {
                let width = i64::from(self.width.seconds);
                Some(seconds.div_euclid(width) * width)
            }
            _ => None,
        }
    }

    /// Adds a checked row under its key codes.
    pub(super) fn add(&mut self, row: &[Datum], key: &[u32], inputs: &[Input]) -> Shift {
        if self.dropped(&row[TIME]) {
            return Shift::Same;
        }
        let cells = self.cells_of(&row[TIME], key.len(), inputs);
        let entry = cells.entry(key);
        let cell = entry.position;
        cells.rows.add(cell, 1);
        for (state, input) in cells.states.iter_mut().zip(inputs) {
            state.add(cell, &row[input.column.0]);
        }
        if entry.new { Shift::New } else { Shift::Same }
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
            if let Shift::New = self.add(row, key, inputs) {
                new_cells += 1;
            }
        }
        new_cells
    }

    /// Takes a row out of the cell of `key`.
    pub(super) fn remove(
        &mut self,
        row: &[Datum],
        key: &[u32],
        inputs: &[Input],
    ) -> Result<Shift, RowError> {
        if self.dropped(&row[TIME]) {
            return Ok(Shift::Same);
        }
        let Entry::Occupied(mut bucket) = self.buckets.entry(self.bucket(&row[TIME])) else {
            return Err(RowError::Missing);
        };
        let cells = bucket.get_mut();
        let cell = cells.find(key).ok_or(RowError::Missing)?;
        let rows = cells.rows.get(cell);
        for (state, input) in cells.states.iter_mut().zip(inputs) {
            state.remove(cell, rows, &row[input.column.0])?;
        }
        cells.rows.sub(cell, 1);
        if rows > 1 {
            return Ok(Shift::Same);
        }
        drop_cell(bucket, cell);
        Ok(Shift::Removed)
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
        let Entry::Occupied(mut bucket) = self.buckets.entry(self.bucket(time)) else {
            return Err(RowError::Missing);
        };
        let cells = bucket.get_mut();
        let Some(cell) = cells.find(key) else {
            return Err(RowError::Missing);
        };
        if !cells.release(cell, count, source, source_cell) {
            return Err(RowError::Missing);
        }
        if cells.rows.get(cell) != 0 {
            return Ok(Shift::Same);
        }
        drop_cell(bucket, cell);
        Ok(Shift::Removed)
    }

    /// Whether `time` falls before the horizon, in a bucket this level
    /// dropped or never makes. A null time falls in no bucket an ask reads,
    /// so its bucket is always kept.
    fn dropped(&self, time: &Datum) -> bool {
        let (Some(start), Some(kept_from)) = (self.bucket(time), self.kept_from()) else {
            return false;
        };
        start < kept_from
    }

    /// The start of the first bucket kept, once a horizon is set.
    fn kept_from(&self) -> Option<i64> {
        self.horizon.as_ref()?.kept_from
    }

    /// Moves the horizon to `at` less the time kept, down to an edge of
    /// the next wider width, and drops every bucket before it; how many
    /// cells went. A horizon never moves back.
    pub(super) fn advance(&mut self, at: i64) -> usize {
        let Some(horizon) = &mut self.horizon else {
            return 0;
        };
        // Past the start of `i64` there is no bucket to drop.
        let Some(back) = at.checked_sub(i64::from(horizon.kept_for.seconds)) else {
            return 0;
        };
        let Some(moved) = horizon.next_wider.bucket_start(back) else {
            return 0;
        };
        if let Some(kept_from) = horizon.kept_from
            && kept_from >= moved
        {
            return 0;
        }
        horizon.kept_from = Some(moved);
        // `None` sorts first, so `split_off` would drop the null-time
        // bucket with the old ones: it is taken out and put back.
        let untimed = self.buckets.remove(&None);
        let kept = self.buckets.split_off(&Some(moved));
        let old = mem::replace(&mut self.buckets, kept);
        if let Some(cells) = untimed {
            self.buckets.insert(None, cells);
        }
        let mut dropped = 0;
        for cells in old.values() {
            dropped += cells.len();
        }
        dropped
    }

    /// From the start of the first bucket to the end of the last; none
    /// while the level holds no bucket with a time.
    pub(super) fn span(&self) -> Option<Range<i64>> {
        // `flatten` passes over the `None` key of the null-time bucket.
        let mut starts = self.buckets.keys().flatten();
        let first = *starts.next()?;
        let last = match starts.next_back() {
            Some(last) => *last,
            None => first,
        };
        Some(first..last + i64::from(self.width.seconds))
    }

    // An iterator, not a `Vec`: an ask allocates per group, never per
    // bucket (`an_ask_allocates_per_group_not_per_cell` in tests/cost/main.rs).
    fn buckets_in(&self, from: i64, to: i64) -> impl Iterator<Item = CoveredBucket<'_>> {
        let width = i64::from(self.width.seconds);
        let buckets = self.buckets.range(Some(from)..Some(to));
        buckets.map(move |(start, cells)| covered(width, *start, cells))
    }
}

/// Takes out `cell`, whose last row went, and its bucket with it once
/// the bucket holds no cell.
fn drop_cell(mut bucket: OccupiedEntry<'_, Option<i64>, Cells>, cell: usize) {
    let cells = bucket.get_mut();
    cells.swap_remove(cell);
    if cells.len() == 0 {
        bucket.remove();
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
    // bucket (`an_ask_allocates_per_group_not_per_cell` in tests/cost/main.rs).
    // So the runs' buckets are chained with `flat_map` rather than
    // gathered in a loop.
    pub(crate) fn cover(
        &self,
        from: i64,
        to: i64,
        cover: Cover,
    ) -> impl Iterator<Item = CoveredBucket<'_>> {
        (self.runs_of(from, to, cover).into_iter())
            .flat_map(move |run| self.levels[run.level.0].buckets_in(run.from, run.to))
    }

    /// A refusal for each level kept for a time whose buckets the cover of
    /// `from..to` would read from before its horizon, narrowest first.
    /// Asked of the bounds alone, so a range where no row was ever held is
    /// refused too.
    pub(crate) fn past_horizons(&self, from: i64, to: i64, cover: Cover) -> Vec<AskError> {
        let runs = self.runs_of(from, to, cover);
        let mut past = Vec::new();
        for (index, level) in self.levels.iter().enumerate() {
            let Some(kept_from) = level.kept_from() else {
                continue;
            };
            for run in &runs {
                if run.level == LevelIndex(index) && run.from < kept_from {
                    past.push(AskError::PastRetention {
                        width: level.width,
                        kept_from,
                    });
                    break;
                }
            }
        }
        past
    }

    /// What an ask with an open end reads: the `date_bin` buckets held,
    /// while that level keeps every bucket. Once it is kept for a time,
    /// from the first bucket of the widest level, which keeps every row,
    /// to the end of the last `date_bin` bucket, or of the last widest
    /// one while the `date_bin` level holds none. A narrower level may
    /// have dropped its first buckets, never its last.
    pub(crate) fn open_range(&self) -> Option<Range<i64>> {
        if self.levels[DATE_BIN.0].horizon.is_none() {
            return self.levels[DATE_BIN.0].span();
        }
        let widest = self.levels[self.widest().0].span()?;
        let end = match self.levels[DATE_BIN.0].span() {
            Some(date_bin) => date_bin.end,
            None => widest.end,
        };
        Some(widest.start..end)
    }

    /// Each width kept only for a time, narrowest first.
    pub(crate) fn horizons(&self) -> Vec<KeptFrom> {
        let mut horizons = Vec::new();
        for level in &self.levels {
            let Some(horizon) = &level.horizon else {
                continue;
            };
            horizons.push(KeptFrom {
                width: level.width,
                kept_from: horizon.kept_from,
            });
        }
        horizons
    }

    /// The runs of whole buckets that cover `from..to` as `cover` says.
    fn runs_of(&self, from: i64, to: i64, cover: Cover) -> Vec<Run> {
        let widest = match cover {
            Cover::NoWiderThan(width) => self.level_of(width),
            Cover::WidestFit => self.widest(),
        };
        let mut runs = Vec::new();
        self.runs(widest, from, to, &mut runs);
        runs
    }

    /// The level of `width`, which the view keeps.
    fn level_of(&self, width: Width) -> LevelIndex {
        for (index, level) in self.levels.iter().enumerate() {
            if level.width == width {
                return LevelIndex(index);
            }
        }
        panic!("the spec keeps every width an ask's rows take")
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
fn ceiling_multiple(at: i64, width: Width) -> Option<i64> {
    let seconds = i64::from(width.seconds);
    let mut quotient = at.div_euclid(seconds);
    if at.rem_euclid(seconds) != 0 {
        quotient += 1;
    }
    quotient.checked_mul(seconds)
}

/// The greatest multiple of `width` at or below `at`, if `i64` holds it.
fn floor_multiple(at: i64, width: Width) -> Option<i64> {
    width.bucket_start(at)
}
