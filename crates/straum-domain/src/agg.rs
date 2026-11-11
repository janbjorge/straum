//! Every match on `Agg` is here, so a new aggregate is one variant and the
//! arms the compiler asks for.

use std::collections::HashSet;

use crate::ident::Ident;
use crate::rollup::{Column, Kind};
use crate::value::{Datum, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Agg {
    Count,
    Sum(Ident),
    Min(Ident),
    Max(Ident),
    Avg(Ident),
    CountDistinct(Ident),
}

impl Agg {
    pub fn input(&self) -> Option<&Ident> {
        self.state().map(|(_, column)| column)
    }

    pub fn reads_numbers(&self) -> bool {
        self.state().is_some_and(|(kind, _)| kind.numeric())
    }

    pub(crate) fn state(&self) -> Option<(Kind, &Ident)> {
        match self {
            Agg::Count => None,
            Agg::Sum(c) | Agg::Avg(c) => Some((Kind::Sum, c)),
            Agg::Min(c) | Agg::Max(c) => Some((Kind::Extent, c)),
            Agg::CountDistinct(c) => Some((Kind::Distinct, c)),
        }
    }
}

/// Copies of each group's partial, cell `i` merging into copy `i % LANES`.
/// Most cells of a bucket can fall in one group, and adds to one place in
/// memory each wait on the last; spread over copies, they overlap.
const LANES: usize = 4;

fn lane(i: usize, g: u32) -> usize {
    g as usize * LANES + i % LANES
}

/// One measure merged over cells, per group: entries `g * LANES..` are
/// group `g`'s. Group 0 takes the cells an ask passes over, so merging needs
/// no test per cell; [`Partials::finish`] never reads it.
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
}

impl Partials {
    pub(crate) fn new(agg: &Agg) -> Partials {
        match agg {
            Agg::Count => Partials::Count(Vec::new()),
            Agg::Sum(_) | Agg::Avg(_) => Partials::Sum {
                sums: Vec::new(),
                counts: Vec::new(),
                average: matches!(agg, Agg::Avg(_)),
            },
            Agg::Min(_) => Partials::Min {
                values: Vec::new(),
                seen: Vec::new(),
            },
            Agg::Max(_) => Partials::Max {
                values: Vec::new(),
                seen: Vec::new(),
            },
            Agg::CountDistinct(_) => Partials::Distinct(Vec::new()),
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
        }
    }

    pub(crate) fn merge(&mut self, groups: &[u32], rows: &[u64], column: Option<&Column>) {
        if let Some(&g) = groups.first()
            && groups.iter().all(|&h| h == g)
        {
            return self.merge_into(g, rows, column);
        }
        let cells = groups.iter().enumerate().map(|(i, &g)| lane(i, g));
        match (self, column) {
            (Partials::Count(counts), None) => {
                for (g, &n) in cells.zip(rows) {
                    counts[g] += n;
                }
            }
            (
                Partials::Sum { sums, counts, .. },
                Some(Column::Sum {
                    sums: cell_sums,
                    counts: cell_counts,
                }),
            ) => {
                for (g, (&sum, &count)) in cells.zip(cell_sums.iter().zip(cell_counts)) {
                    sums[g] += sum;
                    counts[g] += count;
                }
            }
            (Partials::Min { values, seen }, Some(Column::Extent { mins, maxs, .. })) => {
                for (g, (&min, &max)) in cells.zip(mins.iter().zip(maxs)) {
                    values[g] = values[g].min(min);
                    seen[g] |= min <= max;
                }
            }
            (Partials::Max { values, seen }, Some(Column::Extent { mins, maxs, .. })) => {
                for (g, (&min, &max)) in cells.zip(mins.iter().zip(maxs)) {
                    values[g] = values[g].max(max);
                    seen[g] |= min <= max;
                }
            }
            (Partials::Distinct(seen), Some(Column::Distinct(counts))) => {
                // Unlike a sum, a value set costs its group 0 merge.
                for (&g, counts) in groups.iter().zip(counts).filter(|&(&g, _)| g != 0) {
                    seen[g as usize].extend(counts.keys().cloned());
                }
            }
            _ => unreachable!("Agg::state names the column the measure reads"),
        }
    }

    /// One cell into one group, for a cell that counts more than once or
    /// under several groups.
    pub(crate) fn merge_cell(&mut self, g: u32, i: usize, rows: &[u64], column: Option<&Column>) {
        let at = lane(i, g);
        match (self, column) {
            (Partials::Count(counts), None) => counts[at] += rows[i],
            (
                Partials::Sum { sums, counts, .. },
                Some(Column::Sum {
                    sums: cell_sums,
                    counts: cell_counts,
                }),
            ) => {
                sums[at] += cell_sums[i];
                counts[at] += cell_counts[i];
            }
            (Partials::Min { values, seen }, Some(Column::Extent { mins, maxs, .. })) => {
                values[at] = values[at].min(mins[i]);
                seen[at] |= mins[i] <= maxs[i];
            }
            (Partials::Max { values, seen }, Some(Column::Extent { mins, maxs, .. })) => {
                values[at] = values[at].max(maxs[i]);
                seen[at] |= mins[i] <= maxs[i];
            }
            (Partials::Distinct(seen), Some(Column::Distinct(counts))) if g != 0 => {
                seen[g as usize].extend(counts[i].keys().cloned());
            }
            (Partials::Distinct(_), Some(Column::Distinct(_))) => {}
            _ => unreachable!("Agg::state names the column the measure reads"),
        }
    }

    /// Folding in registers beats adding to one place in memory, each add
    /// waiting on the last.
    fn merge_into(&mut self, g: u32, rows: &[u64], column: Option<&Column>) {
        let (at, g) = (lane(0, g), g as usize);
        match (self, column) {
            (Partials::Count(counts), None) => counts[at] += rows.iter().sum::<u64>(),
            (
                Partials::Sum { sums, counts, .. },
                Some(Column::Sum {
                    sums: cell_sums,
                    counts: cell_counts,
                }),
            ) => {
                sums[at] += cell_sums.iter().sum::<i128>();
                counts[at] += cell_counts.iter().sum::<u64>();
            }
            (Partials::Min { values, seen }, Some(Column::Extent { mins, maxs, .. })) => {
                values[at] = mins.iter().fold(values[at], |a, &b| a.min(b));
                seen[at] |= mins.iter().zip(maxs).any(|(a, b)| a <= b);
            }
            (Partials::Max { values, seen }, Some(Column::Extent { mins, maxs, .. })) => {
                values[at] = maxs.iter().fold(values[at], |a, &b| a.max(b));
                seen[at] |= mins.iter().zip(maxs).any(|(a, b)| a <= b);
            }
            (Partials::Distinct(seen), Some(Column::Distinct(counts))) if g != 0 => {
                seen[g].extend(counts.iter().flat_map(|c| c.keys().cloned()));
            }
            (Partials::Distinct(_), Some(Column::Distinct(_))) => {}
            _ => unreachable!("Agg::state names the column the measure reads"),
        }
    }

    pub(crate) fn finish(&self, g: usize) -> Value {
        let lanes = g * LANES..(g + 1) * LANES;
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
                Value::Int(values[lanes].iter().fold(i64::MAX, |a, &b| a.min(b)).into())
            }
            Partials::Max { values, .. } => {
                Value::Int(values[lanes].iter().fold(i64::MIN, |a, &b| a.max(b)).into())
            }
            Partials::Distinct(seen) => Value::Int(seen[g].len() as i128),
        }
    }
}
