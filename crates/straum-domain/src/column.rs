//! The numbers of a bucket's cells, one per cell, side by side: each
//! cell's row count, null count, true and false counts and sums. A row
//! added raises its cell's entries; an ask reads a whole column in one
//! pass. A column keeps its numbers in as few bytes apiece as its widest
//! takes: an hour's row counts fit two bytes and its sums four, where the
//! widest a count or a sum can be takes eight and sixteen. A number its
//! column can't hold moves the column to the next width that does.

use std::ops::{Add, RangeInclusive};

use crate::wide::U256;

// The functions below are generic over the type of a column's entries
// (`u8` up to `i128`): one body serves every byte width, where a copy per
// width would be four or five functions alike but for a type.

/// Room in a column for one more cell's `each` entries. A `Vec` doubles
/// when full, which leaves a column a third empty on average; an eighth
/// more leaves it a sixteenth.
pub(crate) fn grow<Entry>(column: &mut Vec<Entry>, each: usize) {
    if column.len() + each > column.capacity() {
        column.reserve_exact((column.len() / 8).max(4 * each));
    }
}

/// A new cell's entry in a column.
pub(crate) fn put<Entry>(column: &mut Vec<Entry>, entry: Entry) {
    grow(column, 1);
    column.push(entry);
}

/// The entry of `cell` set to `number`, unless the column's type doesn't
/// hold it.
fn set<Entry: TryFrom<Wide>, Wide>(column: &mut [Entry], cell: usize, number: Wide) -> bool {
    let Ok(entry) = Entry::try_from(number) else {
        return false;
    };
    column[cell] = entry;
    true
}

/// The entry of `cell` raised by `number`, unless the column's type
/// doesn't hold the sum. `Wide` holds any entry and the sum.
fn raise<Entry, Wide>(column: &mut [Entry], cell: usize, number: Wide) -> bool
where
    Entry: Copy + TryFrom<Wide>,
    Wide: From<Entry> + Add<Output = Wide>,
{
    let entry = &mut column[cell];
    let Ok(sum) = Entry::try_from(Wide::from(*entry) + number) else {
        return false;
    };
    *entry = sum;
    true
}

/// The entry of `cell` lowered by `number`, which is at most the entry.
fn lower<Entry: Copy + TryFrom<u64>>(column: &mut [Entry], cell: usize, number: u64)
where
    u64: From<Entry>,
{
    let entry = &mut column[cell];
    *entry = (Entry::try_from(u64::from(*entry) - number).ok()).expect("less than it was");
}

/// `length` numbers a wider column held, in a type that holds them all.
// An iterator in, so a column is copied straight into its new type with
// no list of wide numbers between.
fn held<Entry: TryFrom<Wide>, Wide>(
    numbers: impl Iterator<Item = Wide>,
    length: usize,
) -> Vec<Entry> {
    let mut column = Vec::with_capacity(length);
    for number in numbers {
        let entry = Entry::try_from(number).ok();
        column.push(entry.expect("the type was picked to hold them"));
    }
    column
}

/// The sum of a column's entries.
fn total_of<Entry: Copy, Wide: From<Entry> + Add<Output = Wide>>(
    column: &[Entry],
    zero: Wide,
) -> Wide {
    let mut total = zero;
    for &entry in column {
        total = total + Wide::from(entry);
    }
    total
}

// `visit` is the caller's step per cell. Taking it as a closure puts the
// caller's sum inside the loop over the column's own type, so an ask
// reads each column in one tight pass, with no list of cells between.

/// Calls `visit` with each cell's place, its entry of `beside` and its
/// entry of `column`.
fn walk<Entry: Copy, Wide: From<Entry>, Beside: Copy>(
    column: &[Entry],
    beside: &[Beside],
    mut visit: impl FnMut(usize, Beside, Wide),
) {
    for (cell, (&entry, &beside_entry)) in column.iter().zip(beside).enumerate() {
        visit(cell, beside_entry, entry.into());
    }
}

