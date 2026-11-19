//! The values a group gathers for a percentile or a mode: the count per
//! value of each cell an ask meets, read in place for one cell and copied
//! out for several. `Gathered::answer` picks the value a `Pick` names.

use std::cmp::Ordering;
use std::mem;

use crate::aggregate::Fraction;
use crate::value::Value;
use crate::value_counts::{ValueCount, ValueCounts};

/// What a group's gathered values answer.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Pick {
    /// `percentile_disc(fraction)`.
    Rank(Fraction),
    /// `mode()`: the value counted most, the least of those tied.
    Mode,
}

/// The values one group has gathered for a percentile or a mode, with
/// their counts.
pub(crate) enum Gathered<'s> {
    None,
    /// The counts of the one cell met, read in place and in order.
    OneCell(&'s ValueCounts),
    /// One entry per cell a value is in, in no order.
    Cells(Vec<ValueCount>),
}

impl<'s> Gathered<'s> {
    /// Gathers the counts of one more cell.
    pub(crate) fn meet(&mut self, counts: &'s ValueCounts) {
        match self {
            Gathered::None => *self = Gathered::OneCell(counts),
            Gathered::OneCell(first) => {
                let mut values: Vec<ValueCount> = first.iter().collect();
                values.extend(counts.iter());
                *self = Gathered::Cells(values);
            }
            Gathered::Cells(values) => values.extend(counts.iter()),
        }
    }

    /// The value `pick` names; null with no value gathered.
    pub(crate) fn answer(&mut self, pick: Pick) -> Value {
        match (self, pick) {
            (Gathered::None, _) => Value::Null,
            (Gathered::OneCell(counts), Pick::Rank(fraction)) => {
                percentile_in_order(fraction, counts)
            }
            (Gathered::Cells(values), Pick::Rank(fraction)) => percentile(fraction, values),
            (Gathered::OneCell(counts), Pick::Mode) => mode_in_order(counts.iter()),
            (Gathered::Cells(values), Pick::Mode) => {
                values.sort_unstable_by_key(value_of);
                mode_in_order(values.iter().copied())
            }
        }
    }
}

/// A group that has gathered no value yet.
pub(crate) fn gathered_none<'s>() -> Gathered<'s> {
    Gathered::None
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

/// As [`percentile`], over one cell's counts, which come in value order:
/// the value whose running count first reaches the rank.
fn percentile_in_order(fraction: Fraction, counts: &ValueCounts) -> Value {
    let mut total = 0;
    for entry in counts.iter() {
        total += entry.count;
    }
    if total == 0 {
        return Value::Null;
    }
    let rank = fraction.rank(total);
    let mut reached = 0;
    for entry in counts.iter() {
        reached += entry.count;
        if reached >= rank {
            return Value::Int(entry.value.into());
        }
    }
    unreachable!("the rank is at most the count of the entries")
}

/// The value counted most among `entries`, which come in value order, so
/// the first of those tied is the least, as PostgreSQL's `mode()` takes
/// it. A value met in several cells comes once from each, side by side.
fn mode_in_order(entries: impl Iterator<Item = ValueCount>) -> Value {
    let mut best: Option<ValueCount> = None;
    let mut run: Option<ValueCount> = None;
    for entry in entries {
        run = match run {
            Some(current) if current.value == entry.value => Some(ValueCount {
                value: entry.value,
                count: current.count + entry.count,
            }),
            Some(current) => {
                best = Some(more_counted(best, current));
                Some(entry)
            }
            None => Some(entry),
        };
    }
    if let Some(current) = run {
        best = Some(more_counted(best, current));
    }
    match best {
        Some(entry) => Value::Int(entry.value.into()),
        None => Value::Null,
    }
}

/// `candidate` where it is counted more than `best`; a tie keeps `best`,
/// the lesser value.
fn more_counted(best: Option<ValueCount>, candidate: ValueCount) -> ValueCount {
    match best {
        Some(kept) if kept.count.cmp(&candidate.count) != Ordering::Less => kept,
        _ => candidate,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_rng::Rng;

    /// One cell's counts read in value order give the value a split of
    /// the same entries in no order does, at every fraction.
    #[test]
    fn a_cell_read_in_order_gives_the_percentile_of_its_entries() {
        let mut rng = Rng::new(7);
        for _ in 0..500 {
            let mut entries = Vec::new();
            let mut value = i64::try_from(rng.below(100)).unwrap() - 50;
            for _ in 0..rng.below(20) {
                value += 1 + i64::try_from(rng.below(5)).unwrap();
                entries.push(ValueCount {
                    value,
                    count: 1 + rng.below(4),
                });
            }
            let counts = ValueCounts::from_ascending(&entries);
            let mut shuffled = entries.clone();
            shuffled.reverse();
            for units in [0, 1, 25, 50, 75, 99, 100] {
                let fraction = Fraction::new(units, 2).unwrap();
                assert_eq!(
                    percentile_in_order(fraction, &counts),
                    percentile(fraction, &mut shuffled),
                    "{entries:?} at {units}"
                );
            }
        }
    }

    /// The value counted most, the least of those tied, by a count of
    /// its own over the entries.
    fn mode_by_counting(entries: &[ValueCount]) -> Value {
        let mut totals: Vec<ValueCount> = Vec::new();
        for entry in entries {
            match totals.iter_mut().find(|total| total.value == entry.value) {
                Some(total) => total.count += entry.count,
                None => totals.push(*entry),
            }
        }
        let mut best: Option<ValueCount> = None;
        for total in totals {
            let wins = match best {
                None => true,
                Some(kept) => {
                    total.count > kept.count
                        || (total.count == kept.count && total.value < kept.value)
                }
            };
            if wins {
                best = Some(total);
            }
        }
        match best {
            Some(entry) => Value::Int(entry.value.into()),
            None => Value::Null,
        }
    }

    /// One cell's counts in order and several cells' copied out in no
    /// order give the mode a count of the entries gives, ties to the
    /// least value.
    #[test]
    fn a_mode_is_the_value_counted_most() {
        let mut rng = Rng::new(9);
        for _ in 0..2_000 {
            let mut cells = Vec::new();
            for _ in 0..=rng.below(4) {
                let mut entries = Vec::new();
                let mut value = i64::try_from(rng.below(10)).unwrap() - 5;
                for _ in 0..rng.below(8) {
                    value += 1 + i64::try_from(rng.below(3)).unwrap();
                    entries.push(ValueCount {
                        value,
                        count: 1 + rng.below(3),
                    });
                }
                cells.push(ValueCounts::from_ascending(&entries));
            }
            let mut all = Vec::new();
            let mut gathered = gathered_none();
            for cell in &cells {
                all.extend(cell.iter());
                gathered.meet(cell);
            }
            let expected = mode_by_counting(&all);
            assert_eq!(gathered.answer(Pick::Mode), expected, "{all:?}");
            assert_eq!(
                mode_in_order(cells[0].iter()),
                mode_by_counting(&cells[0].iter().collect::<Vec<_>>())
            );
        }
        let tied = [
            ValueCount { value: 7, count: 2 },
            ValueCount { value: 3, count: 2 },
        ];
        let mut values = tied.to_vec();
        assert_eq!(
            Gathered::Cells(values.clone()).answer(Pick::Mode),
            Value::Int(3)
        );
        values.clear();
        assert_eq!(Gathered::Cells(values).answer(Pick::Mode), Value::Null);
    }
}
