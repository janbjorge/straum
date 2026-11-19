//! The distinct values a group meets for `count(DISTINCT c)`,
//! `sum(DISTINCT c)` and `avg(DISTINCT c)`: the count per value of each
//! cell, read in place, merged in value order for several cells, and
//! [`Tally`], how many there were and what they add up to.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

use crate::hash::{FastMap, FastSet};
use crate::rollup::DistinctCells;
use crate::value::Datum;
use crate::value_counts::ValueCounts;

/// How many distinct values a group met, and their sum: integers in
/// their column's scale, so an `i128` holds any sum of `i64`s below
/// 2^64 of them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Tally {
    pub(crate) count: usize,
    pub(crate) sum: i128,
}

/// What a tally must hold: a count alone needs no sum.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Tallied {
    Count,
    CountAndSum,
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
    pub(crate) fn meet(&mut self, cells: &'s DistinctCells, cell: usize) {
        match cells {
            DistinctCells::Numbers(counts) => self.numbers.meet(&counts[cell]),
            DistinctCells::Values(counts) => self.values.meet(&counts[cell]),
        }
    }

    /// The values met, a number met both as a number and as a value
    /// counted once.
    pub(crate) fn tally(&self, tallied: Tallied) -> Tally {
        let mut tally = self.numbers.tally(tallied);
        match &self.values {
            Values::None => {}
            Values::OneCell(values) => {
                for value in values.keys() {
                    self.add_unless_a_number(&mut tally, value);
                }
            }
            Values::Cells(seen) => {
                for value in seen {
                    self.add_unless_a_number(&mut tally, value);
                }
            }
        }
        tally
    }

    /// Adds `value` to `tally` unless a cell of numbers holds it, which
    /// counted it already.
    fn add_unless_a_number(&self, tally: &mut Tally, value: &Datum) {
        if self.numbers.has(value) {
            return;
        }
        tally.count += 1;
        if let &Datum::Int(number) = value {
            tally.sum += i128::from(number);
        }
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

    fn tally(&self, tallied: Tallied) -> Tally {
        match self {
            Numbers::None => Tally::default(),
            Numbers::OneCell(counts) => {
                let mut tally = Tally {
                    count: counts.len(),
                    sum: 0,
                };
                // A count alone reads the length, not every value.
                if let Tallied::CountAndSum = tallied {
                    for entry in counts.iter() {
                        tally.sum += i128::from(entry.value);
                    }
                }
                tally
            }
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
/// merge takes the least value any cell has left, and counts and adds it
/// when it differs from the last one taken. The heap holds one value per
/// cell, never a set of all of them.
// A heap rather than a scan of every cell for the least: a group may
// meet thousands of cells, and the scan costs that many per value.
fn distinct_in_order(cells: &[&ValueCounts]) -> Tally {
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
    let mut tally = Tally::default();
    let mut last = None;
    while let Some(Reverse(head)) = heads.pop() {
        if last != Some(head.value) {
            tally.count += 1;
            tally.sum += i128::from(head.value);
            last = Some(head.value);
        }
        if let Some(entry) = entries[head.cell.0].next() {
            heads.push(Reverse(Head {
                value: entry.value,
                cell: head.cell,
            }));
        }
    }
    tally
}

/// A group that has met no value yet.
pub(crate) fn distinct_none<'s>() -> Distinct<'s> {
    Distinct {
        numbers: Numbers::None,
        values: Values::None,
    }
}
