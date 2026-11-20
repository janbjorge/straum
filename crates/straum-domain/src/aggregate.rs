//! The aggregates a measure may name, and how an ask merges cells into
//! one answer per group. An ask hands each bucket's cell states to
//! [`Partials`], which keeps one running partial per group and turns it
//! into a [`Value`] at the end. Every match on `Aggregate` is here, so a
//! new aggregate is one variant and the arms the compiler asks for.

use crate::column::{Counts, Squares};
use crate::gathered::{Gathered, Pick, gathered_none};
use crate::identifier::Identifier;
use crate::rollup::{State, StateKind};
use crate::spread::{Moments, SPREAD_PLACES, Spread};
use crate::tally::{Distinct, Tallied, Tally, distinct_none};
use crate::value::Value;
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
    SumDistinct(Identifier),
    AvgDistinct(Identifier),
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
            | Aggregate::Mode(column)
            | Aggregate::SumDistinct(column)
            | Aggregate::AvgDistinct(column) => Some(column),
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
            | Aggregate::Mode(_)
            | Aggregate::SumDistinct(_)
            | Aggregate::AvgDistinct(_) => Answers::Numbers,
        }
    }

    /// What the aggregate reads of its column; none for `count(*)`.
    pub fn reads(&self) -> Option<Reads> {
        let kind = self.state_kind()?;
        match self {
            // Adds its values, so they must be numbers, though the state
            // it shares with `count(DISTINCT c)` tells any values apart.
            Aggregate::SumDistinct(_) | Aggregate::AvgDistinct(_) => Some(Reads::Numbers),
            _ => Some(kind.reads()),
        }
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
            Aggregate::CountDistinct(_) | Aggregate::SumDistinct(_) | Aggregate::AvgDistinct(_) => {
                Some(StateKind::Distinct)
            }
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
// Benchmark: tests/cost/main.rs an_ask_stays_within_its_time_per_cell, by
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

/// Adds the sum of squares of every cell into lane `at`. Only a variance
/// or standard deviation fills `squares`, and only its cells hold them.
fn merge_squares(squares: &mut [U256], at: usize, cell_squares: Option<&Squares>) {
    let Some(cell_squares) = cell_squares else {
        return;
    };
    if squares.is_empty() {
        return;
    }
    for cell in 0..cell_squares.len() {
        squares[at] = squares[at].wrapping_add(cell_squares.get(cell));
    }
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
    /// `count`, `sum` and `avg` of distinct values merge alike and part
    /// only when finished. Each group's tally is kept once worked out, so
    /// the other distinct measures of the column read it too.
    Distinct {
        seen: Vec<Distinct<'s>>,
        tallies: Vec<Option<Tally>>,
        tallied: Tallied,
        finish: DistinctFinish,
    },
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

/// What a [`Partials::Distinct`] gives when finished.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DistinctFinish {
    Count,
    Sum,
    Avg,
}

/// What a [`Partials::Sum`] gives when finished.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Finish {
    Sum,
    Avg,
    Spread(Spread),
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
            Aggregate::CountDistinct(_) => Partials::distinct(DistinctFinish::Count),
            Aggregate::SumDistinct(_) => Partials::distinct(DistinctFinish::Sum),
            Aggregate::AvgDistinct(_) => Partials::distinct(DistinctFinish::Avg),
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

    fn distinct(finish: DistinctFinish) -> Partials<'s> {
        Partials::Distinct {
            seen: Vec::new(),
            tallies: Vec::new(),
            tallied: finish.tallied(),
            finish,
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
            Partials::Distinct { seen, tallies, .. } => {
                seen.resize_with(groups, distinct_none);
                tallies.resize(groups, None);
            }
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
            (Partials::Distinct { seen, .. }, Some(State::Distinct(cells))) => {
                // Unlike a sum, a value set costs its group 0 merge.
                for (cell, &group) in groups.iter().enumerate() {
                    if group != 0 {
                        seen[group as usize].meet(cells, cell);
                    }
                }
            }
            (Partials::Distinct { seen, .. }, Some(State::Extent { counts, .. })) => {
                for (&group, counts) in groups.iter().zip(counts) {
                    if group != 0 {
                        seen[group as usize].meet_numbers(counts);
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
            (Partials::Distinct { seen, .. }, Some(State::Distinct(cells))) if group != 0 => {
                seen[group as usize].meet(cells, cell);
            }
            (Partials::Distinct { seen, .. }, Some(State::Extent { counts, .. })) if group != 0 => {
                seen[group as usize].meet_numbers(&counts[cell]);
            }
            (Partials::Percentile { values, .. }, Some(State::Extent { counts, .. }))
                if group != 0 =>
            {
                values[group as usize].meet(&counts[cell]);
            }
            // Group 0 is never finished, and a value set or list costs its merge.
            (Partials::Distinct { .. }, Some(State::Distinct(_) | State::Extent { .. }))
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
                merge_squares(squares, at, cell_squares.as_ref());
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
            (Partials::Distinct { seen, .. }, Some(State::Distinct(cells))) if group != 0 => {
                for cell in 0..cells.len() {
                    seen[group].meet(cells, cell);
                }
            }
            (Partials::Distinct { seen, .. }, Some(State::Extent { counts, .. })) if group != 0 => {
                for cell in counts {
                    seen[group].meet_numbers(cell);
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
            (Partials::Distinct { .. }, Some(State::Distinct(_) | State::Extent { .. }))
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
            Partials::Distinct { finish, .. } => {
                let finish = *finish;
                self.finish_as(group, finish)
            }
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

impl DistinctFinish {
    /// What a tally must hold to give this answer.
    fn tallied(self) -> Tallied {
        match self {
            DistinctFinish::Count => Tallied::Count,
            DistinctFinish::Sum | DistinctFinish::Avg => Tallied::CountAndSum,
        }
    }
}

impl Partials<'_> {
    /// The answer a distinct measure gives, when this is one.
    pub(crate) fn distinct_finish(&self) -> Option<DistinctFinish> {
        match self {
            Partials::Distinct { finish, .. } => Some(*finish),
            _ => None,
        }
    }

    /// Readies these distinct partials to give `finish` as well, from the
    /// same tallies.
    pub(crate) fn share(&mut self, finish: DistinctFinish) {
        let Partials::Distinct { tallied, .. } = self else {
            unreachable!("only distinct partials are shared");
        };
        if let Tallied::CountAndSum = finish.tallied() {
            *tallied = Tallied::CountAndSum;
        }
    }

    /// Group `group`'s distinct values as `finish` gives them: their
    /// count, or their sum or average, null when there are none. The
    /// tally is worked out on first ask and kept.
    pub(crate) fn finish_as(&mut self, group: usize, finish: DistinctFinish) -> Value {
        let Partials::Distinct {
            seen,
            tallies,
            tallied,
            ..
        } = self
        else {
            unreachable!("only distinct partials are shared");
        };
        if tallies[group].is_none() {
            tallies[group] = Some(seen[group].tally(*tallied));
        }
        let tally = tallies[group].expect("worked out above");
        match finish {
            DistinctFinish::Count => Value::Int(tally.count as i128),
            _ if tally.count == 0 => Value::Null,
            DistinctFinish::Sum => Value::Int(tally.sum),
            DistinctFinish::Avg => Value::Avg {
                sum: tally.sum,
                count: tally.count as u64,
            },
        }
    }
}
