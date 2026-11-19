//! The aggregates a measure may name, and how an ask merges cells into
//! one answer per group. An ask hands each bucket's cell states to
//! [`Partials`], which keeps one running partial per group and turns it
//! into a [`Value`] at the end. Every match on `Aggregate` is here, so a
//! new aggregate is one variant and the arms the compiler asks for.

use crate::hash::{FastMap, FastSet};
use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::column::Counts;
use crate::gathered::{Gathered, Pick, gathered_none};
use crate::identifier::Identifier;
use crate::rollup::{DistinctCells, State, StateKind};
use crate::spread::{Moments, SPREAD_PLACES, Spread};
use crate::value::{Datum, Value};
use crate::value_counts::ValueCounts;
use crate::wide::U256;

/// What a measure reads of its input column, which decides the types the
/// column may have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reads {
    /// Integers, which a load makes of a numeric by its scale.
    Numbers,
    /// Values told apart, of any type but a time.
    Values,
    /// Whether there is a value, of any type.
    Nullness,
    /// True or false.
    Booleans,
}

/// What a measure gives an answer, which decides what HAVING may compare
/// it with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answers {
    /// Integers in the column's scale, an average of them, or a variance
    /// or deviation of them in a scale of its own.
    Numbers,
    /// True or false.
    Booleans,
}

/// The aggregate function of a measure, and the column it reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Aggregate {
    Count,
    CountNotNull(Identifier),
    Sum(Identifier),
    Min(Identifier),
    Max(Identifier),
    Avg(Identifier),
    CountDistinct(Identifier),
    BoolAnd(Identifier),
    BoolOr(Identifier),
    PercentileDisc {
        fraction: Fraction,
        column: Identifier,
    },
    VarSamp(Identifier),
    VarPop(Identifier),
    StddevSamp(Identifier),
    StddevPop(Identifier),
    Mode(Identifier),
}

/// A fraction from 0 to 1 as written: its digits and how many follow the
/// point, so a spec compares exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fraction {
    pub units: u64,
    pub places: u32,
}

impl Fraction {
    /// `None` above 1.
    pub fn new(units: u64, places: u32) -> Option<Fraction> {
        let one = 10u64.checked_pow(places)?;
        if units > one {
            return None;
        }
        Some(Fraction { units, places })
    }

    /// The 1-based position of the value `percentile_disc` gives among
    /// `total`. PostgreSQL takes `ceil(fraction * total)` in float8, which
    /// differs from exact math where the product falls near a whole number:
    /// 0.07 of 100 is rank 8 there, not 7.
    #[expect(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "PostgreSQL rounds in float8 too, and the product lies in 0..=total"
    )]
    pub(crate) fn rank(self, total: u64) -> u64 {
        let fraction: f64 = format!("{}e-{}", self.units, self.places)
            .parse()
            .expect("digits and an exponent parse as f64");
        ((fraction * total as f64).ceil() as u64).clamp(1, total)
    }
}

impl Aggregate {
    /// The column the aggregate reads; none for `count(*)`.
    pub fn input(&self) -> Option<&Identifier> {
        match self {
            Aggregate::Count => None,
            Aggregate::CountNotNull(column)
            | Aggregate::Sum(column)
            | Aggregate::Avg(column)
            | Aggregate::Min(column)
            | Aggregate::Max(column)
            | Aggregate::PercentileDisc { column, .. }
            | Aggregate::CountDistinct(column)
            | Aggregate::BoolAnd(column)
            | Aggregate::BoolOr(column)
            | Aggregate::VarSamp(column)
            | Aggregate::VarPop(column)
            | Aggregate::StddevSamp(column)
            | Aggregate::StddevPop(column)
            | Aggregate::Mode(column) => Some(column),
        }
    }

