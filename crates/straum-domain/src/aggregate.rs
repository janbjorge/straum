//! The aggregates a measure may name, and how an ask merges cells into
//! one answer per group. An ask hands each bucket's cell states to
//! [`Partials`], which keeps one running partial per group and turns it
//! into a [`Value`] at the end. Every match on `Aggregate` is here, so a
//! new aggregate is one variant and the arms the compiler asks for.

use std::collections::HashSet;
use std::mem;

use crate::column::Counts;
use crate::identifier::Identifier;
use crate::rollup::{State, StateKind};
use crate::value::{Datum, Value};
use crate::value_counts::ValueCount;

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
    /// Integers in the column's scale, or an average of them.
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
            | Aggregate::BoolOr(column) => Some(column),
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
            | Aggregate::CountDistinct(_) => Answers::Numbers,
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
            Aggregate::Min(_) | Aggregate::Max(_) | Aggregate::PercentileDisc { .. } => {
                Some(StateKind::Extent)
            }
            Aggregate::CountDistinct(_) => Some(StateKind::Distinct),
            Aggregate::BoolAnd(_) | Aggregate::BoolOr(_) => Some(StateKind::Bools),
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
pub(crate) enum Partials {
    Count(Vec<u64>),
    /// A sum and an average merge alike and part only when finished.
    Sum {
        sums: Vec<i128>,
        counts: Vec<u64>,
        average: bool,
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
    Distinct(Vec<HashSet<Datum>>),
    /// Each group's values with their counts, one entry per cell a value
    /// is in, in no order.
    Percentile {
        fraction: Fraction,
        values: Vec<Vec<ValueCount>>,
    },
    /// A `bool_and` and a `bool_or` merge alike and part only when finished.
    Bools {
        trues: Vec<u64>,
        falses: Vec<u64>,
        all: bool,
    },
}

impl Partials {
    /// No group yet, for the measure's aggregate.
    pub(crate) fn new(aggregate: &Aggregate) -> Partials {
        match aggregate {
            Aggregate::Count | Aggregate::CountNotNull(_) => Partials::Count(Vec::new()),
            Aggregate::Sum(_) | Aggregate::Avg(_) => Partials::Sum {
                sums: Vec::new(),
                counts: Vec::new(),
                average: matches!(aggregate, Aggregate::Avg(_)),
            },
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
                fraction,
                values: Vec::new(),
            },
            Aggregate::BoolAnd(_) | Aggregate::BoolOr(_) => Partials::Bools {
                trues: Vec::new(),
                falses: Vec::new(),
                all: matches!(aggregate, Aggregate::BoolAnd(_)),
            },
        }
    }

