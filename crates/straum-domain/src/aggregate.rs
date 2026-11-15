//! Every match on `Aggregate` is here, so a new aggregate is one variant and the
//! arms the compiler asks for.

use std::collections::HashSet;

use crate::column::Counts;
use crate::identifier::Identifier;
use crate::rollup::{State, StateKind};
use crate::value::{Datum, Value};
use crate::value_counts::ValueCounts;

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
        (units <= one).then_some(Fraction { units, places })
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
    pub fn input(&self) -> Option<&Identifier> {
        self.state().map(|(_, column)| column)
    }

    pub fn reads(&self) -> Option<Reads> {
        self.state().map(|(kind, _)| kind.reads())
    }

    pub(crate) fn state(&self) -> Option<(StateKind, &Identifier)> {
        match self {
            Aggregate::Count => None,
            Aggregate::CountNotNull(column) => Some((StateKind::Count, column)),
            Aggregate::Sum(column) | Aggregate::Avg(column) => Some((StateKind::Sum, column)),
            Aggregate::Min(column)
            | Aggregate::Max(column)
            | Aggregate::PercentileDisc { column, .. } => Some((StateKind::Extent, column)),
            Aggregate::CountDistinct(column) => Some((StateKind::Distinct, column)),
            Aggregate::BoolAnd(column) | Aggregate::BoolOr(column) => {
                Some((StateKind::Bools, column))
            }
        }
    }
}

/// Copies of each group's partial, cell `cell` merging into copy
/// `cell % LANES`. Most cells of a bucket can fall in one group, and adds
/// to one place in memory each wait on the last; spread over copies, they
/// overlap.
const LANES: usize = 4;

fn lane(cell: usize, group: u32) -> usize {
    group as usize * LANES + cell % LANES
}

fn cell_lanes(groups: &[u32]) -> impl Iterator<Item = usize> + '_ {
    groups
        .iter()
        .enumerate()
        .map(|(cell, &group)| lane(cell, group))
}

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
    /// Each group's values with their counts, one pair per cell a value is
    /// in, sorted only when finished.
    Percentile {
        fraction: Fraction,
        values: Vec<Vec<(i64, u64)>>,
    },
    /// A `bool_and` and a `bool_or` merge alike and part only when finished.
    Bools {
        trues: Vec<u64>,
        falses: Vec<u64>,
        all: bool,
    },
}

impl Partials {
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

    /// `rows` are the cells' rows, which a count is and which a column's
    /// nulls are taken from.
    pub(crate) fn merge(&mut self, groups: &[u32], rows: &Counts, state: Option<&State>) {
        if let Some(&first) = groups.first()
            && groups.iter().all(|&group| group == first)
        {
            return self.merge_into(first, rows, state);
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
                cell_sums.each(rows, groups, |cell, group, sum, count| {
                    let at = lane(cell, group);
                    sums[at] += sum;
                    counts[at] += count;
                });
                take_counts(counts, groups, nulls);
            }
            (Partials::Min { values, seen }, Some(State::Extent { mins, maxs, .. })) => {
                for (at, (&min, &max)) in cell_lanes(groups).zip(mins.iter().zip(maxs)) {
                    values[at] = values[at].min(min);
                    seen[at] |= min <= max;
                }
            }
            (Partials::Max { values, seen }, Some(State::Extent { mins, maxs, .. })) => {
                for (at, (&min, &max)) in cell_lanes(groups).zip(mins.iter().zip(maxs)) {
                    values[at] = values[at].max(max);
                    seen[at] |= min <= max;
                }
            }
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

    /// Folding in registers beats adding to one place in memory, each add
    /// waiting on the last.
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
                values[at] = mins.iter().fold(values[at], |least, &min| least.min(min));
                seen[at] |= mins.iter().zip(maxs).any(|(min, max)| min <= max);
            }
            (Partials::Max { values, seen }, Some(State::Extent { mins, maxs, .. })) => {
                values[at] = maxs.iter().fold(values[at], |most, &max| most.max(max));
                seen[at] |= mins.iter().zip(maxs).any(|(min, max)| min <= max);
            }
            (Partials::Distinct(seen), Some(State::Distinct(counts))) if group != 0 => {
                seen[group].extend(counts.iter().flat_map(|cell| cell.keys().cloned()));
            }
            (Partials::Percentile { values, .. }, Some(State::Extent { counts, .. }))
                if group != 0 =>
            {
                values[group].extend(counts.iter().flat_map(ValueCounts::iter));
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
            Partials::Min { values, .. } => Value::Int(
                values[lanes]
                    .iter()
                    .fold(i64::MAX, |least, &value| least.min(value))
                    .into(),
            ),
            Partials::Max { values, .. } => Value::Int(
                values[lanes]
                    .iter()
                    .fold(i64::MIN, |most, &value| most.max(value))
                    .into(),
            ),
            Partials::Distinct(seen) => Value::Int(seen[group].len() as i128),
            Partials::Percentile { fraction, values } => {
                let values = &mut values[group];
                values.sort_unstable_by_key(|&(value, _)| value);
                let total: u64 = values.iter().map(|&(_, count)| count).sum();
                if total == 0 {
                    return Value::Null;
                }
                let rank = fraction.rank(total);
                let mut seen = 0;
                for &(value, count) in values.iter() {
                    seen += count;
                    if seen >= rank {
                        return Value::Int(value.into());
                    }
                }
                unreachable!("rank is at most the values' count")
            }
            Partials::Bools { trues, falses, all } => {
                let any_true = trues[lanes.clone()].iter().any(|&count| count > 0);
                let any_false = falses[lanes].iter().any(|&count| count > 0);
                match (any_true, any_false) {
                    (false, false) => Value::Null,
                    _ if *all => Value::Bool(!any_false),
                    _ => Value::Bool(any_true),
                }
            }
        }
    }
}