    /// The scale of the answer, for an input column of `column_scale`: a
    /// variance is in the column's units squared, so twice its scale, and
    /// a variance and a deviation keep [`SPREAD_PLACES`] more.
    pub(crate) fn answer_scale(&self, column_scale: u32) -> u32 {
        match self.spread() {
            Some(Spread::VarSamp | Spread::VarPop) => 2 * column_scale + SPREAD_PLACES,
            Some(Spread::StddevSamp | Spread::StddevPop) => column_scale + SPREAD_PLACES,
            None => column_scale,
        }
    }

    /// Which spread the aggregate gives, if it is a variance or deviation.
    fn spread(&self) -> Option<Spread> {
        match self {
            Aggregate::VarSamp(_) => Some(Spread::VarSamp),
            Aggregate::VarPop(_) => Some(Spread::VarPop),
            Aggregate::StddevSamp(_) => Some(Spread::StddevSamp),
            Aggregate::StddevPop(_) => Some(Spread::StddevPop),
            _ => None,
        }
    }

    /// What the measure's answer is: a number, or for `bool_and` and
    /// `bool_or` true or false.
    pub(crate) fn answers(&self) -> Answers {
        match self {
            Aggregate::BoolAnd(_) | Aggregate::BoolOr(_) => Answers::Booleans,
            Aggregate::Count
            | Aggregate::CountNotNull(_)
            | Aggregate::Sum(_)
            | Aggregate::Avg(_)
            | Aggregate::Min(_)
            | Aggregate::Max(_)
            | Aggregate::PercentileDisc { .. }
            | Aggregate::CountDistinct(_)
            | Aggregate::VarSamp(_)
            | Aggregate::VarPop(_)
            | Aggregate::StddevSamp(_)
            | Aggregate::StddevPop(_)
            | Aggregate::Mode(_) => Answers::Numbers,
        }
    }

    /// What the aggregate reads of its column; none for `count(*)`.
    pub fn reads(&self) -> Option<Reads> {
        let kind = self.state_kind()?;
        Some(kind.reads())
    }

    /// The form of cell state the aggregate keeps of its column; none for
    /// `count(*)`, which reads the cell's row count.
    pub(crate) fn state_kind(&self) -> Option<StateKind> {
        match self {
            Aggregate::Count => None,
            Aggregate::CountNotNull(_) => Some(StateKind::Count),
            Aggregate::Sum(_) | Aggregate::Avg(_) => Some(StateKind::Sum),
            Aggregate::Min(_)
            | Aggregate::Max(_)
            | Aggregate::PercentileDisc { .. }
            | Aggregate::Mode(_) => Some(StateKind::Extent),
            Aggregate::CountDistinct(_) => Some(StateKind::Distinct),
            Aggregate::BoolAnd(_) | Aggregate::BoolOr(_) => Some(StateKind::Bools),
            Aggregate::VarSamp(_)
            | Aggregate::VarPop(_)
            | Aggregate::StddevSamp(_)
            | Aggregate::StddevPop(_) => Some(StateKind::Spread),
        }
    }
}

/// Copies of each group's partial, cell `cell` merging into copy
/// `cell % LANES`. Most cells of a bucket can fall in one group, and adds
/// to one place in memory each wait on the last; spread over copies, they
/// overlap.
const LANES: usize = 4;

/// Where cell `cell` of group `group` merges among the partials.
fn lane(cell: usize, group: u32) -> usize {
    group as usize * LANES + cell % LANES
}

// The closures below are visitors: the column walks its cells in the
// width it keeps them in, and hands each count over.
// Benchmark: tests/cost.rs an_ask_stays_within_its_time_per_cell, by
// borough, least of 6 runs: 10.1 ns a cell with visitors, 15.3 ns with a
// `for` loop calling `Counts::get` and `Sums::get` per cell.

fn add_counts(partials: &mut [u64], groups: &[u32], counts: &Counts) {
    counts.each(groups, |cell, group, count| {
        partials[lane(cell, group)] += count;
    });
}

fn take_counts(partials: &mut [u64], groups: &[u32], counts: &Counts) {
    counts.each(groups, |cell, group, count| {
        partials[lane(cell, group)] -= count;
    });
}