    /// Makes room for `groups` groups, group 0 counted.
    pub(crate) fn grow(&mut self, groups: usize) {
        let lanes = groups * LANES;
        match self {
            Partials::Count(counts) => counts.resize(lanes, 0),
            Partials::Sum { sums, counts, .. } => {
                sums.resize(lanes, 0);
                counts.resize(lanes, 0);
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
            Partials::Distinct(seen) => seen.resize_with(groups, HashSet::new),
            Partials::Percentile { values, .. } => values.resize_with(groups, Vec::new),
            Partials::Bools { trues, falses, .. } => {
                trues.resize(lanes, 0);
                falses.resize(lanes, 0);
            }
        }
    }

    /// Merges every cell of a bucket into its group, `groups` giving each
    /// cell's. `rows` are the cells' rows, which a count is and which a
    /// column's nulls are taken from.
    pub(crate) fn merge(&mut self, groups: &[u32], rows: &Counts, state: Option<&State>) {
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
                Partials::Sum { sums, counts, .. },
                Some(State::Sum {
                    sums: cell_sums,
                    nulls,
                }),
            ) => {
                // Benchmark: as for `add_counts`.
                cell_sums.each(rows, groups, |cell, group, sum, count| {
                    let at = lane(cell, group);
                    sums[at] += sum;
                    counts[at] += count;
                });
                take_counts(counts, groups, nulls);
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
    fn merge_collected(&mut self, groups: &[u32], state: Option<&State>) {
        match (self, state) {
            (Partials::Distinct(seen), Some(State::Distinct(counts))) => {
                // Unlike a sum, a value set costs its group 0 merge.
                for (&group, counts) in groups.iter().zip(counts) {
                    if group != 0 {
                        seen[group as usize].extend(counts.keys().cloned());
                    }
                }
            }
            (Partials::Percentile { values, .. }, Some(State::Extent { counts, .. })) => {
                for (&group, counts) in groups.iter().zip(counts) {
                    if group != 0 {
                        values[group as usize].extend(counts.iter());
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
            _ => unreachable!("Aggregate::state names the column the measure reads"),
        }
    }

    /// One cell into one group, for a cell that counts more than once or
    /// under several groups.
    pub(crate) fn merge_cell(
        &mut self,
        group: u32,
        cell: usize,
        rows: &Counts,
        state: Option<&State>,
    ) {
        let at = lane(cell, group);
        match (self, state) {
            (Partials::Count(counts), None) => counts[at] += rows.get(cell),
            (Partials::Count(counts), Some(State::Count { nulls })) => {
                counts[at] += rows.get(cell) - nulls.get(cell);
            }
            (
                Partials::Sum { sums, counts, .. },
                Some(State::Sum {
                    sums: cell_sums,
                    nulls,
                }),
            ) => {
                sums[at] += cell_sums.get(cell);
                counts[at] += rows.get(cell) - nulls.get(cell);
            }
            (Partials::Min { values, seen }, Some(State::Extent { mins, maxs, .. })) => {
                values[at] = values[at].min(mins[cell]);
                seen[at] |= mins[cell] <= maxs[cell];
            }
            (Partials::Max { values, seen }, Some(State::Extent { mins, maxs, .. })) => {
                values[at] = values[at].max(maxs[cell]);
                seen[at] |= mins[cell] <= maxs[cell];
            }
            (Partials::Distinct(seen), Some(State::Distinct(counts))) if group != 0 => {
                seen[group as usize].extend(counts[cell].keys().cloned());
            }
            (Partials::Percentile { values, .. }, Some(State::Extent { counts, .. }))
                if group != 0 =>
            {
                values[group as usize].extend(counts[cell].iter());
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
            _ => unreachable!("Aggregate::state names the column the measure reads"),
        }
    }

    /// Every cell of a bucket into one group. Folding in registers beats
    /// adding to one place in memory, each add waiting on the last.
    fn merge_into(&mut self, group: u32, rows: &Counts, state: Option<&State>) {
        let at = lane(0, group);
        let group = group as usize;
        match (self, state) {
            (Partials::Count(counts), None) => counts[at] += rows.total(),
            (Partials::Count(counts), Some(State::Count { nulls })) => {
                counts[at] += rows.total() - nulls.total();
            }
            (
                Partials::Sum { sums, counts, .. },
                Some(State::Sum {
                    sums: cell_sums,
                    nulls,
                }),
            ) => {
                sums[at] += cell_sums.total();
                counts[at] += rows.total() - nulls.total();
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
            (Partials::Distinct(seen), Some(State::Distinct(counts))) if group != 0 => {
                for cell in counts {
                    seen[group].extend(cell.keys().cloned());
                }
            }
            (Partials::Percentile { values, .. }, Some(State::Extent { counts, .. }))
                if group != 0 =>
            {
                for cell in counts {
                    values[group].extend(cell.iter());
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
            _ => unreachable!("Aggregate::state names the column the measure reads"),
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
                average,
            } => {
                let count: u64 = counts[lanes.clone()].iter().sum();
                let sum: i128 = sums[lanes].iter().sum();
                if count == 0 {
                    Value::Null
                } else if *average {
                    Value::Avg { sum, count }
                } else {
                    Value::Int(sum)
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
            Partials::Distinct(seen) => Value::Int(seen[group].len() as i128),
            Partials::Percentile { fraction, values } => percentile(*fraction, &mut values[group]),
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

/// The value at `fraction`'s rank among `values`, as `percentile_disc`
/// gives it. The entries are put in order only around the rank: each step
/// splits those left at their middle value and keeps the side the rank
/// falls in, so the work halves with each step.
// Benchmark: the median fare of the six taxi months, 2.2 million entries,
// takes 32 ms where sorting them took 49.
fn percentile(fraction: Fraction, values: &mut [ValueCount]) -> Value {
    let total = counted(values);
    if total == 0 {
        return Value::Null;
    }
    // From 1 up to the count of the entries left.
    let mut rank = fraction.rank(total);
    let mut left = values;
    loop {
        let middle = left.len() / 2;
        let (below, at, above) = mem::take(&mut left).select_nth_unstable_by_key(middle, value_of);
        let below_count = counted(below);
        if rank <= below_count {
            left = below;
        } else if rank <= below_count + at.count {
            return Value::Int(at.value.into());
        } else {
            rank -= below_count + at.count;
            left = above;
        }
    }
}

fn counted(entries: &[ValueCount]) -> u64 {
    let mut count = 0;
    for entry in entries {
        count += entry.count;
    }
    count
}

fn value_of(entry: &ValueCount) -> i64 {
    entry.value
}