/// As [`walk`], with each cell's entry of a column of counts too: one pass
/// where reading the two apart would take the cells' places twice.
fn walk_both<Entry: Copy, Wide: From<Entry>, Count: Copy, Beside: Copy>(
    column: &[Entry],
    counts: &[Count],
    beside: &[Beside],
    mut visit: impl FnMut(usize, Beside, Wide, u64),
) where
    u64: From<Count>,
{
    for (cell, ((&entry, &count), &beside_entry)) in
        column.iter().zip(counts).zip(beside).enumerate()
    {
        visit(cell, beside_entry, entry.into(), count.into());
    }
}

/// A column walked with a column of counts beside it.
fn walk_with<Entry: Copy, Wide: From<Entry>, Beside: Copy>(
    column: &[Entry],
    counts: &Counts,
    beside: &[Beside],
    mut visit: impl FnMut(usize, Beside, Wide, u64),
) {
    match counts {
        Counts::Zero(_) => {
            for (cell, (&entry, &beside_entry)) in column.iter().zip(beside).enumerate() {
                visit(cell, beside_entry, entry.into(), 0);
            }
        }
        Counts::U8(counts) => walk_both(column, counts, beside, visit),
        Counts::U16(counts) => walk_both(column, counts, beside, visit),
        Counts::U32(counts) => walk_both(column, counts, beside, visit),
        Counts::U64(counts) => walk_both(column, counts, beside, visit),
    }
}

/// Counts, one a cell: of its rows, or of the nulls or the trues among
/// them. None is kept while every count is zero, as the nulls of most
/// columns are.
#[derive(Debug)]
pub(crate) enum Counts {
    /// Every count is zero; this many cells.
    Zero(usize),
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    U64(Vec<u64>),
}

impl Counts {
    /// `length` counts in the narrowest type that holds `most`.
    fn holding(most: u64, length: usize, counts: impl Iterator<Item = u64>) -> Counts {
        match most {
            0 => Counts::Zero(length),
            1..=0xff => Counts::U8(held(counts, length)),
            0x100..=0xffff => Counts::U16(held(counts, length)),
            0x1_0000..=0xffff_ffff => Counts::U32(held(counts, length)),
            _ => Counts::U64(held(counts, length)),
        }
    }

    /// Cells counted.
    pub(crate) fn len(&self) -> usize {
        match self {
            Counts::Zero(length) => *length,
            Counts::U8(column) => column.len(),
            Counts::U16(column) => column.len(),
            Counts::U32(column) => column.len(),
            Counts::U64(column) => column.len(),
        }
    }

    /// The count of `cell`.
    pub(crate) fn get(&self, cell: usize) -> u64 {
        match self {
            Counts::Zero(_) => 0,
            Counts::U8(column) => column[cell].into(),
            Counts::U16(column) => column[cell].into(),
            Counts::U32(column) => column[cell].into(),
            Counts::U64(column) => column[cell],
        }
    }

    /// Sets the count of `cell`, widening the column if it must.
    pub(crate) fn set(&mut self, cell: usize, number: u64) {
        let set = match self {
            Counts::Zero(_) => number == 0,
            Counts::U8(column) => set(column, cell, number),
            Counts::U16(column) => set(column, cell, number),
            Counts::U32(column) => set(column, cell, number),
            Counts::U64(column) => set(column, cell, number),
        };
        if !set {
            *self = Counts::holding(number, self.len(), self.iter());
            self.set(cell, number);
        }
    }

    /// Raises the count of `cell` by `number`, widening the column if it
    /// must.
    pub(crate) fn add(&mut self, cell: usize, number: u64) {
        let raised = match self {
            Counts::Zero(_) => number == 0,
            Counts::U8(column) => raise(column, cell, number),
            Counts::U16(column) => raise(column, cell, number),
            Counts::U32(column) => raise(column, cell, number),
            Counts::U64(column) => raise(column, cell, number),
        };
        if !raised {
            self.set(cell, self.get(cell) + number);
        }
    }

    /// Lowers the count of `cell` by `number`, which is at most the count.
    pub(crate) fn sub(&mut self, cell: usize, number: u64) {
        match self {
            Counts::Zero(_) => assert_eq!(number, 0, "more than the count"),
            Counts::U8(column) => lower(column, cell, number),
            Counts::U16(column) => lower(column, cell, number),
            Counts::U32(column) => lower(column, cell, number),
            Counts::U64(column) => lower(column, cell, number),
        }
    }