/// The one group every cell of a bucket falls in, if they all fall in one.
fn single_group(groups: &[u32]) -> Option<u32> {
    let first = *groups.first()?;
    for &group in groups {
        if group != first {
            return None;
        }
    }
    Some(first)
}

/// Whether any cell holds a value: an empty cell's min lies above its max.
fn any_value(mins: &[i64], maxs: &[i64]) -> bool {
    for (min, max) in mins.iter().zip(maxs) {
        if min <= max {
            return true;
        }
    }
    false
}

/// Whether any count is above zero.
fn any_counted(counts: &[u64]) -> bool {
    for &count in counts {
        if count > 0 {
            return true;
        }
    }
    false
}

/// One measure merged over cells, per group: entries `group * LANES..`
/// are group `group`'s. Group 0 takes the cells an ask passes over, so
/// merging needs no test per cell; [`Partials::finish`] never reads it.
// `'s` borrows the cells the ask reads: a count of distinct values
// keeps references to their values rather than copies.
pub(crate) enum Partials<'s> {
    Count(Vec<u64>),
    /// A sum, an average, a variance and a deviation merge alike and part
    /// only when finished. Only the last two fill `squares`.
    Sum {
        sums: Vec<i128>,
        counts: Vec<u64>,
        squares: Vec<U256>,
        finish: Finish,
    },
    /// The least value, or `i64::MAX` while there is none, and whether
    /// there is one.
    Min {
        values: Vec<i64>,
        seen: Vec<bool>,
    },
    Max {
        values: Vec<i64>,
        seen: Vec<bool>,
    },
    Distinct(Vec<Distinct<'s>>),
    /// Each group's values with their counts, for a percentile or a mode.
    Percentile {
        pick: Pick,
        values: Vec<Gathered<'s>>,
    },
    /// A `bool_and` and a `bool_or` merge alike and part only when finished.
    Bools {
        trues: Vec<u64>,
        falses: Vec<u64>,
        all: bool,
    },
}

/// What a [`Partials::Sum`] gives when finished.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Finish {
    Sum,
    Avg,
    Spread(Spread),
}

/// The distinct values one group has met: the cells of numbers, read in
/// place, and the values of cells of other kinds.
pub(crate) struct Distinct<'s> {
    numbers: Numbers<'s>,
    values: Values<'s>,
}

