//! What each aggregate is: the column it reads, the cell state it reads it
//! from, the scale it answers in, and how it merges cells into an answer.
//! Every match on `Agg` is here, so a new aggregate is one variant and the
//! arms the compiler asks for.

use std::collections::HashSet;

use crate::ident::Ident;
use crate::rollup::{Kind, State};
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
    /// The column the measure reads; `None` for count.
    pub fn input(&self) -> Option<&Ident> {
        self.state().map(|(_, column)| column)
    }

    /// Whether the input must hold integers: integer columns, or numeric
    /// ones as scaled integers. Other measures read values of any type.
    pub fn reads_numbers(&self) -> bool {
        self.state().is_some_and(|(kind, _)| kind.numeric())
    }

    /// The cell state the measure reads, and from which column; `None` for
    /// count, which reads the row count.
    pub(crate) fn state(&self) -> Option<(Kind, &Ident)> {
        match self {
            Agg::Count => None,
            Agg::Sum(c) | Agg::Avg(c) => Some((Kind::Sum, c)),
            Agg::Min(c) | Agg::Max(c) => Some((Kind::Extent, c)),
            Agg::CountDistinct(c) => Some((Kind::Distinct, c)),
        }
    }

    /// The scale the measure answers in, given its input's.
    pub(crate) fn scale(&self, input: u32) -> u32 {
        match self {
            Agg::Count | Agg::CountDistinct(_) => 0,
            Agg::Sum(_) | Agg::Min(_) | Agg::Max(_) | Agg::Avg(_) => input,
        }
    }
}

/// One measure merged over the cells of one group.
#[derive(Clone)]
pub(crate) enum Partial {
    Count(u64),
    Sum { sum: i128, count: u64 },
    Avg { sum: i128, count: u64 },
    Min(Option<i64>),
    Max(Option<i64>),
    Distinct(HashSet<Datum>),
}

impl Partial {
    /// The measure over no cells.
    pub(crate) fn new(agg: &Agg) -> Partial {
        match agg {
            Agg::Count => Partial::Count(0),
            Agg::Sum(_) => Partial::Sum { sum: 0, count: 0 },
            Agg::Avg(_) => Partial::Avg { sum: 0, count: 0 },
            Agg::Min(_) => Partial::Min(None),
            Agg::Max(_) => Partial::Max(None),
            Agg::CountDistinct(_) => Partial::Distinct(HashSet::new()),
        }
    }

    /// Merges a cell of `rows` rows, whose state `state` is the one
    /// [`Agg::state`] names.
    pub(crate) fn merge(&mut self, rows: u64, state: Option<&State>) {
        match (self, state) {
            (Partial::Count(count), None) => *count += rows,
            (
                Partial::Sum { sum, count } | Partial::Avg { sum, count },
                Some(State::Sum {
                    sum: cell_sum,
                    count: cell_count,
                }),
            ) => {
                *sum += cell_sum;
                *count += cell_count;
            }
            (Partial::Min(min), Some(State::Extent { counts, min: v, .. })) => {
                if !counts.is_empty() {
                    *min = Some(min.map_or(*v, |m| m.min(*v)));
                }
            }
            (Partial::Max(max), Some(State::Extent { counts, max: v, .. })) => {
                if !counts.is_empty() {
                    *max = Some(max.map_or(*v, |m| m.max(*v)));
                }
            }
            (Partial::Distinct(seen), Some(State::Distinct(counts))) => {
                seen.extend(counts.keys().cloned());
            }
            _ => unreachable!("Agg::state names the state the measure reads"),
        }
    }

    pub(crate) fn finish(self) -> Value {
        match self {
            Partial::Count(count) => Value::Int(count.into()),
            Partial::Sum { count: 0, .. }
            | Partial::Avg { count: 0, .. }
            | Partial::Min(None)
            | Partial::Max(None) => Value::Null,
            Partial::Sum { sum, .. } => Value::Int(sum),
            Partial::Avg { sum, count } => Value::Avg { sum, count },
            Partial::Min(Some(v)) | Partial::Max(Some(v)) => Value::Int(v.into()),
            Partial::Distinct(seen) => Value::Int(seen.len() as i128),
        }
    }
}
