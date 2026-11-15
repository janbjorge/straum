//! The cells of one bucket, side by side. A cell is one distinct rollup
//! key: its key codes, its row count and one entry in each cell state.
//! A row finds its cell through the index and adds to its entries; a
//! whole cell's entries move in or out of a bound cell at once.

use crate::column::{Counts, grow};
use crate::index::{Index, Keys};

use super::state::{CellRows, State};
use super::{InputIndex, PartIndex};

/// The cells of one bucket, side by side: cell `c` has key
/// `keys[c * key_parts..][..key_parts]`, `rows[c]` rows and entry `c` of
/// each state, so an ask reads each measure's states as one flat run.
#[derive(Debug)]
pub(crate) struct Cells {
    /// Codes in one cell's key: the rollup key's parts.
    pub(super) key_parts: usize,
    pub(super) index: Index,
    pub(super) keys: Vec<u32>,
    pub(super) rows: Counts,
    /// One per input, in [`super::Rollup`] input order.
    pub(super) states: Vec<State>,
}

/// Where the cell of a key lies in its bucket, and whether it was made
/// just now.
#[derive(Debug, Clone, Copy)]
pub(super) struct CellEntry {
    pub(super) position: usize,
    pub(super) new: bool,
}

impl Cells {
    /// No cells yet, with a state per input of these kinds.
    pub(super) fn new(key_parts: usize, states: Vec<State>) -> Cells {
        Cells {
            key_parts,
            index: Index::default(),
            keys: Vec::new(),
            rows: Counts::Zero(0),
            states,
        }
    }

