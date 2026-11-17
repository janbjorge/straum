//! What a count of distinct values keeps of one column: a count per
//! value, one entry per cell of a bucket. A column starts out keeping
//! numbers as a min or max does, each cell's values ascending in as few
//! bytes as they need. The first value of another kind turns it into a
//! map per cell, which holds any value. A row adds one to its value's
//! count, a removed row takes one away, and a whole cell's counts move
//! between keyed and bound cells.

use crate::column::put;
use crate::hash::FastMap;
use crate::value::Datum;
use crate::value_counts::ValueCounts;

/// The count per value of each cell of one column.
// Told by the values rather than the column's type: a load sets the
// types only after the spec has built the rollup and its states.
#[derive(Debug)]
pub(crate) enum DistinctCells {
    /// Numbers only, as [`crate::rollup::State::Extent`] keeps them.
    Numbers(Vec<ValueCounts>),
    /// Values of any kind, numbers among them.
    Values(Vec<FastMap<Datum, u64>>),
}

impl DistinctCells {
    /// How many cells the column holds.
    pub(crate) fn len(&self) -> usize {
        match self {
            DistinctCells::Numbers(counts) => counts.len(),
            DistinctCells::Values(counts) => counts.len(),
        }
    }

    /// Adds an empty entry for a new cell.
    pub(super) fn push(&mut self) {
        match self {
            DistinctCells::Numbers(counts) => put(counts, ValueCounts::EMPTY),
            DistinctCells::Values(counts) => put(counts, FastMap::default()),
        }
    }

    /// Drops the entry of `cell`, the last entry taking its place.
    pub(super) fn swap_remove(&mut self, cell: usize) {
        match self {
            DistinctCells::Numbers(counts) => drop(counts.swap_remove(cell)),
            DistinctCells::Values(counts) => drop(counts.swap_remove(cell)),
        }
    }

    /// Counts one more of `value`, which is not null.
    pub(super) fn add(&mut self, cell: usize, value: &Datum) {
        if let (DistinctCells::Numbers(counts), &Datum::Int(number)) = (&mut *self, value) {
            counts[cell].add(number, 1);
            return;
        }
        let counts = self.values();
        *counts[cell].entry(value.clone()).or_default() += 1;
    }

    /// Takes one of `value` out of `cell`; false, with nothing taken, when
    /// the cell holds none.
    pub(super) fn remove(&mut self, cell: usize, value: &Datum) -> bool {
        match (self, value) {
            (DistinctCells::Numbers(counts), &Datum::Int(number)) => counts[cell].sub(number, 1),
            // A column of numbers was never given another kind.
            (DistinctCells::Numbers(_), _) => false,
            (DistinctCells::Values(counts), _) => take(&mut counts[cell], value, 1),
        }
    }

    /// Whether `cell` counts each value of `given` of `source` as often at
    /// least.
    pub(super) fn covers(&self, cell: usize, source: &DistinctCells, given: usize) -> bool {
        match (self, source) {
            (DistinctCells::Numbers(counts), DistinctCells::Numbers(source)) => {
                counts[cell].covers(&source[given])
            }
            (DistinctCells::Values(counts), DistinctCells::Numbers(source)) => {
                for entry in source[given].iter() {
                    if count_in(&counts[cell], &Datum::Int(entry.value)) < entry.count {
                        return false;
                    }
                }
                true
            }
            (DistinctCells::Numbers(counts), DistinctCells::Values(source)) => {
                for (value, &count) in &source[given] {
                    let &Datum::Int(number) = value else {
                        return false;
                    };
                    if !counts[cell].has(number, count) {
                        return false;
                    }
                }
                true
            }
            (DistinctCells::Values(counts), DistinctCells::Values(source)) => {
                for (value, &count) in &source[given] {
                    if count_in(&counts[cell], value) < count {
                        return false;
                    }
                }
                true
            }
        }
    }

    /// Adds cell `given` of `source` into `cell`.
    pub(super) fn add_from(&mut self, cell: usize, source: &DistinctCells, given: usize) {
        match (self, source) {
            (DistinctCells::Numbers(counts), DistinctCells::Numbers(source)) => {
                counts[cell].add_counts(&source[given]);
            }
            (DistinctCells::Values(counts), DistinctCells::Numbers(source)) => {
                for entry in source[given].iter() {
                    *counts[cell].entry(Datum::Int(entry.value)).or_default() += entry.count;
                }
            }
            (cells, DistinctCells::Values(source)) => {
                let counts = cells.values();
                for (value, &count) in &source[given] {
                    *counts[cell].entry(value.clone()).or_default() += count;
                }
            }
        }
    }

