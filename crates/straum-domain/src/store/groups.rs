//! The groups of an ask: each combination of the values of its `by` keys
//! the cells it reads fall in. A key's values are numbered without hashing
//! any (a key by its codes, the bucket in time order), and a group is
//! found from those numbers in a flat table, or a map when the table
//! would be too big. Each group keeps its key values for the answer.

use std::borrow::Cow;

use crate::hash::FastMap;
use crate::rollup::{Cells, Rollup};
use crate::spec::Width;
use crate::value::{ColumnType, Datum};

use super::bound::JoinRow;
use super::lookup::Lookup;

/// A cell's group while [`Groups::assign`] has yet to look it up. Groups
/// number fewer than the cells, which fit in memory.
const NEW: u32 = u32::MAX;

/// Most entries a dense group table may have: 4 MiB of `u32`, of which an
/// ask touches only the pages its groups fall on.
const DENSE: usize = 1 << 20;

/// A by key of an ask, its values numbered without hashing any: a key's by
/// its codes, a time key's in time order.
// The `'s` in this file borrows the store for the length of an ask, as
// in `lookup.rs`.
pub(super) enum Grouping<'s> {
    Lookup {
        lookup: Lookup<'s>,
        bound: usize,
    },
    Time {
        width: Width,
        /// The start of each time-key bucket met so far, the one at hand last.
        bucket_starts: Vec<i64>,
        /// The buckets the ask covers, the most it can meet.
        bound: usize,
    },
}

impl<'s> Grouping<'s> {
    /// A key read through `lookup`.
    pub(super) fn lookup(lookup: Lookup<'s>, rollup: &Rollup) -> Grouping<'s> {
        Grouping::Lookup {
            bound: lookup.bound(rollup),
            lookup,
        }
    }

    /// The bucket key, of `width`, over an ask that covers `buckets`.
    pub(super) fn time(width: Width, buckets: usize) -> Grouping<'s> {
        Grouping::Time {
            width,
            bucket_starts: Vec::new(),
            bound: buckets,
        }
    }

    /// Numbers the key's values have.
    fn bound(&self) -> usize {
        match self {
            Grouping::Lookup { bound, .. } | Grouping::Time { bound, .. } => *bound,
        }
    }

    /// The type of the key's values, as the load set it.
    pub(super) fn column_type(&self, rollup: &Rollup) -> ColumnType {
        match self {
            Grouping::Lookup { lookup, .. } => lookup.column_type(rollup),
            Grouping::Time { .. } => rollup.time_type(),
        }
    }

    /// Buckets come in time order, so those of one time-key bucket come
    /// together.
    pub(super) fn enter(&mut self, bucket: i64) {
        if let Grouping::Time {
            width,
            bucket_starts,
            ..
        } = self
        {
            let start = width.bucket_start(bucket);
            if bucket_starts.last() != Some(&start) {
                bucket_starts.push(start);
            }
        }
    }

    /// Adds to each cell's place in the dense table this key's number
    /// times `stride`.
    // Benchmark: see `Lookup::code`.
    #[inline]
    fn offsets(&mut self, rollup: &Rollup, cells: &Cells, stride: usize, at: &mut [usize]) {
        match self {
            Grouping::Time { bucket_starts, .. } => {
                let offset = (bucket_starts.len() - 1) * stride;
                for place in at.iter_mut() {
                    *place += offset;
                }
            }
            // Cells passed over too, as a branch costs more than their
            // entries, which nothing reads.
            Grouping::Lookup { lookup, .. } => {
                for (cell, place) in at.iter_mut().enumerate() {
                    *place += lookup.code(rollup, cells, cell) as usize * stride;
                }
            }
        }
    }

    fn number(&mut self, rollup: &Rollup, cells: &Cells, cell: usize) -> u32 {
        match self {
            Grouping::Lookup { lookup, .. } => lookup.code(rollup, cells, cell),
            Grouping::Time { bucket_starts, .. } => current(bucket_starts),
        }
    }

    /// The key's number for `cell` under one join row.
    pub(super) fn number_in(&self, cells: &Cells, cell: usize, row: &JoinRow<'_>) -> u32 {
        match self {
            Grouping::Lookup { lookup, .. } => lookup.code_in(cells, cell, row),
            Grouping::Time { bucket_starts, .. } => current(bucket_starts),
        }
    }

    // `'r` is the shorter of the rollup's borrow and the store's, as in
    // `Lookup::value`; a borrowed value spares a copy per group.
    fn value<'r>(&self, rollup: &'r Rollup, number: u32) -> Cow<'r, Datum>
    where
        's: 'r,
    {
        match self {
            Grouping::Lookup { lookup, .. } => Cow::Borrowed(lookup.value(rollup, number)),
            Grouping::Time { bucket_starts, .. } => {
                Cow::Owned(Datum::Int(bucket_starts[number as usize]))
            }
        }
    }
}

/// The number of the time-key bucket at hand.
fn current(bucket_starts: &[i64]) -> u32 {
    u32::try_from(bucket_starts.len() - 1).expect("codes are u32")
}

