//! One cell's count per value, which a min, max or percentile keeps so
//! that a removed end reveals the next one. A row adds one to its value's
//! count here and a removed row takes one away; an ask reads the least and
//! greatest value, or every value for a percentile. Values repeat little
//! inside a cell, so these counts are most of what a view with a min or
//! max holds, and a cell keeps each in the fewest bytes its values need.

mod records;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, btree_map};
use std::mem;

use records::{
    Placement, Records, Search, Taken, Widths, byte, count_width, key_of, put, record, runs_into,
    settled, signed_width, take, write,
};

/// Bytes of records a cell holds in itself, where a run's pointer would
/// be. Nearly two thirds of the taxi cells hold no more.
const INLINE: usize = 28;

/// Ahead of a cell's own records: how many, a value's width, a count's.
const CELL_HEADER: usize = 3;

/// Ahead of a run's records: a value's width, a count's, how many records
/// in two bytes, and the base in eight.
const RUN_HEADER: usize = 12;

/// Most bytes in one run. Adding a value moves half the run on average,
/// which at this many costs about what finding the run among others does.
const RUN_MAX_BYTES: usize = 4 << 10;

/// One value a min or max column holds, and how many rows hold it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct ValueCount {
    pub(crate) value: i64,
    pub(crate) count: u64,
}

/// One cell's count per value, ascending by value. A record is a value
/// less its run's base, in as many bytes as the widest of the run takes,
/// then the value's count less one likewise: three fares seen once take
/// two bytes each, where a pair of integers took 16. The widths are fixed
/// within a run, so a value is found by halving, and a value or a count
/// too wide for its run has the run encoded anew.
#[derive(Debug)]
pub(crate) enum ValueCounts {
    /// Few values, in the cell itself behind [`CELL_HEADER`], their base
    /// zero.
    Inline([u8; CELL_HEADER + INLINE]),
    /// [`RUN_HEADER`], then the records.
    Run(Vec<u8>),
    /// Runs of at most [`RUN_MAX_BYTES`]. A run holds the values from its
    /// key up to the next run's, and the first those below every key too.
    Runs(BTreeMap<i64, Vec<u8>>),
}

/// The least and greatest value a cell holds: `i64::MAX` and `i64::MIN`
/// when it holds none, which any min or max passes over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ends {
    pub(crate) least: i64,
    pub(crate) greatest: i64,
}

// A column of these is a third of what a min or max holds beside its runs.
const _: () = assert!(mem::size_of::<ValueCounts>() == 32);

impl ValueCounts {
    /// A cell that counts no value.
    pub(crate) const EMPTY: ValueCounts = ValueCounts::Inline({
        let mut cell = [0; CELL_HEADER + INLINE];
        cell[1] = 1;
        cell
    });

    /// The counts of `entries`, which ascend by value, each counted once
    /// at least.
    pub(crate) fn from_ascending(entries: &[ValueCount]) -> ValueCounts {
        let (Some(least), Some(greatest)) = (entries.first(), entries.last()) else {
            return ValueCounts::EMPTY;
        };
        let widths = Widths {
            value: signed_width(least.value).max(signed_width(greatest.value)),
            count: count_width(entries),
        };
        let size = widths.size();
        if entries.len() * size <= INLINE {
            let mut cell = [0; CELL_HEADER + INLINE];
            cell[..CELL_HEADER].copy_from_slice(&[
                byte(entries.len()),
                byte(widths.value),
                byte(widths.count),
            ]);
            for (position, entry) in entries.iter().enumerate() {
                let at = CELL_HEADER + position * size;
                record(&mut cell, at, widths, entry.value, entry.count);
            }
            return ValueCounts::Inline(cell);
        }
        let mut runs = BTreeMap::new();
        runs_into(entries, &mut runs);
        if runs.len() > 1 {
            return ValueCounts::Runs(runs);
        }
        let (_, run) = runs.pop_first().expect("entries make a run");
        ValueCounts::Run(run)
    }