    /// Takes cell `given` of `source` back out of `cell`, which
    /// [`DistinctCells::covers`] found holds it.
    pub(super) fn sub_from(&mut self, cell: usize, source: &DistinctCells, given: usize) {
        match (self, source) {
            (DistinctCells::Numbers(counts), DistinctCells::Numbers(source)) => {
                counts[cell].sub_counts(&source[given]);
            }
            (DistinctCells::Values(counts), DistinctCells::Numbers(source)) => {
                for entry in source[given].iter() {
                    let value = Datum::Int(entry.value);
                    assert!(
                        take(&mut counts[cell], &value, entry.count),
                        "covers checked"
                    );
                }
            }
            (DistinctCells::Numbers(counts), DistinctCells::Values(source)) => {
                for (value, &count) in &source[given] {
                    let &Datum::Int(number) = value else {
                        unreachable!("covers checked");
                    };
                    assert!(counts[cell].sub(number, count), "covers checked");
                }
            }
            (DistinctCells::Values(counts), DistinctCells::Values(source)) => {
                for (value, &count) in &source[given] {
                    assert!(take(&mut counts[cell], value, count), "covers checked");
                }
            }
        }
    }

    /// The maps of values, which a column of numbers turns into first.
    fn values(&mut self) -> &mut Vec<FastMap<Datum, u64>> {
        if let DistinctCells::Numbers(numbers) = self {
            let mut maps = Vec::with_capacity(numbers.len());
            for counts in numbers.iter() {
                let mut map = FastMap::default();
                for entry in counts.iter() {
                    map.insert(Datum::Int(entry.value), entry.count);
                }
                maps.push(map);
            }
            *self = DistinctCells::Values(maps);
        }
        match self {
            DistinctCells::Values(maps) => maps,
            DistinctCells::Numbers(_) => unreachable!("turned into maps above"),
        }
    }
}

fn count_in(counts: &FastMap<Datum, u64>, value: &Datum) -> u64 {
    match counts.get(value) {
        Some(&count) => count,
        None => 0,
    }
}

