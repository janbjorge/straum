//! The groups of an ask: each combination of the values of its `by` keys
//! the cells it reads fall in. A key's values are numbered without hashing
//! any (a key by its codes, the bucket in time order), and a group is
//! found from those numbers in a flat table, or a map when the table
//! would be too big. The table leaves out the bucket: buckets come in time
//! order, so it holds the groups of the bucket at hand, and a group made
//! before it counts as none. Each group keeps its key numbers, from which
//! the answer reads its key values, in their order.

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
    fn enter(&mut self, bucket: i64) -> Entered {
        if let Grouping::Time {
            width,
            bucket_starts,
            ..
        } = self
        {
            let start = width.bucket_start(bucket);
            if bucket_starts.last() != Some(&start) {
                bucket_starts.push(start);
                return Entered::NewTimeKey;
            }
        }
        Entered::SameTimeKey
    }

    /// Adds to each cell's place in the dense table this key's number
    /// times `stride`.
    // Benchmark: see `Lookup::code`.
    #[inline]
    fn offsets(&mut self, rollup: &Rollup, cells: &Cells, stride: usize, at: &mut [usize]) {
        match self {
            // The table leaves out the bucket.
            Grouping::Time { .. } => {}
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

    fn value(&self, rollup: &Rollup, number: u32) -> Datum {
        match self {
            Grouping::Lookup { lookup, .. } => lookup.value(rollup, number).clone(),
            Grouping::Time { bucket_starts, .. } => Datum::Int(bucket_starts[number as usize]),
        }
    }

    /// Per number of the key, the place of its value among those `met`
    /// has. A bucket's number is its place already.
    fn ranks(&self, rollup: &Rollup, met: &[u32]) -> Ranks {
        let lookup = match self {
            Grouping::Lookup { lookup, .. } => lookup,
            Grouping::Time { bucket_starts, .. } => {
                let count = u32::try_from(bucket_starts.len()).expect("codes are u32");
                let mut by_number = Vec::with_capacity(bucket_starts.len());
                for number in 0..count {
                    by_number.push(number);
                }
                return Ranks { by_number, count };
            }
        };
        let mut sorted = met.to_vec();
        sorted.sort_unstable_by(|left, right| {
            lookup
                .value(rollup, *left)
                .cmp(lookup.value(rollup, *right))
        });
        // Sized by the codes met, not by every code the key has: an ask
        // may meet a few of millions.
        let mut most = 0;
        for &number in met {
            most = most.max(number as usize);
        }
        let mut by_number = vec![0; most + 1];
        // Codes of equal values each take a place: their groups end up
        // side by side all the same.
        let mut place = 0;
        for &number in &sorted {
            by_number[number as usize] = place;
            place += 1;
        }
        Ranks {
            by_number,
            count: place,
        }
    }
}

/// One by key's numbers ranked by their values.
struct Ranks {
    /// Per number, the place of its value; numbers no group has are 0.
    by_number: Vec<u32>,
    /// Places in all.
    count: u32,
}

/// Bits a group number takes in a packed sort key.
const GROUP_BITS: u32 = 32;

/// Bits that hold every place below `count`.
fn bits_for(count: u32) -> u32 {
    count.saturating_sub(1).bit_width()
}

/// Whether a bucket starts a new time-key bucket.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Entered {
    NewTimeKey,
    SameTimeKey,
}

/// The number of the time-key bucket at hand.
fn current(bucket_starts: &[i64]) -> u32 {
    u32::try_from(bucket_starts.len() - 1).expect("codes are u32")
}