    /// Counts `count` more of `value`, `count` above zero.
    pub(crate) fn add(&mut self, value: i64, count: u64) {
        match self {
            ValueCounts::Inline(cell) => {
                if let Some(moved) = add_in_cell(cell, value, count) {
                    *self = moved;
                }
            }
            ValueCounts::Run(run) => {
                if !put(run, value, count) {
                    *self = ValueCounts::from_ascending(&Records::of(run).with(value, count));
                }
            }
            ValueCounts::Runs(runs) => {
                let key = key_of(runs, value);
                if !put(runs.get_mut(&key).expect("its key"), value, count) {
                    let run = runs.remove(&key).expect("its key");
                    runs_into(&Records::of(&run).with(value, count), runs);
                }
            }
        }
    }

    /// Takes `count` of `value` back out. False, with nothing taken, when
    /// fewer are held.
    pub(crate) fn sub(&mut self, value: i64, count: u64) -> bool {
        match self {
            ValueCounts::Inline(cell) => match sub_in_cell(cell, value, count) {
                Taken::TooFew => return false,
                Taken::Some => {}
                // Empty, the cell forgets the widths its values had.
                Taken::Last => *self = ValueCounts::EMPTY,
            },
            ValueCounts::Run(run) => match take(run, value, count) {
                Taken::TooFew => return false,
                Taken::Some => {}
                Taken::Last => {
                    if let Some(moved) = settled(run) {
                        *self = moved;
                    }
                }
            },
            ValueCounts::Runs(runs) => {
                let key = key_of(runs, value);
                let run = runs.get_mut(&key).expect("its key");
                let taken = take(run, value, count);
                if taken == Taken::TooFew {
                    return false;
                }
                if run.len() == RUN_HEADER {
                    runs.remove(&key);
                }
                // One run stands without a tree.
                if taken == Taken::Last && runs.len() == 1 {
                    let (_, mut run) = runs.pop_first().expect("a run is left");
                    *self = match settled(&mut run) {
                        Some(moved) => moved,
                        None => ValueCounts::Run(run),
                    };
                }
            }
        }
        true
    }

    /// Counts every value of `other` here too.
    pub(crate) fn add_counts(&mut self, other: &ValueCounts) {
        for entry in other.iter() {
            self.add(entry.value, entry.count);
        }
    }

    /// Takes out what [`ValueCounts::covers`] found held.
    pub(crate) fn sub_counts(&mut self, other: &ValueCounts) {
        for entry in other.iter() {
            let held = self.sub(entry.value, entry.count);
            assert!(held, "covers checked");
        }
    }

    /// Whether this cell holds each value of `other` at least as often.
    pub(crate) fn covers(&self, other: &ValueCounts) -> bool {
        for entry in other.iter() {
            if !self.has(entry.value, entry.count) {
                return false;
            }
        }
        true
    }

    fn has(&self, value: i64, count: u64) -> bool {
        let records = match self {
            ValueCounts::Runs(runs) => Records::of(&runs[&key_of(runs, value)]),
            ValueCounts::Inline(cell) => Records::in_cell(cell),
            ValueCounts::Run(run) => Records::of(run),
        };
        let Search::Found(record) = records.find(value) else {
            return false;
        };
        records.count(record) >= count
    }

    /// Each value and its count, ascending by value.
    pub(crate) fn iter(&self) -> Entries<'_> {
        let (records, runs) = match self {
            ValueCounts::Inline(cell) => (Some(Records::in_cell(cell)), None),
            ValueCounts::Run(run) => (Some(Records::of(run)), None),
            ValueCounts::Runs(runs) => (None, Some(runs.values())),
        };
        Entries {
            records,
            next_record: 0,
            runs,
        }
    }

    /// The least and greatest value held.
    pub(crate) fn ends(&self) -> Ends {
        let (first, last) = match self {
            ValueCounts::Inline(cell) => (Records::in_cell(cell), Records::in_cell(cell)),
            ValueCounts::Run(run) => (Records::of(run), Records::of(run)),
            ValueCounts::Runs(runs) => {
                let first = runs.values().next().expect("a tree holds runs");
                let last = runs.values().next_back().expect("a tree holds runs");
                (Records::of(first), Records::of(last))
            }
        };
        Ends {
            least: least_in(first),
            greatest: greatest_in(last),
        }
    }
}