/// Takes `count` of `value` out of `counts`, where one is taken or
/// [`DistinctCells::covers`] found as many; false, with nothing taken,
/// when none is held.
fn take(counts: &mut FastMap<Datum, u64>, value: &Datum, count: u64) -> bool {
    let Some(held) = counts.get_mut(value) else {
        return false;
    };
    *held -= count;
    if *held == 0 {
        counts.remove(value);
    }
    true
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use crate::aggregate::{Aggregate, Partials};
    use crate::column::Counts;
    use crate::identifier::Identifier;
    use crate::test_rng::Rng;
    use crate::value::{Datum, Value};

    use super::super::state::{CellRows, State, StateKind};

    const COLUMNS: usize = 3;
    const CELLS: usize = 4;
    /// Groups an ask puts cells in, group 0 the one never finished.
    const GROUPS: usize = 3;
    /// The one column given text as well as numbers.
    const MIXED: usize = 2;
    /// Steps before the columns start over, so most hold numbers only.
    const STEPS: usize = 100;

    /// A cell's place: its column, and its place among the column's cells.
    #[derive(Debug, Clone, Copy)]
    struct Place {
        column: usize,
        cell: usize,
    }

    /// Columns of count-distinct states, and the counts a map of every
    /// value keeps per cell beside them.
    struct Followed {
        columns: Vec<State>,
        reference: Vec<Vec<BTreeMap<Datum, u64>>>,
    }

    impl Followed {
        fn new() -> Followed {
            let mut columns = Vec::new();
            for _ in 0..COLUMNS {
                let mut state = State::new(StateKind::Distinct);
                for _ in 0..CELLS {
                    state.push();
                }
                columns.push(state);
            }
            Followed {
                columns,
                reference: vec![vec![BTreeMap::new(); CELLS]; COLUMNS],
            }
        }
    }

    fn place(rng: &mut Rng) -> Place {
        Place {
            column: rng.position_below(COLUMNS),
            cell: rng.position_below(CELLS),
        }
    }

    /// A number, near enough to others to repeat; text in the mixed
    /// column now and then.
    fn value(rng: &mut Rng, column: usize) -> Datum {
        let number = i64::try_from(rng.below(40)).unwrap() - 10;
        if column == MIXED && rng.below(3) == 0 {
            return Datum::Text(number.to_string().into());
        }
        Datum::Int(number * 1_000)
    }

    fn take_from(counts: &mut BTreeMap<Datum, u64>, value: &Datum, count: u64) {
        let held = counts.get_mut(value).unwrap();
        *held -= count;
        if *held == 0 {
            counts.remove(value);
        }
    }

    fn covers(held: &BTreeMap<Datum, u64>, given: &BTreeMap<Datum, u64>) -> bool {
        for (value, count) in given {
            if held.get(value).copied().unwrap_or(0) < *count {
                return false;
            }
        }
        true
    }

    fn cell_rows(position: usize) -> CellRows {
        CellRows { position, rows: 0 }
    }

    /// Column `to` to change, and column `from` beside it.
    fn pair(columns: &mut [State], to: usize, from: usize) -> (&mut State, &State) {
        if to < from {
            let (low, high) = columns.split_at_mut(from);
            (&mut low[to], &high[0])
        } else {
            let (low, high) = columns.split_at_mut(to);
            (&mut high[0], &low[from])
        }
    }

    fn add(followed: &mut Followed, rng: &mut Rng) {
        let at = place(rng);
        let datum = value(rng, at.column);
        followed.columns[at.column].add(at.cell, &datum);
        *followed.reference[at.column][at.cell]
            .entry(datum)
            .or_default() += 1;
    }

    fn remove(followed: &mut Followed, rng: &mut Rng) {
        let at = place(rng);
        let datum = value(rng, at.column);
        let counts = &mut followed.reference[at.column][at.cell];
        let held = counts.contains_key(&datum);
        let removed = followed.columns[at.column].remove(at.cell, 1, &datum);
        assert_eq!(removed.is_ok(), held, "{datum:?} out of {at:?}");
        if held {
            take_from(counts, &datum, 1);
        }
    }

    /// Moves one cell into another, or back out when it is held there,
    /// as a keyed cell moves into a bound one.
    fn absorb(followed: &mut Followed, rng: &mut Rng) {
        let (to, from) = (place(rng), place(rng));
        if to.column == from.column {
            return;
        }
        let (state, source) = pair(&mut followed.columns, to.column, from.column);
        let given = followed.reference[from.column][from.cell].clone();
        let held = &mut followed.reference[to.column][to.cell];
        let covered = covers(held, &given);
        let found = state.covers(cell_rows(to.cell), source, cell_rows(from.cell));
        assert_eq!(found, covered, "{from:?} in {to:?}");
        if covered && rng.below(2) == 0 {
            state.sub_from(to.cell, source, from.cell);
            for (value, count) in &given {
                take_from(held, value, *count);
            }
            return;
        }
        state.add_from(to.cell, source, from.cell);
        for (value, count) in given {
            *held.entry(value).or_default() += count;
        }
    }

    /// A cell goes, the last taking its place, and a new one comes.
    fn replace(followed: &mut Followed, rng: &mut Rng) {
        let at = place(rng);
        followed.columns[at.column].swap_remove(at.cell);
        followed.columns[at.column].push();
        let cells = &mut followed.reference[at.column];
        cells.swap_remove(at.cell);
        cells.push(BTreeMap::new());
    }

    /// Asks the distinct values of each group, the cells put in groups
    /// at random, against a set of every value.
    fn ask(followed: &Followed, rng: &mut Rng) {
        let aggregate = Aggregate::CountDistinct(Identifier::new("x").unwrap());
        let mut partials = Partials::new(&aggregate);
        partials.grow(GROUPS);
        let rows = Counts::Zero(CELLS);
        let mut expected = vec![BTreeSet::new(); GROUPS];
        for column in 0..COLUMNS {
            let mut groups = Vec::with_capacity(CELLS);
            for cell in 0..CELLS {
                let group = rng.position_below(GROUPS);
                groups.push(u32::try_from(group).unwrap());
                expected[group].extend(followed.reference[column][cell].keys());
            }
            partials.merge(&groups, &rows, Some(&followed.columns[column]));
        }
        for (group, values) in expected.iter().enumerate().skip(1) {
            let count = i128::try_from(values.len()).unwrap();
            assert_eq!(partials.finish(group), Value::Int(count), "group {group}");
        }
    }

    /// Counts per number and per value, moved in and out of other cells
    /// and asked over several cells, give what a map of every value
    /// gives.
    #[test]
    fn distinct_counts_follow_a_map_of_every_value() {
        let mut rng = Rng::new(17);
        for _ in 0..200 {
            let mut followed = Followed::new();
            for _ in 0..STEPS {
                match rng.below(10) {
                    0..4 => add(&mut followed, &mut rng),
                    4..7 => remove(&mut followed, &mut rng),
                    7 | 8 => absorb(&mut followed, &mut rng),
                    _ => replace(&mut followed, &mut rng),
                }
                ask(&followed, &mut rng);
            }
        }
    }
}