/// The cells of numbers one group has met.
enum Numbers<'s> {
    None,
    /// Most groups of a fine ask meet one cell, which counts its values.
    OneCell(&'s ValueCounts),
    Cells(Vec<&'s ValueCounts>),
}

/// The values of the cells of other kinds one group has met.
enum Values<'s> {
    None,
    /// The value counts of the one cell met, read in place: copying its
    /// values cost as much memory as the cell holds.
    OneCell(&'s FastMap<Datum, u64>),
    /// The values of several cells, borrowed from them.
    Cells(FastSet<&'s Datum>),
}

impl<'s> Distinct<'s> {
    /// Meets the value counts of cell `cell` of `cells`.
    fn meet(&mut self, cells: &'s DistinctCells, cell: usize) {
        match cells {
            DistinctCells::Numbers(counts) => self.numbers.meet(&counts[cell]),
            DistinctCells::Values(counts) => self.values.meet(&counts[cell]),
        }
    }

    /// The values met, a number met both as a number and as a value
    /// counted once.
    fn count(&self) -> usize {
        let mut count = self.numbers.count();
        match &self.values {
            Values::None => {}
            Values::OneCell(values) => {
                for value in values.keys() {
                    count += usize::from(!self.numbers.has(value));
                }
            }
            Values::Cells(seen) => {
                for value in seen {
                    count += usize::from(!self.numbers.has(value));
                }
            }
        }
        count
    }
}

impl<'s> Numbers<'s> {
    fn meet(&mut self, counts: &'s ValueCounts) {
        match self {
            Numbers::None => *self = Numbers::OneCell(counts),
            Numbers::OneCell(first) => *self = Numbers::Cells(vec![*first, counts]),
            Numbers::Cells(cells) => cells.push(counts),
        }
    }

    fn count(&self) -> usize {
        match self {
            Numbers::None => 0,
            Numbers::OneCell(counts) => counts.len(),
            Numbers::Cells(cells) => distinct_in_order(cells),
        }
    }

    /// Whether a cell met holds `value`, a number.
    fn has(&self, value: &Datum) -> bool {
        let &Datum::Int(number) = value else {
            return false;
        };
        match self {
            Numbers::None => false,
            Numbers::OneCell(counts) => counts.has(number, 1),
            Numbers::Cells(cells) => {
                for counts in cells {
                    if counts.has(number, 1) {
                        return true;
                    }
                }
                false
            }
        }
    }
}

impl<'s> Values<'s> {
    fn meet(&mut self, values: &'s FastMap<Datum, u64>) {
        match self {
            Values::None => *self = Values::OneCell(values),
            Values::OneCell(first) => {
                let mut seen = FastSet::default();
                seen.extend(first.keys());
                seen.extend(values.keys());
                *self = Values::Cells(seen);
            }
            Values::Cells(seen) => seen.extend(values.keys()),
        }
    }
}

/// A cell's place among the cells a group met.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct MetIndex(usize);

/// The least value a cell has not yet given up, and the cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Head {
    value: i64,
    cell: MetIndex,
}

/// The distinct values of several cells, each read in value order: a
/// merge takes the least value any cell has left, and counts it when it
/// differs from the last one taken. The heap holds one value per cell,
/// never a set of all of them.
// A heap rather than a scan of every cell for the least: a group may
// meet thousands of cells, and the scan costs that many per value.
fn distinct_in_order(cells: &[&ValueCounts]) -> usize {
    let mut entries = Vec::with_capacity(cells.len());
    let mut heads = BinaryHeap::with_capacity(cells.len());
    for (position, counts) in cells.iter().enumerate() {
        let mut cell_entries = counts.iter();
        if let Some(entry) = cell_entries.next() {
            heads.push(Reverse(Head {
                value: entry.value,
                cell: MetIndex(position),
            }));
        }
        entries.push(cell_entries);
    }
    let mut count = 0;
    let mut last = None;
    while let Some(Reverse(head)) = heads.pop() {
        if last != Some(head.value) {
            count += 1;
            last = Some(head.value);
        }
        if let Some(entry) = entries[head.cell.0].next() {
            heads.push(Reverse(Head {
                value: entry.value,
                cell: head.cell,
            }));
        }
    }
    count
}

impl<'s> Partials<'s> {
    /// No group yet, for the measure's aggregate.
    pub(crate) fn new(aggregate: &Aggregate) -> Partials<'s> {
        match aggregate {
            Aggregate::Count | Aggregate::CountNotNull(_) => Partials::Count(Vec::new()),
            Aggregate::Sum(_) => Partials::sums(Finish::Sum),
            Aggregate::Avg(_) => Partials::sums(Finish::Avg),
            Aggregate::Min(_) => Partials::Min {
                values: Vec::new(),
                seen: Vec::new(),
            },
            Aggregate::Max(_) => Partials::Max {
                values: Vec::new(),
                seen: Vec::new(),
            },
            Aggregate::CountDistinct(_) => Partials::Distinct(Vec::new()),
            &Aggregate::PercentileDisc { fraction, .. } => Partials::Percentile {
                pick: Pick::Rank(fraction),
                values: Vec::new(),
            },
            Aggregate::Mode(_) => Partials::Percentile {
                pick: Pick::Mode,
                values: Vec::new(),
            },
            Aggregate::BoolAnd(_) | Aggregate::BoolOr(_) => Partials::Bools {
                trues: Vec::new(),
                falses: Vec::new(),
                all: matches!(aggregate, Aggregate::BoolAnd(_)),
            },
            Aggregate::VarSamp(_)
            | Aggregate::VarPop(_)
            | Aggregate::StddevSamp(_)
            | Aggregate::StddevPop(_) => {
                let spread = aggregate.spread().expect("a variance or deviation");
                Partials::sums(Finish::Spread(spread))
            }
        }
    }

    fn sums(finish: Finish) -> Partials<'s> {
        Partials::Sum {
            sums: Vec::new(),
            counts: Vec::new(),
            squares: Vec::new(),
            finish,
        }
    }

    /// Makes room for `groups` groups, group 0 counted.
    pub(crate) fn grow(&mut self, groups: usize) {
        let lanes = groups * LANES;
        match self {
            Partials::Count(counts) => counts.resize(lanes, 0),
            Partials::Sum {
                sums,
                counts,
                squares,
                finish,
            } => {
                sums.resize(lanes, 0);
                counts.resize(lanes, 0);
                if let Finish::Spread(_) = finish {
                    squares.resize(lanes, U256::ZERO);
                }
            }
            Partials::Min { values, seen } => {
                values.resize(lanes, i64::MAX);
                seen.resize(lanes, false);
            }
            Partials::Max { values, seen } => {
                values.resize(lanes, i64::MIN);
                seen.resize(lanes, false);
            }
            // A set per lane would cost a merge of sets; one per group.
            Partials::Distinct(seen) => seen.resize_with(groups, distinct_none),
            Partials::Percentile { values, .. } => values.resize_with(groups, gathered_none),
            Partials::Bools { trues, falses, .. } => {
                trues.resize(lanes, 0);
                falses.resize(lanes, 0);
            }
        }
    }

    /// Merges every cell of a bucket into its group, `groups` giving each
    /// cell's. `rows` are the cells' rows, which a count is and which a
    /// column's nulls are taken from.
    pub(crate) fn merge(&mut self, groups: &[u32], rows: &Counts, state: Option<&'s State>) {
        if let Some(group) = single_group(groups) {
            return self.merge_into(group, rows, state);
        }
        match (self, state) {
            (Partials::Count(counts), None) => add_counts(counts, groups, rows),
            (Partials::Count(counts), Some(State::Count { nulls })) => {
                add_counts(counts, groups, rows);
                take_counts(counts, groups, nulls);
            }
            (
                Partials::Sum {
                    sums,
                    counts,
                    squares,
                    ..
                },
                Some(State::Sum {
                    sums: cell_sums,
                    nulls,
                    squares: cell_squares,
                }),
            ) => {
                // Benchmark: as for `add_counts`.
                cell_sums.each(rows, groups, |cell, group, sum, count| {
                    let at = lane(cell, group);
                    sums[at] += sum;
                    counts[at] += count;
                });
                take_counts(counts, groups, nulls);
                if let Some(cell_squares) = cell_squares
                    && !squares.is_empty()
                {
                    for (cell, &group) in groups.iter().enumerate() {
                        let at = lane(cell, group);
                        squares[at] = squares[at].wrapping_add(cell_squares.get(cell));
                    }
                }
            }
            (Partials::Min { values, seen }, Some(State::Extent { mins, maxs, .. })) => {
                let cells = groups.iter().zip(mins).zip(maxs);
                for (cell, ((&group, &min), &max)) in cells.enumerate() {
                    let at = lane(cell, group);
                    values[at] = values[at].min(min);
                    seen[at] |= min <= max;
                }
            }
            (Partials::Max { values, seen }, Some(State::Extent { mins, maxs, .. })) => {
                let cells = groups.iter().zip(mins).zip(maxs);
                for (cell, ((&group, &min), &max)) in cells.enumerate() {
                    let at = lane(cell, group);
                    values[at] = values[at].max(max);
                    seen[at] |= min <= max;
                }
            }
            (partials, state) => partials.merge_collected(groups, state),
        }
    }

    /// The rest of [`Partials::merge`]: the aggregates that collect values
    /// rather than add them, and the booleans.
    fn merge_collected(&mut self, groups: &[u32], state: Option<&'s State>) {
        match (self, state) {
            (Partials::Distinct(seen), Some(State::Distinct(cells))) => {
                // Unlike a sum, a value set costs its group 0 merge.
                for (cell, &group) in groups.iter().enumerate() {
                    if group != 0 {
                        seen[group as usize].meet(cells, cell);
                    }
                }
            }
            (Partials::Percentile { values, .. }, Some(State::Extent { counts, .. })) => {
                for (&group, counts) in groups.iter().zip(counts) {
                    if group != 0 {
                        values[group as usize].meet(counts);
                    }
                }
            }
            (
                Partials::Bools { trues, falses, .. },
                Some(State::Bools {
                    trues: cell_trues,
                    falses: cell_falses,
                }),
            ) => {
                add_counts(trues, groups, cell_trues);
                add_counts(falses, groups, cell_falses);
            }
            _ => unreachable!("Aggregate::state_kind names the state the measure reads"),
        }
    }

    /// One cell into one group, for a cell that counts more than once or
    /// under several groups.
    pub(crate) fn merge_cell(
        &mut self,
        group: u32,
        cell: usize,
        rows: &Counts,
        state: Option<&'s State>,
    ) {
        let at = lane(cell, group);
        match (self, state) {
            (Partials::Count(counts), None) => counts[at] += rows.get(cell),
            (Partials::Count(counts), Some(State::Count { nulls })) => {
                counts[at] += rows.get(cell) - nulls.get(cell);
            }
            (
                Partials::Sum {
                    sums,
                    counts,
                    squares,
                    ..
                },
                Some(State::Sum {
                    sums: cell_sums,
                    nulls,
                    squares: cell_squares,
                }),
            ) => {
                sums[at] += cell_sums.get(cell);
                counts[at] += rows.get(cell) - nulls.get(cell);
                if let Some(cell_squares) = cell_squares
                    && !squares.is_empty()
                {
                    squares[at] = squares[at].wrapping_add(cell_squares.get(cell));
                }
            }
            (Partials::Min { values, seen }, Some(State::Extent { mins, maxs, .. })) => {
                values[at] = values[at].min(mins[cell]);
                seen[at] |= mins[cell] <= maxs[cell];
            }
            (Partials::Max { values, seen }, Some(State::Extent { mins, maxs, .. })) => {
                values[at] = values[at].max(maxs[cell]);
                seen[at] |= mins[cell] <= maxs[cell];
            }
            (Partials::Distinct(seen), Some(State::Distinct(cells))) if group != 0 => {
                seen[group as usize].meet(cells, cell);
            }
            (Partials::Percentile { values, .. }, Some(State::Extent { counts, .. }))
                if group != 0 =>
            {
                values[group as usize].meet(&counts[cell]);
            }
            // Group 0 is never finished, and a value set or list costs its merge.
            (Partials::Distinct(_), Some(State::Distinct(_)))
            | (Partials::Percentile { .. }, Some(State::Extent { .. })) => {}
            (
                Partials::Bools { trues, falses, .. },
                Some(State::Bools {
                    trues: cell_trues,
                    falses: cell_falses,
                }),
            ) => {
                trues[at] += cell_trues.get(cell);
                falses[at] += cell_falses.get(cell);
            }
            _ => unreachable!("Aggregate::state_kind names the state the measure reads"),
        }
    }

    /// Every cell of a bucket into one group. Folding in registers beats
    /// adding to one place in memory, each add waiting on the last.
    fn merge_into(&mut self, group: u32, rows: &Counts, state: Option<&'s State>) {
        let at = lane(0, group);
        let group = group as usize;
        match (self, state) {
            (Partials::Count(counts), None) => counts[at] += rows.total(),
            (Partials::Count(counts), Some(State::Count { nulls })) => {
                counts[at] += rows.total() - nulls.total();
            }
            (
                Partials::Sum {
                    sums,
                    counts,
                    squares,
                    ..
                },
                Some(State::Sum {
                    sums: cell_sums,
                    nulls,
                    squares: cell_squares,
                }),
            ) => {
                sums[at] += cell_sums.total();
                counts[at] += rows.total() - nulls.total();
                if let Some(cell_squares) = cell_squares
                    && !squares.is_empty()
                {
                    for cell in 0..cell_squares.len() {
                        squares[at] = squares[at].wrapping_add(cell_squares.get(cell));
                    }
                }
            }
            (Partials::Min { values, seen }, Some(State::Extent { mins, maxs, .. })) => {
                for &min in mins {
                    values[at] = values[at].min(min);
                }
                seen[at] |= any_value(mins, maxs);
            }
            (Partials::Max { values, seen }, Some(State::Extent { mins, maxs, .. })) => {
                for &max in maxs {
                    values[at] = values[at].max(max);
                }
                seen[at] |= any_value(mins, maxs);
            }
            (Partials::Distinct(seen), Some(State::Distinct(cells))) if group != 0 => {
                for cell in 0..cells.len() {
                    seen[group].meet(cells, cell);
                }
            }
            (Partials::Percentile { values, .. }, Some(State::Extent { counts, .. }))
                if group != 0 =>
            {
                for cell in counts {
                    values[group].meet(cell);
                }
            }
            // Group 0 is never finished, and a value set or list costs its merge.
            (Partials::Distinct(_), Some(State::Distinct(_)))
            | (Partials::Percentile { .. }, Some(State::Extent { .. })) => {}
            (
                Partials::Bools { trues, falses, .. },
                Some(State::Bools {
                    trues: cell_trues,
                    falses: cell_falses,
                }),
            ) => {
                trues[at] += cell_trues.total();
                falses[at] += cell_falses.total();
            }
            _ => unreachable!("Aggregate::state_kind names the state the measure reads"),
        }
    }

    /// The answer of group `group`, from its partials.
    pub(crate) fn finish(&mut self, group: usize) -> Value {
        let lanes = group * LANES..(group + 1) * LANES;
        match self {
            Partials::Count(counts) => Value::Int(counts[lanes].iter().sum::<u64>().into()),
            Partials::Sum {
                sums,
                counts,
                squares,
                finish,
            } => {
                let count: u64 = counts[lanes.clone()].iter().sum();
                let sum: i128 = sums[lanes.clone()].iter().sum();
                match finish {
                    _ if count == 0 => Value::Null,
                    Finish::Sum => Value::Int(sum),
                    Finish::Avg => Value::Avg { sum, count },
                    Finish::Spread(spread) => {
                        let mut total = U256::ZERO;
                        for &square in &squares[lanes] {
                            total = total.wrapping_add(square);
                        }
                        spread.answer(Moments {
                            count,
                            sum,
                            squares: total,
                        })
                    }
                }
            }
            Partials::Min { seen, .. } | Partials::Max { seen, .. }
                if !seen[lanes.clone()].contains(&true) =>
            {
                Value::Null
            }
            Partials::Min { values, .. } => {
                let mut least = i64::MAX;
                for &value in &values[lanes] {
                    least = least.min(value);
                }
                Value::Int(least.into())
            }
            Partials::Max { values, .. } => {
                let mut most = i64::MIN;
                for &value in &values[lanes] {
                    most = most.max(value);
                }
                Value::Int(most.into())
            }
            Partials::Distinct(seen) => Value::Int(seen[group].count() as i128),
            Partials::Percentile { pick, values } => values[group].answer(*pick),
            Partials::Bools { trues, falses, all } => {
                let any_true = any_counted(&trues[lanes.clone()]);
                let any_false = any_counted(&falses[lanes]);
                match (any_true, any_false) {
                    (false, false) => Value::Null,
                    _ if *all => Value::Bool(!any_false),
                    _ => Value::Bool(any_true),
                }
            }
        }
    }
}

/// A group that has met no value yet.
fn distinct_none<'s>() -> Distinct<'s> {
    Distinct {
        numbers: Numbers::None,
        values: Values::None,
    }
}