    /// A new cell's count, zero.
    pub(crate) fn push(&mut self) {
        match self {
            Counts::Zero(length) => *length += 1,
            Counts::U8(column) => put(column, 0),
            Counts::U16(column) => put(column, 0),
            Counts::U32(column) => put(column, 0),
            Counts::U64(column) => put(column, 0),
        }
    }

    /// Drops the count of `cell`; the last cell's count takes its place.
    pub(crate) fn swap_remove(&mut self, cell: usize) {
        match self {
            Counts::Zero(length) => *length -= 1,
            Counts::U8(column) => drop(column.swap_remove(cell)),
            Counts::U16(column) => drop(column.swap_remove(cell)),
            Counts::U32(column) => drop(column.swap_remove(cell)),
            Counts::U64(column) => drop(column.swap_remove(cell)),
        }
    }

    /// Each cell's count, in cell order.
    pub(crate) fn iter(&self) -> CountsIter<'_> {
        CountsIter {
            counts: self,
            next_cell: 0,
        }
    }

    /// Every cell's count added up.
    pub(crate) fn total(&self) -> u64 {
        match self {
            Counts::Zero(_) => 0,
            Counts::U8(column) => total_of(column, 0),
            Counts::U16(column) => total_of(column, 0),
            Counts::U32(column) => total_of(column, 0),
            Counts::U64(column) => total_of(column, 0),
        }
    }

    /// Calls `visit` with each cell's place, its entry of `beside` and its
    /// count. Not once for a column of zeros: every caller adds what it
    /// is given.
    pub(crate) fn each<Beside: Copy>(
        &self,
        beside: &[Beside],
        visit: impl FnMut(usize, Beside, u64),
    ) {
        match self {
            Counts::Zero(_) => {}
            Counts::U8(column) => walk(column, beside, visit),
            Counts::U16(column) => walk(column, beside, visit),
            Counts::U32(column) => walk(column, beside, visit),
            Counts::U64(column) => walk(column, beside, visit),
        }
    }
}

/// Each count of a column, in cell order.
// Borrows the column, so reading it copies nothing.
pub(crate) struct CountsIter<'column> {
    counts: &'column Counts,
    next_cell: usize,
}

impl Iterator for CountsIter<'_> {
    type Item = u64;

    fn next(&mut self) -> Option<u64> {
        if self.next_cell == self.counts.len() {
            return None;
        }
        self.next_cell += 1;
        Some(self.counts.get(self.next_cell - 1))
    }

    // Exact, so a column copied into a list allocates once.
    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.counts.len() - self.next_cell;
        (left, Some(left))
    }
}

fn both_within(least: i128, greatest: i128, range: RangeInclusive<i128>) -> bool {
    range.contains(&least) && range.contains(&greatest)
}

/// Sums, one a cell.
#[derive(Debug)]
pub(crate) enum Sums {
    I32(Vec<i32>),
    I64(Vec<i64>),
    I128(Vec<i128>),
}

impl Sums {
    /// No cells yet, in the narrowest type.
    pub(crate) const fn new() -> Sums {
        Sums::I32(Vec::new())
    }

    /// `length` sums in the narrowest type that holds `least` and
    /// `greatest`.
    fn holding(
        least: i128,
        greatest: i128,
        length: usize,
        sums: impl Iterator<Item = i128>,
    ) -> Sums {
        if both_within(least, greatest, i32::MIN.into()..=i32::MAX.into()) {
            Sums::I32(held(sums, length))
        } else if both_within(least, greatest, i64::MIN.into()..=i64::MAX.into()) {
            Sums::I64(held(sums, length))
        } else {
            Sums::I128(held(sums, length))
        }
    }

    /// Cells summed.
    pub(crate) fn len(&self) -> usize {
        match self {
            Sums::I32(column) => column.len(),
            Sums::I64(column) => column.len(),
            Sums::I128(column) => column.len(),
        }
    }

    /// The sum of `cell`.
    pub(crate) fn get(&self, cell: usize) -> i128 {
        match self {
            Sums::I32(column) => column[cell].into(),
            Sums::I64(column) => column[cell].into(),
            Sums::I128(column) => column[cell],
        }
    }