/// Each value and its count of one cell, ascending by value: the records
/// at hand, then those of each run left.
// Borrows the cell, so an ask reads its values in place, without a copy.
pub(crate) struct Entries<'cell> {
    records: Option<Records<'cell>>,
    next_record: usize,
    runs: Option<btree_map::Values<'cell, i64, Vec<u8>>>,
}

impl Iterator for Entries<'_> {
    type Item = ValueCount;

    fn next(&mut self) -> Option<ValueCount> {
        loop {
            if let Some(records) = self.records
                && self.next_record < records.record_count
            {
                self.next_record += 1;
                return Some(records.entry(self.next_record - 1));
            }
            let run = self.runs.as_mut()?.next()?;
            self.records = Some(Records::of(run));
            self.next_record = 0;
        }
    }
}

/// Counts `count` more of `value` among a cell's own records: the counts
/// the cell moves to when they no longer fit it as it is.
fn add_in_cell(
    cell: &mut [u8; CELL_HEADER + INLINE],
    value: i64,
    count: u64,
) -> Option<ValueCounts> {
    let held = Records::in_cell(cell);
    let (size, widths) = (held.size(), held.widths);
    let end = held.at(held.record_count);
    match held.place(value, count) {
        Placement::Counted {
            record: place,
            new_count,
        } => {
            let count_end = held.at(place + 1);
            write(cell, count_end, widths.count, new_count - 1);
            None
        }
        Placement::Inserted { record: place } if end + size <= cell.len() => {
            let at = held.at(place);
            cell.copy_within(at..end, at + size);
            record(cell, at, widths, value, count);
            cell[0] += 1;
            None
        }
        // Too many or too wide for the cell as it is: encoded anew they
        // may fit it yet.
        Placement::Inserted { .. } | Placement::TooWide => {
            let mut few = held.few();
            match held.find(value) {
                Search::Found(place) => few.entries[place].count += count,
                Search::Absent(place) => {
                    few.entries.copy_within(place..few.length, place + 1);
                    few.entries[place] = ValueCount { value, count };
                    few.length += 1;
                }
            }
            Some(ValueCounts::from_ascending(few.as_slice()))
        }
    }
}

/// Takes `count` of `value` out of a cell's own records.
/// [`Taken::Last`] only when the cell held nothing else, so it is empty.
fn sub_in_cell(cell: &mut [u8; CELL_HEADER + INLINE], value: i64, count: u64) -> Taken {
    let held = Records::in_cell(cell);
    let (size, end) = (held.size(), held.at(held.record_count));
    let Search::Found(place) = held.find(value) else {
        return Taken::TooFew;
    };
    let held_count = held.count(place);
    if held_count < count {
        return Taken::TooFew;
    }
    let (at, count_width, record_count) = (held.at(place), held.widths.count, held.record_count);
    if held_count > count {
        write(cell, at + size, count_width, held_count - count - 1);
        return Taken::Some;
    }
    if record_count > 1 {
        cell.copy_within(at + size..end, at);
        cell[0] -= 1;
        return Taken::Some;
    }
    Taken::Last
}

fn least_in(records: Records<'_>) -> i64 {
    if records.record_count == 0 {
        return i64::MAX;
    }
    records.value(0)
}

fn greatest_in(records: Records<'_>) -> i64 {
    if records.record_count == 0 {
        return i64::MIN;
    }
    records.value(records.record_count - 1)
}
