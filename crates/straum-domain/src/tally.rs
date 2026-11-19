//! The distinct values a group meets for `count(DISTINCT c)`,
//! `sum(DISTINCT c)` and `avg(DISTINCT c)`: the count per value of each
//! cell, read in place, merged in value order for several cells, and
//! [`Tally`], how many there were and what they add up to.

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
            Numbers::Cells(cells) => distinct_of_cells(cells),
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

/// Bits a value set may spend per value counted in its cells before it
/// sorts the values instead: at 64 a set of bits takes no more memory
/// than a list of the values would.
const BITS_PER_ENTRY: u128 = 64;

/// The distinct values of several cells. Most columns hold values close
/// together, a fare a few thousand cents from the next, so a set of bits
/// from the least value to the greatest marks each value met; the set's
/// ones are then counted and added. Values too far apart for that are
/// listed, sorted and counted once each.
// Benchmark: the distinct fares of the six taxi months for one payment
// type, 1.4 million entries in 6,398 cells, took 96 ms merged in order
// through a heap.
fn distinct_of_cells(cells: &[&ValueCounts]) -> Tally {
    let mut entries = 0;
    let mut least = i64::MAX;
    let mut greatest = i64::MIN;
    for counts in cells {
        let ends = counts.ends();
        least = least.min(ends.least);
        greatest = greatest.max(ends.greatest);
        entries += counts.len();
    }
    if entries == 0 {
        return Tally::default();
    }
    let span = u128::from(greatest.abs_diff(least)) + 1;
    if span <= entries as u128 * BITS_PER_ENTRY {
        return distinct_by_bits(cells, least, span);
    }
    distinct_by_sorting(cells, entries)
}

/// Bits in one word of a value set.
const WORD_BITS: usize = u64::BITS as usize;

/// [`distinct_of_cells`] over values that lie within `span` of `least`.
fn distinct_by_bits(cells: &[&ValueCounts], least: i64, span: u128) -> Tally {
    let words = usize::try_from(span.div_ceil(WORD_BITS as u128)).expect("span is bounded");
    let mut bits = vec![0_u64; words];
    for counts in cells {
        for entry in counts.iter() {
            let offset = usize::try_from(entry.value.abs_diff(least)).expect("within the span");
            bits[offset / WORD_BITS] |= 1 << (offset % WORD_BITS);
        }
    }
    let mut tally = Tally::default();
    for (word, &set) in bits.iter().enumerate() {
        let mut left = set;
        while left != 0 {
            let offset = word * WORD_BITS + left.trailing_zeros() as usize;
            tally.count += 1;
            tally.sum += i128::from(least) + offset as i128;
            left &= left - 1;
        }
    }
    tally
}

/// [`distinct_of_cells`] over values too far apart for a set of bits.
fn distinct_by_sorting(cells: &[&ValueCounts], entries: usize) -> Tally {
    let mut values = Vec::with_capacity(entries);
    for counts in cells {
        for entry in counts.iter() {
            values.push(entry.value);
        }
    }
    values.sort_unstable();
    values.dedup();
    let mut tally = Tally {
        count: values.len(),
        sum: 0,
    };
    for value in values {
        tally.sum += i128::from(value);
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