    /// Adds `number` to the sum of `cell`, widening the column if it must.
    pub(crate) fn add(&mut self, cell: usize, number: i128) {
        let raised = match self {
            Sums::I32(column) => raise(column, cell, number),
            Sums::I64(column) => raise(column, cell, number),
            Sums::I128(column) => raise(column, cell, number),
        };
        if !raised {
            // The others fit a narrower type, so the sum is the end to hold.
            let sum = self.get(cell) + number;
            *self = Sums::holding(sum.min(0), sum.max(0), self.len(), self.iter());
            self.add(cell, number);
        }
    }

    /// A new cell's sum, zero.
    pub(crate) fn push(&mut self) {
        match self {
            Sums::I32(column) => put(column, 0),
            Sums::I64(column) => put(column, 0),
            Sums::I128(column) => put(column, 0),
        }
    }

    /// Drops the sum of `cell`; the last cell's sum takes its place.
    pub(crate) fn swap_remove(&mut self, cell: usize) {
        match self {
            Sums::I32(column) => drop(column.swap_remove(cell)),
            Sums::I64(column) => drop(column.swap_remove(cell)),
            Sums::I128(column) => drop(column.swap_remove(cell)),
        }
    }

    /// Each cell's sum, in cell order.
    pub(crate) fn iter(&self) -> SumsIter<'_> {
        SumsIter {
            sums: self,
            next_cell: 0,
        }
    }

    /// Every cell's sum added up.
    pub(crate) fn total(&self) -> i128 {
        match self {
            Sums::I32(column) => total_of(column, 0),
            Sums::I64(column) => total_of(column, 0),
            Sums::I128(column) => total_of(column, 0),
        }
    }

    /// Calls `visit` with each cell's place, its entry of `beside`, its sum
    /// and its count among `counts`.
    pub(crate) fn each<Beside: Copy>(
        &self,
        counts: &Counts,
        beside: &[Beside],
        visit: impl FnMut(usize, Beside, i128, u64),
    ) {
        match self {
            Sums::I32(column) => walk_with(column, counts, beside, visit),
            Sums::I64(column) => walk_with(column, counts, beside, visit),
            Sums::I128(column) => walk_with(column, counts, beside, visit),
        }
    }
}

/// Each sum of a column, in cell order.
// Borrows the column, so reading it copies nothing.
pub(crate) struct SumsIter<'column> {
    sums: &'column Sums,
    next_cell: usize,
}

impl Iterator for SumsIter<'_> {
    type Item = i128;

    fn next(&mut self) -> Option<i128> {
        if self.next_cell == self.sums.len() {
            return None;
        }
        self.next_cell += 1;
        Some(self.sums.get(self.next_cell - 1))
    }

    // Exact, so a column copied into a list allocates once.
    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = self.sums.len() - self.next_cell;
        (left, Some(left))
    }
}

/// Sums of squares, one a cell, for a variance: as [`Sums`] while every
/// sum fits an `i128`, then in 256 bits. A square of an `i64` is below
/// 2^126, so two near the ends of `bigint` already pass an `i128`; below
/// 2^64 rows a sum stays below 2^190.
#[derive(Debug)]
pub(crate) enum Squares {
    Narrow(Sums),
    Wide(Vec<U256>),
}

impl Squares {
    pub(crate) const fn new() -> Squares {
        Squares::Narrow(Sums::new())
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Squares::Narrow(sums) => sums.len(),
            Squares::Wide(column) => column.len(),
        }
    }

    /// The sum of squares of `cell`.
    pub(crate) fn get(&self, cell: usize) -> U256 {
        match self {
            Squares::Narrow(sums) => wrapped(sums.get(cell)),
            Squares::Wide(column) => column[cell],
        }
    }

    /// Adds `square` to the sum of `cell`, widening the column if it must.
    pub(crate) fn add(&mut self, cell: usize, square: U256) {
        if let Squares::Narrow(sums) = self {
            if let Some(square) = narrow(square)
                && sums.get(cell).checked_add(square).is_some()
            {
                sums.add(cell, square);
                return;
            }
            self.widen();
        }
        let Squares::Wide(column) = self else {
            unreachable!("widened above")
        };
        column[cell] = column[cell].wrapping_add(square);
    }

    /// Takes `square` out of the sum of `cell`, widening the column if it
    /// must: a cell whose rows go out before they come in runs below zero.
    pub(crate) fn sub(&mut self, cell: usize, square: U256) {
        if let Squares::Narrow(sums) = self {
            if let Some(square) = narrow(square)
                && sums.get(cell).checked_sub(square).is_some()
            {
                sums.add(cell, -square);
                return;
            }
            self.widen();
        }
        let Squares::Wide(column) = self else {
            unreachable!("widened above")
        };
        column[cell] = column[cell].wrapping_sub(square);
    }

    pub(crate) fn push(&mut self) {
        match self {
            Squares::Narrow(sums) => sums.push(),
            Squares::Wide(column) => put(column, U256::ZERO),
        }
    }

    pub(crate) fn swap_remove(&mut self, cell: usize) {
        match self {
            Squares::Narrow(sums) => sums.swap_remove(cell),
            Squares::Wide(column) => drop(column.swap_remove(cell)),
        }
    }

    fn widen(&mut self) {
        let Squares::Narrow(sums) = self else {
            return;
        };
        let mut column = Vec::with_capacity(sums.len());
        for sum in sums.iter() {
            column.push(wrapped(sum));
        }
        *self = Squares::Wide(column);
    }
}