/// The groups an ask has met, numbered from 1 in the order it met them.
pub(super) struct Groups {
    index: GroupIndex,
    /// Per by key, what its number is multiplied by in the dense table;
    /// 0 for the bucket, which it leaves out.
    strides: Vec<usize>,
    /// Groups made before the time-key bucket at hand: a dense entry no
    /// greater is a group of an earlier bucket, so none of this one.
    before: u32,
    /// Each group's by-key numbers, `by.len()` a group, group 1's first.
    made_numbers: Vec<u32>,
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

impl Groups {
    /// No groups yet, for the by keys `by`.
    pub(super) fn new(by: &[Grouping<'_>]) -> Groups {
        let mut strides = Vec::with_capacity(by.len());
        let mut size = Some(1_usize);
        // Past an overflow the table is sparse, which reads no stride.
        for grouping in by {
            if let Some(so_far) = size {
                match grouping {
                    Grouping::Time { .. } => strides.push(0),
                    Grouping::Lookup { .. } => {
                        strides.push(so_far);
                        size = so_far.checked_mul(grouping.bound());
                    }
                }
            }
        }
        let index = match size {
            Some(size) if size <= DENSE => GroupIndex::Dense(vec![0; size]),
            _ => GroupIndex::Sparse(FastMap::default()),
        };
        Groups {
            index,
            strides,
            before: 0,
            made_numbers: Vec::new(),
            made: 0,
            numbers: Vec::with_capacity(by.len()),
            at: Vec::new(),
        }
    }

    /// Groups so far, counting group 0.
    pub(super) fn len(&self) -> usize {
        self.made + 1
    }

    /// Moves each by key to the bucket that starts at `bucket`.
    pub(super) fn enter(&mut self, by: &mut [Grouping<'_>], bucket: i64) {
        for grouping in by {
            if grouping.enter(bucket) == Entered::NewTimeKey {
                self.before = u32::try_from(self.made).expect("fewer groups than cells");
            }
        }
    }

    /// Replaces each nonzero entry of `groups`, one per cell, with the
    /// cell's group. Without `by`, every nonzero entry is group 1 already.
    pub(super) fn assign(
        &mut self,
        by: &mut [Grouping<'_>],
        rollup: &Rollup,
        cells: &Cells,
        groups: &mut [u32],
    ) {
        if by.is_empty() {
            return;
        }
        // Known groups in one tight pass, the rest after: few cells start
        // a group.
        let new = match &self.index {
            GroupIndex::Dense(table) => {
                let places = Places {
                    strides: &self.strides,
                    at: &mut self.at,
                    before: self.before,
                };
                assign_known(table, places, by, rollup, cells, groups)
            }
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
                GroupIndex::Dense(table) if table[self.at[cell]] > self.before => {
                    table[self.at[cell]]
                }
                _ => self.make(by, rollup, cells, cell),
            };
        }
    }

    /// The group of `cell`, made if new.
    fn make(
        &mut self,
        by: &mut [Grouping<'_>],
        rollup: &Rollup,
        cells: &Cells,
        cell: usize,
    ) -> u32 {
        self.numbers.clear();
        for grouping in by.iter_mut() {
            self.numbers.push(grouping.number(rollup, cells, cell));
        }
        self.insert()
    }

    /// The group `numbers` names, made if new.
    pub(super) fn insert(&mut self) -> u32 {
        let next = u32::try_from(self.len()).expect("fewer groups than cells");
        let group = match &mut self.index {
            GroupIndex::Dense(table) => {
                let mut at = 0;
                for (&number, stride) in self.numbers.iter().zip(&self.strides) {
                    at += number as usize * stride;
                }
                if table[at] <= self.before {
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
            self.made_numbers.extend_from_slice(&self.numbers);
            self.made += 1;
        }
        group
    }

    /// Group `group`'s key values.
    pub(super) fn key_values(
        &self,
        by: &[Grouping<'_>],
        rollup: &Rollup,
        group: u32,
    ) -> Vec<Datum> {
        let start = (group as usize - 1) * by.len();
        let numbers = &self.made_numbers[start..start + by.len()];
        let mut values = Vec::with_capacity(by.len());
        for (grouping, &number) in by.iter().zip(numbers) {
            values.push(grouping.value(rollup, number));
        }
        values
    }

    /// Every group made, in the order of its key values. Each key's
    /// numbers are ranked once, so groups compare as integers, packed in
    /// one where they fit.
    // Benchmark: `bench/taxi.py`'s ask by hour and zone over six months,
    // 509,200 groups, answers in 80 ms where sorting its rows by their key
    // values took 118.
    pub(super) fn in_key_order(&self, by: &[Grouping<'_>], rollup: &Rollup) -> Vec<u32> {
        let made = u32::try_from(self.made).expect("fewer groups than cells");
        if by.is_empty() {
            // The one group of an ask without GROUP BY.
            return (1..=made).collect();
        }
        let mut ranks = Vec::with_capacity(by.len());
        for (position, grouping) in by.iter().enumerate() {
            ranks.push(grouping.ranks(rollup, &self.met(by.len(), position)));
        }
        // Per group, its key's places, `by.len()` a group, group 1's first.
        let mut places = Vec::with_capacity(self.made_numbers.len());
        for (index, &number) in self.made_numbers.iter().enumerate() {
            places.push(ranks[index % by.len()].by_number[number as usize]);
        }
        let mut widths = Vec::with_capacity(ranks.len());
        for key in &ranks {
            widths.push(bits_for(key.count));
        }
        match packed_order(&places, &widths, made) {
            Some(order) => order,
            None => in_place_order(&places, by.len(), made),
        }
    }

    /// The numbers the groups have for by key `position`, each once.
    fn met(&self, key_count: usize, position: usize) -> Vec<u32> {
        let mut met = Vec::new();
        let mut seen = Vec::new();
        for numbers in self.made_numbers.chunks_exact(key_count) {
            let number = numbers[position] as usize;
            if number >= seen.len() {
                seen.resize(number + 1, false);
            }
            if !seen[number] {
                seen[number] = true;
                met.push(numbers[position]);
            }
        }
        met
    }
}

/// Groups 1 to `made` ordered by their key places, `widths.len()` a
/// group: each place in its key's width of bits, then the group, packed
/// in one integer. None when they don't fit.
fn packed_order(places: &[u32], widths: &[u32], made: u32) -> Option<Vec<u32>> {
    let mut bits = GROUP_BITS;
    for width in widths {
        bits += width;
    }
    if bits > u128::BITS {
        return None;
    }
    let mut packed = Vec::with_capacity(made as usize);
    for (index, group_places) in places.chunks_exact(widths.len().max(1)).enumerate() {
        let mut key: u128 = 0;
        for (&place, &width) in group_places.iter().zip(widths) {
            key = (key << width) | u128::from(place);
        }
        let group = u32::try_from(index + 1).expect("fewer groups than cells");
        packed.push((key << GROUP_BITS) | u128::from(group));
    }
    packed.sort_unstable();
    let mut order = Vec::with_capacity(packed.len());
    for key in packed {
        order.push(u32::try_from(key & u128::from(u32::MAX)).expect("masked to u32"));
    }
    Some(order)
}

/// Groups 1 to `made` ordered by their key places, `key_count` a group,
/// for keys too many to pack in one integer.
fn in_place_order(places: &[u32], key_count: usize, made: u32) -> Vec<u32> {
    let mut order = Vec::with_capacity(made as usize);
    for group in 1..=made {
        order.push(group);
    }
    // Ties go by group, as in the packed order.
    order.sort_unstable_by(|&left, &right| {
        let left_places = group_places(places, key_count, left);
        let right_places = group_places(places, key_count, right);
        left_places.cmp(right_places).then(left.cmp(&right))
    });
    order
}

fn group_places(places: &[u32], key_count: usize, group: u32) -> &[u32] {
    let start = (group as usize - 1) * key_count;
    &places[start..start + key_count]
}

/// Where each cell of a bucket lies in the dense group table: its key's
/// numbers times their strides.
struct Places<'g> {
    // Borrowed from `Groups`, beside its table.
    strides: &'g [usize],
    at: &'g mut Vec<usize>,
    /// As [`Groups::before`].
    before: u32,
}

/// With a dense table: the group of each cell whose group is known,
/// [`NEW`] for the rest; whether any is new.
fn assign_known(
    table: &[u32],
    places: Places<'_>,
    by: &mut [Grouping<'_>],
    rollup: &Rollup,
    cells: &Cells,
    groups: &mut [u32],
) -> bool {
    let Places {
        strides,
        at,
        before,
    } = places;
    at.clear();
    at.resize(groups.len(), 0);
    for (grouping, &stride) in by.iter_mut().zip(strides) {
        grouping.offsets(rollup, cells, stride, at);
    }
    // Selects, not branches: under a filter, whether a cell passed is
    // as likely as not. `at` is in the table for every cell.
    let mut new = false;
    for (group, &place) in groups.iter_mut().zip(at.iter()) {
        let known = table[place];
        let unknown = known <= before;
        new |= unknown && *group != 0;
        let found = if unknown { NEW } else { known };
        *group = if *group != 0 { found } else { 0 };
    }
    new
}

#[cfg(test)]
mod tests {
    use super::{in_place_order, packed_order};

    /// Groups 1 to `made` sorted by their places, then by number.
    fn reference(places: &[u32], key_count: usize, made: u32) -> Vec<u32> {
        let mut keyed = Vec::new();
        for group in 1..=made {
            let start = (group as usize - 1) * key_count;
            keyed.push((places[start..start + key_count].to_vec(), group));
        }
        keyed.sort();
        let mut order = Vec::new();
        for (_, group) in keyed {
            order.push(group);
        }
        order
    }

    /// The next of a fixed sequence of numbers below `below`.
    fn next(state: &mut u64, below: u32) -> u32 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        u32::try_from((*state >> 33) % u64::from(below)).unwrap()
    }

    #[test]
    fn both_orders_sort_groups_by_their_places() {
        let mut state = 7;
        for widths in [vec![1], vec![3, 2], vec![2, 1, 3], vec![32, 32, 32]] {
            let made = 200;
            let mut places = Vec::new();
            for _ in 0..made {
                for &width in &widths {
                    // Small places, so that groups tie on some keys.
                    places.push(next(&mut state, 1 << width.min(3)));
                }
            }
            let wanted = reference(&places, widths.len(), made);
            assert_eq!(packed_order(&places, &widths, made), Some(wanted.clone()));
            assert_eq!(in_place_order(&places, widths.len(), made), wanted);
        }
    }

    #[test]
    fn places_too_wide_to_pack_are_left_to_the_slower_order() {
        let places = [0, 0, 0, 1];
        assert_eq!(packed_order(&places, &[32, 32, 32, 1], 1), None);
        assert_eq!(in_place_order(&places, 4, 1), [1]);
    }
}