/// The groups an ask has met, numbered from 1 in the order it met them.
// `'s` borrows the store: a group's key values are borrowed from the
// rollup or a projection where they can be.
pub(super) struct Groups<'s> {
    index: GroupIndex,
    /// Per by key, what its number is multiplied by in the dense table.
    strides: Vec<usize>,
    /// Each group's values, `by.len()` a group, group 1's first.
    pub(super) keys: Vec<Cow<'s, Datum>>,
    /// Groups so far, not counting group 0.
    pub(super) made: usize,
    /// The by-key numbers of the group at hand.
    pub(super) numbers: Vec<u32>,
    /// Per cell of a bucket, its place in the dense table.
    at: Vec<usize>,
}

/// Where a combination of by-key numbers finds its group.
enum GroupIndex {
    /// Group per combination of by-key numbers, 0 for none yet.
    Dense(Vec<u32>),
    Sparse(FastMap<Box<[u32]>, u32>),
}

impl<'s> Groups<'s> {
    /// No groups yet, for the by keys `by`.
    pub(super) fn new(by: &[Grouping<'_>]) -> Groups<'s> {
        let mut strides = Vec::with_capacity(by.len());
        let mut size = Some(1_usize);
        for grouping in by {
            let Some(so_far) = size else {
                break;
            };
            strides.push(so_far);
            size = so_far.checked_mul(grouping.bound());
        }
        let index = match size {
            Some(size) if size <= DENSE => GroupIndex::Dense(vec![0; size]),
            _ => GroupIndex::Sparse(FastMap::default()),
        };
        Groups {
            index,
            strides,
            keys: Vec::new(),
            made: 0,
            numbers: Vec::with_capacity(by.len()),
            at: Vec::new(),
        }
    }

    /// Groups so far, counting group 0.
    pub(super) fn len(&self) -> usize {
        self.made + 1
    }

    /// Replaces each nonzero entry of `groups`, one per cell, with the
    /// cell's group. Without `by`, every nonzero entry is group 1 already.
    pub(super) fn assign(
        &mut self,
        by: &mut [Grouping<'s>],
        rollup: &'s Rollup,
        cells: &Cells,
        groups: &mut [u32],
    ) {
        if by.is_empty() {
            return;
        }
        // Known groups in one tight pass, the rest after: few cells start
        // a group.
        let new = match &self.index {
            GroupIndex::Dense(_) => self.assign_known(by, rollup, cells, groups),
            GroupIndex::Sparse(_) => {
                for group in groups.iter_mut() {
                    if *group != 0 {
                        *group = NEW;
                    }
                }
                true
            }
        };
        if !new {
            return;
        }
        for (cell, group) in groups.iter_mut().enumerate() {
            if *group != NEW {
                continue;
            }
            // A cell before it in the bucket may have made its group.
            *group = match &self.index {
                GroupIndex::Dense(table) if table[self.at[cell]] != 0 => table[self.at[cell]],
                _ => self.make(by, rollup, cells, cell),
            };
        }
    }

    /// With a dense table: the group of each cell whose group is known,
    /// [`NEW`] for the rest; whether any is new.
    fn assign_known(
        &mut self,
        by: &mut [Grouping<'s>],
        rollup: &Rollup,
        cells: &Cells,
        groups: &mut [u32],
    ) -> bool {
        let GroupIndex::Dense(table) = &self.index else {
            unreachable!("called with a dense table");
        };
        self.at.clear();
        self.at.resize(groups.len(), 0);
        for (grouping, &stride) in by.iter_mut().zip(&self.strides) {
            grouping.offsets(rollup, cells, stride, &mut self.at);
        }
        // Selects, not branches: under a filter, whether a cell passed is
        // as likely as not. `at` is in the table for every cell.
        let mut new = false;
        for (group, &at) in groups.iter_mut().zip(&self.at) {
            let known = table[at];
            new |= known == 0 && *group != 0;
            let found = if known == 0 { NEW } else { known };
            *group = if *group != 0 { found } else { 0 };
        }
        new
    }

    /// The group of `cell`, made if new.
    fn make(
        &mut self,
        by: &mut [Grouping<'s>],
        rollup: &'s Rollup,
        cells: &Cells,
        cell: usize,
    ) -> u32 {
        self.numbers.clear();
        for grouping in by.iter_mut() {
            self.numbers.push(grouping.number(rollup, cells, cell));
        }
        self.insert(by, rollup)
    }

    /// The group `numbers` names, made if new.
    pub(super) fn insert(&mut self, by: &[Grouping<'s>], rollup: &'s Rollup) -> u32 {
        let next = u32::try_from(self.len()).expect("fewer groups than cells");
        let group = match &mut self.index {
            GroupIndex::Dense(table) => {
                let mut at = 0;
                for (&number, stride) in self.numbers.iter().zip(&self.strides) {
                    at += number as usize * stride;
                }
                if table[at] == 0 {
                    table[at] = next;
                }
                table[at]
            }
            // Looked up before inserting, so only a new group allocates.
            GroupIndex::Sparse(map) => {
                if let Some(&group) = map.get(self.numbers.as_slice()) {
                    group
                } else {
                    map.insert(self.numbers.as_slice().into(), next);
                    next
                }
            }
        };
        if group == next {
            for (&number, grouping) in self.numbers.iter().zip(by) {
                self.keys.push(grouping.value(rollup, number));
            }
            self.made += 1;
        }
        group
    }
}