/// `square` as an `i128`, if it fits one.
fn narrow(square: U256) -> Option<i128> {
    i128::try_from(square.to_u128()?).ok()
}

/// `sum` modulo 2^256. A sum of squares is below zero only for a cell
/// whose rows went out without coming in; wrapping keeps it exact once
/// they do.
fn wrapped(sum: i128) -> U256 {
    let magnitude = U256::from(sum.unsigned_abs());
    if sum < 0 {
        U256::ZERO.wrapping_sub(magnitude)
    } else {
        magnitude
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A column of `counts`, in the narrowest type that holds them.
    fn counts_of(counts: &[u64]) -> Counts {
        let most = counts.iter().copied().max().unwrap_or(0);
        Counts::holding(most, counts.len(), counts.iter().copied())
    }

    /// A column of `sums`, in the narrowest type that holds them.
    fn sums_of(sums: &[i128]) -> Sums {
        let least = sums.iter().copied().min().unwrap_or(0);
        let greatest = sums.iter().copied().max().unwrap_or(0);
        Sums::holding(least, greatest, sums.len(), sums.iter().copied())
    }

    use crate::test_rng::Rng;

    fn width(counts: &Counts) -> usize {
        match counts {
            Counts::Zero(_) => 0,
            Counts::U8(_) => 1,
            Counts::U16(_) => 2,
            Counts::U32(_) => 4,
            Counts::U64(_) => 8,
        }
    }

    fn sum_width(sums: &Sums) -> usize {
        match sums {
            Sums::I32(_) => 4,
            Sums::I64(_) => 8,
            Sums::I128(_) => 16,
        }
    }

    /// What a walk over a column gives one cell.
    #[derive(Debug, PartialEq, Eq)]
    struct Visit<Number> {
        cell: usize,
        beside: usize,
        number: Number,
        count: u64,
    }

    /// What a column of counts read beside `beside` gives each cell.
    fn read(counts: &Counts, beside: &[usize]) -> Vec<Visit<u64>> {
        let mut seen = Vec::new();
        counts.each(beside, |cell, beside, count| {
            seen.push(Visit {
                cell,
                beside,
                number: count,
                count: 0,
            });
        });
        seen
    }

    /// Each entry of `list`, beside its own place.
    fn every_cell<Number: Copy>(list: &[Number], count: u64) -> Vec<Visit<Number>> {
        let mut visits = Vec::with_capacity(list.len());
        for (cell, &number) in list.iter().enumerate() {
            visits.push(Visit {
                cell,
                beside: cell,
                number,
                count,
            });
        }
        visits
    }

    /// Cells come and go and their counts rise and fall, against a plain
    /// list: every count read is the list's, one at a time and all at
    /// once, and the column is as wide as the widest count it has held.
    #[test]
    fn counts_follow_a_list_at_every_width() {
        for most in [1, 200, 60_000, 4_000_000_000, 1 << 50] {
            let mut rng = Rng::new(most);
            let mut counts = Counts::Zero(0);
            let mut list: Vec<u64> = Vec::new();
            let mut widest = 0;
            for step in 0..4_000 {
                let at = format!("up to {most}, step {step}");
                match (rng.below(10), list.len()) {
                    (0..=2, _) | (_, 0) => {
                        counts.push();
                        list.push(0);
                    }
                    (3, length) => {
                        let cell = rng.position_below(length);
                        counts.swap_remove(cell);
                        list.swap_remove(cell);
                    }
                    (4..=7, length) => {
                        let (cell, number) = (rng.position_below(length), rng.below(most));
                        counts.add(cell, number);
                        list[cell] += number;
                    }
                    (_, length) => {
                        let cell = rng.position_below(length);
                        let number = rng.below(list[cell] + 1);
                        counts.sub(cell, number);
                        list[cell] -= number;
                    }
                }
                widest = widest.max(list.iter().copied().max().unwrap_or(0));
                assert_eq!(counts.len(), list.len(), "{at}");
                assert_eq!(width(&counts), width(&counts_of(&[widest])), "{at}");
                if step % 97 == 0 {
                    assert!(counts.iter().eq(list.iter().copied()), "{at}");
                    assert_eq!(counts.total(), list.iter().sum::<u64>(), "{at}");
                    let beside: Vec<usize> = (0..list.len()).collect();
                    // A column of zeros is passed over whole.
                    let expected = if widest == 0 {
                        Vec::new()
                    } else {
                        every_cell(&list, 0)
                    };
                    assert_eq!(read(&counts, &beside), expected, "{at}");
                    let anew = counts_of(&list);
                    assert!(anew.iter().eq(list.iter().copied()), "{at}");
                    assert!(width(&anew) <= width(&counts), "{at}");
                }
            }
        }
    }

    /// Counts at the ends of each width take that width and no more.
    #[test]
    fn a_count_takes_the_bytes_it_needs() {
        for (count, bytes) in [
            (0, 0),
            (1, 1),
            (0xff, 1),
            (0x100, 2),
            (0xffff, 2),
            (0x1_0000, 4),
            (0xffff_ffff, 4),
            (0x1_0000_0000, 8),
            (u64::MAX, 8),
        ] {
            assert_eq!(width(&counts_of(&[0, count, 0])), bytes, "{count}");
            let mut counts = counts_of(&[0, 0, 0]);
            counts.set(1, count);
            assert_eq!(width(&counts), bytes, "{count} set");
            assert!(counts.iter().eq([0, count, 0]), "{count} set");
        }
    }

    /// Sums rise and fall past the ends of each width, against a plain
    /// list, read alone and beside counts of every width.
    #[test]
    fn sums_follow_a_list_at_every_width() {
        for most in [1_000_i128, 3_000_000_000, i128::from(i64::MAX) * 3] {
            let mut rng = Rng::new(7);
            let mut sums = Sums::new();
            let mut list: Vec<i128> = Vec::new();
            let (mut least, mut greatest) = (0, 0);
            for step in 0..4_000 {
                let at = format!("up to {most}, step {step}");
                match (rng.below(10), list.len()) {
                    (0..=2, _) | (_, 0) => {
                        sums.push();
                        list.push(0);
                    }
                    (3, length) => {
                        let cell = rng.position_below(length);
                        sums.swap_remove(cell);
                        list.swap_remove(cell);
                    }
                    (_, length) => {
                        let cell = rng.position_below(length);
                        let number = i128::from(rng.next()) % (2 * most + 1) - most;
                        sums.add(cell, number);
                        list[cell] += number;
                        (least, greatest) = (least.min(list[cell]), greatest.max(list[cell]));
                    }
                }
                assert_eq!(sums.len(), list.len(), "{at}");
                assert_eq!(
                    sum_width(&sums),
                    sum_width(&sums_of(&[least, greatest])),
                    "{at}"
                );
                if step % 97 == 0 {
                    assert!(sums.iter().eq(list.iter().copied()), "{at}");
                    assert_eq!(sums.total(), list.iter().sum::<i128>(), "{at}");
                    let beside: Vec<usize> = (0..list.len()).collect();
                    for counts in [0, 9, 300, 70_000, 5_000_000_000] {
                        let rows = counts_of(&vec![counts; list.len()]);
                        let mut seen = Vec::new();
                        sums.each(&rows, &beside, |cell, beside, sum, count| {
                            seen.push(Visit {
                                cell,
                                beside,
                                number: sum,
                                count,
                            });
                        });
                        assert_eq!(seen, every_cell(&list, counts), "{at}, beside {counts}");
                    }
                    assert!(sums_of(&list).iter().eq(list.iter().copied()), "{at}");
                }
            }
        }
    }

    /// Sums of squares follow a list kept modulo 2^256, through adds
    /// and takes small and past an `i128`, a cell taken below zero, and
    /// cells pushed and dropped before and after the column widens.
    #[test]
    fn squares_follow_a_list_past_an_i128() {
        for seed in 0..4 {
            let mut rng = Rng::new(seed);
            let mut squares = Squares::new();
            let mut list: Vec<U256> = Vec::new();
            for step in 0..2_000 {
                let at = format!("seed {seed}, step {step}");
                match (rng.below(10), list.len()) {
                    (0..=1, _) | (_, 0) => {
                        squares.push();
                        list.push(U256::ZERO);
                    }
                    (2, length) => {
                        let cell = rng.position_below(length);
                        squares.swap_remove(cell);
                        list.swap_remove(cell);
                    }
                    (draw, length) => {
                        let cell = rng.position_below(length);
                        // Mostly small, now and then the square of a value
                        // near the ends of bigint, which passes an i128.
                        let root = if rng.below(40) == 0 {
                            u128::from(rng.next() | 1 << 63)
                        } else {
                            u128::from(rng.below(1_000))
                        };
                        let square = U256::product(root, root);
                        if draw < 7 {
                            squares.add(cell, square);
                            list[cell] = list[cell].wrapping_add(square);
                        } else {
                            squares.sub(cell, square);
                            list[cell] = list[cell].wrapping_sub(square);
                        }
                    }
                }
                assert_eq!(squares.len(), list.len(), "{at}");
                for (cell, &expected) in list.iter().enumerate() {
                    assert_eq!(squares.get(cell), expected, "{at}, cell {cell}");
                }
            }
            assert!(matches!(squares, Squares::Wide(_)), "seed {seed} widened");
        }
    }

    /// Squares taken out of a cell that never held them run below the
    /// least `i128`: the column widens and stays exact, as it does going
    /// past the greatest, and a take that fits stays narrow.
    #[test]
    fn squares_taken_below_an_i128_widen() {
        // Its square is just below 2^127, the most a narrow take holds.
        let root: u128 = 13_000_000_000_000_000_000;
        let square = U256::product(root, root);
        let mut squares = Squares::new();
        squares.push();
        squares.sub(0, square);
        assert!(matches!(squares, Squares::Narrow(_)));
        assert_eq!(squares.get(0), U256::ZERO.wrapping_sub(square));
        squares.sub(0, square);
        assert!(matches!(squares, Squares::Wide(_)));
        let twice = square.wrapping_add(square);
        assert_eq!(squares.get(0), U256::ZERO.wrapping_sub(twice));
        squares.add(0, twice);
        assert_eq!(squares.get(0), U256::ZERO);
    }

    /// Sums at the ends of each width take that width and no more.
    #[test]
    fn a_sum_takes_the_bytes_it_needs() {
        for (sum, bytes) in [
            (0, 4),
            (i128::from(i32::MAX), 4),
            (i128::from(i32::MIN), 4),
            (i128::from(i32::MAX) + 1, 8),
            (i128::from(i32::MIN) - 1, 8),
            (i128::from(i64::MAX), 8),
            (i128::from(i64::MIN), 8),
            (i128::from(i64::MAX) + 1, 16),
            (i128::from(i64::MIN) - 1, 16),
        ] {
            assert_eq!(sum_width(&sums_of(&[1, sum, -1])), bytes, "{sum}");
            let mut sums = sums_of(&[1, 0, -1]);
            sums.add(1, sum);
            assert_eq!(sum_width(&sums), bytes, "{sum} added");
            assert!(sums.iter().eq([1, sum, -1]), "{sum} added");
        }
    }

    /// A column grows by an eighth, not by doubling.
    #[test]
    fn a_column_grows_by_an_eighth() {
        let mut column: Vec<u64> = Vec::new();
        for number in 0..10_000 {
            put(&mut column, number);
            assert!(column.capacity() <= column.len() + (column.len() / 8).max(4));
        }
        assert!(column.iter().copied().eq(0..10_000));
    }
}