    fn keys(&self) -> Keys<'_> {
        Keys {
            codes: &self.keys,
            len: self.key_parts,
        }
    }

    /// The position of the cell with `key`.
    pub(super) fn find(&self, key: &[u32]) -> Option<usize> {
        self.index.get(key, self.keys())
    }

    /// How many cells the bucket holds.
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// The key codes of `cell`.
    pub(crate) fn key(&self, cell: usize) -> &[u32] {
        &self.keys[cell * self.key_parts..][..self.key_parts]
    }

    /// The code of one part of `cell`'s key.
    pub(crate) fn code(&self, cell: usize, part: PartIndex) -> u32 {
        self.keys[cell * self.key_parts + part.0]
    }

    /// Each cell's row count.
    pub(crate) fn rows(&self) -> &Counts {
        &self.rows
    }

    /// The states of one input column.
    pub(crate) fn state(&self, input: InputIndex) -> &State {
        &self.states[input.0]
    }

    /// The states of every input column, in the rollup's input order.
    pub(crate) fn states(&self) -> &[State] {
        &self.states
    }

    fn same_kinds(&self, source: &[State]) -> bool {
        if self.states.len() != source.len() {
            return false;
        }
        for (destination, source) in self.states.iter().zip(source) {
            if !destination.same_kind(source) {
                return false;
            }
        }
        true
    }

    /// Folds cell `source_cell` of `source`, which holds `count` rows,
    /// into `cell`. False when the columns differ.
    pub(super) fn absorb(
        &mut self,
        cell: usize,
        count: u64,
        source: &[State],
        source_cell: usize,
    ) -> bool {
        if !self.same_kinds(source) {
            return false;
        }
        self.rows.add(cell, count);
        for (destination, source) in self.states.iter_mut().zip(source) {
            destination.add_from(cell, source, source_cell);
        }
        true
    }

    /// Takes cell `source_cell` of `source`, which holds `count` rows,
    /// back out of `cell`. False when it was never there, and the cell is
    /// left as it was.
    pub(super) fn release(
        &mut self,
        cell: usize,
        count: u64,
        source: &[State],
        source_cell: usize,
    ) -> bool {
        let rows = self.rows.get(cell);
        if rows < count || source.len() != self.states.len() {
            return false;
        }
        let held = CellRows {
            position: cell,
            rows,
        };
        let given = CellRows {
            position: source_cell,
            rows: count,
        };
        for (source, destination) in source.iter().zip(&self.states) {
            if !destination.covers(held, source, given) {
                return false;
            }
        }
        self.rows.sub(cell, count);
        for (destination, source) in self.states.iter_mut().zip(source) {
            destination.sub_from(cell, source, source_cell);
        }
        true
    }

    /// The position of the cell with `key`, made empty if it's new, and
    /// whether it is.
    pub(super) fn entry(&mut self, key: &[u32]) -> CellEntry {
        if let Some(position) = self.find(key) {
            return CellEntry {
                position,
                new: false,
            };
        }
        let cell = self.len();
        grow(&mut self.keys, self.key_parts);
        self.keys.extend_from_slice(key);
        self.rows.push();
        for state in &mut self.states {
            state.push();
        }
        let keys = Keys {
            codes: &self.keys,
            len: self.key_parts,
        };
        self.index.insert(cell, keys);
        CellEntry {
            position: cell,
            new: true,
        }
    }

    /// Drops `cell`, the last cell taking its place.
    pub(super) fn swap_remove(&mut self, cell: usize) {
        let last = self.len() - 1;
        let key_parts = self.key_parts;
        let keys = Keys {
            codes: &self.keys,
            len: key_parts,
        };
        self.index.remove(cell, keys);
        if cell != last {
            self.index.moved(last, cell, keys);
            self.keys
                .copy_within(last * key_parts..(last + 1) * key_parts, cell * key_parts);
        }
        self.keys.truncate(last * key_parts);
        self.rows.swap_remove(cell);
        for state in &mut self.states {
            state.swap_remove(cell);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::sealed::SealedColumn;
    use crate::value::Datum;

    use super::super::state::StateKind;
    use super::*;

    /// One cell of key 7, whose count and sum read `values`.
    fn cell_of(values: &[Datum]) -> Cells {
        let states = vec![State::new(StateKind::Count), State::new(StateKind::Sum)];
        let mut cells = Cells::new(1, states);
        let entry = cells.entry(&[7]);
        assert!(entry.new && entry.position == 0);
        for value in values {
            cells.rows.add(entry.position, 1);
            for state in &mut cells.states {
                state.add(entry.position, value);
            }
        }
        cells
    }

    /// The cell's rows and its count column, as a spill keeps them.
    #[derive(Debug, PartialEq)]
    struct Held {
        rows: u64,
        count: SealedColumn,
    }

    fn held(cells: &Cells) -> Held {
        Held {
            rows: cells.rows.get(0),
            count: cells.states[0].sealed(&cells.rows),
        }
    }

    /// A cell gives back only a cell it holds: as many rows, as many of
    /// them null in each column a count or a sum reads, and as many not.
    /// A store never asks for more, so this is asked here.
    #[test]
    fn a_cell_gives_back_only_what_it_holds() {
        let kept = [Datum::Int(4), Datum::Int(9), Datum::Null];
        for (others, gives) in [
            // More nulls than the one it holds, and more values than two.
            (&[Datum::Null, Datum::Null][..], false),
            (&[Datum::Int(4), Datum::Int(9), Datum::Int(1)], false),
            (&[Datum::Int(4), Datum::Null, Datum::Null], false),
            (&[Datum::Int(9), Datum::Null], true),
            (&[Datum::Int(4)], true),
            (&kept, true),
        ] {
            let mut cells = cell_of(&kept);
            let other = cell_of(others);
            let before = held(&cells);
            let count = others.len() as u64;
            assert_eq!(
                cells.release(0, count, &other.states, 0),
                gives,
                "{others:?}"
            );
            let left = if gives {
                before.rows - count
            } else {
                before.rows
            };
            assert_eq!(cells.rows.get(0), left, "{others:?}");
            // What it gave it takes in again, and is as it was either way.
            assert!(!gives || cells.absorb(0, count, &other.states, 0));
            assert_eq!(held(&cells), before, "{others:?}");
        }
    }
}
