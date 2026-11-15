//! The byte layout under [`ValueCounts`]: records of a value and its
//! count, packed into a cell's own bytes or into a run, each field as few
//! bytes wide as the widest of its kind needs. Here are the reads and
//! writes of single fields, the search for a value among the records, and
//! adding to or taking from one run.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::hint;

use super::{CELL_HEADER, INLINE, RUN_HEADER, RUN_MAX_BYTES, ValueCounts};
use crate::sealed::ValueCount;

/// Bytes of a record's value and of its count.
#[derive(Clone, Copy)]
pub(super) struct Widths {
    pub(super) value: usize,
    pub(super) count: usize,
}

impl Widths {
    /// Bytes of one record.
    pub(super) fn size(self) -> usize {
        self.value + self.count
    }
}

/// Records where they lie: `record_count` of them in `bytes` from `start`
/// on, each value less `base`.
#[derive(Clone, Copy)]
pub(super) struct Records<'bytes> {
    pub(super) base: i64,
    pub(super) widths: Widths,
    // Borrowed: the records are read in place, never copied out.
    bytes: &'bytes [u8],
    start: usize,
    pub(super) record_count: usize,
}

/// Where a value lies among the records, as a search finds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Search {
    /// The record that holds the value.
    Found(usize),
    /// The place a record of the value would go, keeping them ascending.
    Absent(usize),
}

/// How more of a value goes into records of fixed widths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Placement {
    /// The record that counts the value, and its count after.
    Counted { record: usize, new_count: u64 },
    /// The place a new record of the value goes.
    Inserted { record: usize },
    /// The widths don't hold the value or its new count.
    TooWide,
}

/// At most [`INLINE`] entries copied out of a cell, with room for one more.
pub(super) struct FewEntries {
    pub(super) entries: [ValueCount; INLINE + 1],
    pub(super) length: usize,
}

impl FewEntries {
    /// The entries held.
    pub(super) fn as_slice(&self) -> &[ValueCount] {
        &self.entries[..self.length]
    }
}

impl<'bytes> Records<'bytes> {
    /// The records of a run, behind its [`RUN_HEADER`].
    pub(super) fn of(run: &'bytes [u8]) -> Records<'bytes> {
        Records {
            base: i64::from_le_bytes(word(run, 4)),
            widths: Widths {
                value: run[0].into(),
                count: run[1].into(),
            },
            bytes: run,
            start: RUN_HEADER,
            record_count: u16::from_le_bytes([run[2], run[3]]).into(),
        }
    }

    /// The records a cell holds in itself, behind its [`CELL_HEADER`].
    pub(super) fn in_cell(cell: &'bytes [u8; CELL_HEADER + INLINE]) -> Records<'bytes> {
        Records {
            base: 0,
            widths: Widths {
                value: cell[1].into(),
                count: cell[2].into(),
            },
            bytes: cell,
            start: CELL_HEADER,
            record_count: cell[0].into(),
        }
    }

    /// Bytes of one record.
    pub(super) fn size(self) -> usize {
        self.widths.size()
    }

    /// Where record `record` starts in the bytes.
    pub(super) fn at(self, record: usize) -> usize {
        self.start + record * self.size()
    }

    /// The value of record `record`.
    pub(super) fn value(self, record: usize) -> i64 {
        let end = self.at(record) + self.widths.value;
        (self.base).wrapping_add(signed(self.bytes, end, self.widths.value))
    }

    /// How many rows hold the value of record `record`.
    pub(super) fn count(self, record: usize) -> u64 {
        unsigned(self.bytes, self.at(record + 1), self.widths.count) + 1
    }

    /// Record `record` as a value and its count.
    pub(super) fn entry(self, record: usize) -> ValueCount {
        ValueCount {
            value: self.value(record),
            count: self.count(record),
        }
    }

    /// Where `value` is, or where it would go.
    pub(super) fn find(self, value: i64) -> Search {
        let value_width = self.widths.value;
        if value_width > 4 {
            let field = Field::Wide {
                first: self.start + value_width - 8,
                unused: 64 - 8 * value_width,
                base: self.base,
            };
            return halve(self, field, value);
        }
        // Up to four bytes never wrap, so `value` is sought as what a record
        // holds of it: less the base.
        let Some(offset) = value.checked_sub(self.base) else {
            let place = if value < self.base {
                0
            } else {
                self.record_count
            };
            return Search::Absent(place);
        };
        let field = Field::Narrow {
            first: self.start + value_width - 4,
            unused: 32 - 8 * value_width,
        };
        halve(self, field, offset)
    }

    /// Whether a record of these widths holds `value` counted `count` times.
    fn holds(self, value: i64, count: u64) -> bool {
        let value_width = match value.checked_sub(self.base) {
            Some(offset) => signed_width(offset),
            // A value further from the base than an `i64` holds wraps,
            // which eight bytes take.
            None => 8,
        };
        value_width <= self.widths.value && unsigned_width(count - 1) <= self.widths.count
    }

    /// How `count` more of `value` goes in.
    pub(super) fn place(self, value: i64, count: u64) -> Placement {
        match self.find(value) {
            Search::Found(record) => {
                let new_count = self.count(record) + count;
                if self.holds(value, new_count) {
                    return Placement::Counted { record, new_count };
                }
                Placement::TooWide
            }
            Search::Absent(record) => {
                if self.holds(value, count) {
                    return Placement::Inserted { record };
                }
                Placement::TooWide
            }
        }
    }

    /// The entries of at most [`INLINE`] records, with a place for one more.
    pub(super) fn few(self) -> FewEntries {
        let mut few = FewEntries {
            entries: [ValueCount { value: 0, count: 0 }; INLINE + 1],
            length: self.record_count,
        };
        for record in 0..self.record_count {
            few.entries[record] = self.entry(record);
        }
        few
    }

    /// The entries with `count` more of `value`.
    pub(super) fn with(self, value: i64, count: u64) -> Vec<ValueCount> {
        let mut entries = Vec::with_capacity(self.record_count + 1);
        for record in 0..self.record_count {
            entries.push(self.entry(record));
        }
        match self.find(value) {
            Search::Found(record) => entries[record].count += count,
            Search::Absent(record) => entries.insert(record, ValueCount { value, count }),
        }
        entries
    }
}

/// How a search reads a record's value: as the eight bytes that end with
/// it, adding back the base, or as the four, which never wrap and are
/// compared less the base.
#[derive(Clone, Copy)]
enum Field {
    Wide {
        /// Where the eight bytes of the first record's value start.
        first: usize,
        /// High bits of the eight that belong to the record before.
        unused: usize,
        base: i64,
    },
    Narrow {
        first: usize,
        unused: usize,
    },
}

/// The value `field` reads `at` bytes past the first record's.
fn read(bytes: &[u8], field: Field, at: usize) -> i64 {
    match field {
        Field::Wide {
            first,
            unused,
            base,
        } => base.wrapping_add(i64::from_le_bytes(word(bytes, first + at)) >> unused),
        Field::Narrow { first, unused } => {
            (i32::from_le_bytes(word(bytes, first + at)) >> unused).into()
        }
    }
}

/// Where `sought` is among the ascending values of `records`, read as
/// `field` says.
///
/// Halved without a branch on a value read: which half holds `sought` is a
/// coin toss, and a processor that guesses it wrong pays more than the
/// read costs. And by powers of two, after one step that leaves such a
/// count, so the next place is an addition away, with no product to wait
/// for.
fn halve(records: Records<'_>, field: Field, sought: i64) -> Search {
    let (record_count, size, bytes) = (records.record_count, records.size(), records.bytes);
    if record_count == 0 {
        return Search::Absent(0);
    }
    // The last value that is `sought` or less is among `span` from `low`
    // on, if there is one at all.
    let mut span = 1 << record_count.ilog2();
    let mut step = span * size;
    let above = read(bytes, field, (record_count - span) * size) > sought;
    let mut low = hint::select_unpredictable(above, 0, record_count - span);
    let mut at = low * size;
    while span > 1 {
        span /= 2;
        step /= 2;
        let above = read(bytes, field, at + step) > sought;
        low = hint::select_unpredictable(above, low, low + span);
        at = hint::select_unpredictable(above, at, at + step);
    }
    match read(bytes, field, at).cmp(&sought) {
        Ordering::Equal => Search::Found(low),
        Ordering::Less => Search::Absent(low + 1),
        Ordering::Greater => Search::Absent(low),
    }
}

/// Bytes that hold `number` in two's complement.
pub(super) fn signed_width(number: i64) -> usize {
    let magnitude = (number ^ (number >> 63)).cast_unsigned();
    (65 - magnitude.leading_zeros() as usize).div_ceil(8)
}

/// Bytes that hold `number`, none for zero.
fn unsigned_width(number: u64) -> usize {
    (64 - number.leading_zeros() as usize).div_ceil(8)
}

/// Bytes that hold the widest count of `entries`, kept less one.
pub(super) fn count_width(entries: &[ValueCount]) -> usize {
    let mut most = 0;
    for entry in entries {
        most = most.max(entry.count - 1);
    }
    unsigned_width(most)
}

/// A width or a record count as the header byte that holds it.
pub(super) fn byte(width: usize) -> u8 {
    u8::try_from(width).expect("a width or a cell's records fit a byte")
}

/// The `WIDTH` bytes from `at`. A field of up to four bytes is read and
/// written as the four that end with it, a wider one as the eight: one
/// load or store where a copy of its own length would be a call. A header
/// is three bytes or more, so there are always that many.
// Generic over the byte count so a read is a fixed-size array the
// compiler turns into one load.
fn word<const WIDTH: usize>(bytes: &[u8], at: usize) -> [u8; WIDTH] {
    bytes[at..at + WIDTH].try_into().expect("WIDTH bytes")
}

/// The `width` bytes that end at `end`, as a signed integer.
fn signed(bytes: &[u8], end: usize, width: usize) -> i64 {
    if width > 4 {
        i64::from_le_bytes(word(bytes, end - 8)) >> (64 - 8 * width)
    } else {
        (i32::from_le_bytes(word(bytes, end - 4)) >> (32 - 8 * width)).into()
    }
}

/// The `width` bytes that end at `end`, as an integer; zero of none.
fn unsigned(bytes: &[u8], end: usize, width: usize) -> u64 {
    if width > 4 {
        u64::from_le_bytes(word(bytes, end - 8)) >> (64 - 8 * width)
    } else {
        u64::from(u32::from_le_bytes(word(bytes, end - 4))) >> (32 - 8 * width)
    }
}

/// Writes the low `width` bytes of `number` to end at `end`.
pub(super) fn write(bytes: &mut [u8], end: usize, width: usize, number: u64) {
    if width > 4 {
        let ahead = 64 - 8 * width;
        let kept = u64::from_le_bytes(word(bytes, end - 8)) & ((1 << ahead) - 1);
        bytes[end - 8..end].copy_from_slice(&(kept | number << ahead).to_le_bytes());
    } else {
        let ahead = 32 - 8 * width;
        let kept = u64::from(u32::from_le_bytes(word(bytes, end - 4))) & ((1 << ahead) - 1);
        bytes[end - 4..end].copy_from_slice(&(kept | number << ahead).to_le_bytes()[..4]);
    }
}

/// Writes a record at `at`: `offset`, then `count` less one.
pub(super) fn record(bytes: &mut [u8], at: usize, widths: Widths, offset: i64, count: u64) {
    write(
        bytes,
        at + widths.value,
        widths.value,
        offset.cast_unsigned(),
    );
    write(bytes, at + widths.size(), widths.count, count - 1);
}

fn set_record_count(run: &mut [u8], record_count: usize) {
    let record_count = u16::try_from(record_count).expect("a run's records fit two bytes");
    run[2..4].copy_from_slice(&record_count.to_le_bytes());
}

/// Counts `count` more of `value` in the run. False, with nothing counted,
/// when the run's widths don't hold that or the run is full.
pub(super) fn put(run: &mut Vec<u8>, value: i64, count: u64) -> bool {
    let records = Records::of(run);
    let (size, widths) = (records.size(), records.widths);
    let run_bytes = run.len();
    match records.place(value, count) {
        Placement::Counted {
            record: place,
            new_count,
        } => {
            let count_end = records.at(place + 1);
            write(run, count_end, widths.count, new_count - 1);
        }
        Placement::Inserted { record: place } if run_bytes + size <= RUN_MAX_BYTES => {
            let at = records.at(place);
            let offset = value.wrapping_sub(records.base);
            let record_count = records.record_count;
            if run_bytes + size > run.capacity() {
                // A `Vec` doubles when full, which would leave a run half
                // empty. An allocator hands out sixteens.
                let wanted = (run_bytes + size + run_bytes / 8).next_multiple_of(16);
                run.reserve_exact(wanted - run_bytes);
            }
            run.resize(run_bytes + size, 0);
            run.copy_within(at..run_bytes, at + size);
            record(run, at, widths, offset, count);
            set_record_count(run, record_count + 1);
        }
        Placement::Inserted { .. } | Placement::TooWide => return false,
    }
    true
}

/// What [`take`] took of a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Taken {
    /// Fewer were held, and nothing was taken.
    TooFew,
    /// Some of the value is held still.
    Some,
    /// That was the last of the value.
    Last,
}

/// Takes `count` of `value` back out of the run.
pub(super) fn take(run: &mut Vec<u8>, value: i64, count: u64) -> Taken {
    let records = Records::of(run);
    let Search::Found(record) = records.find(value) else {
        return Taken::TooFew;
    };
    let held_count = records.count(record);
    let (at, size, run_bytes) = (records.at(record), records.size(), run.len());
    let (record_count, count_width) = (records.record_count, records.widths.count);
    match held_count.cmp(&count) {
        Ordering::Less => Taken::TooFew,
        Ordering::Equal => {
            run.copy_within(at + size..run_bytes, at);
            run.truncate(run_bytes - size);
            set_record_count(run, record_count - 1);
            Taken::Last
        }
        Ordering::Greater => {
            let left = held_count - count - 1;
            write(run, at + size, count_width, left);
            Taken::Some
        }
    }
}

/// `entries`, which ascend and are not empty, as runs of at most
/// [`RUN_MAX_BYTES`] under the narrowest widths that hold them.
pub(super) fn runs_into(entries: &[ValueCount], runs: &mut BTreeMap<i64, Vec<u8>>) {
    let base = entries[entries.len() / 2].value;
    let (least, greatest) = (entries[0].value, entries[entries.len() - 1].value);
    let value_width = match (least.checked_sub(base), greatest.checked_sub(base)) {
        (Some(below), Some(above)) => signed_width(below).max(signed_width(above)),
        // As in `Records::holds`.
        _ => 8,
    };
    let widths = Widths {
        value: value_width,
        count: count_width(entries),
    };
    let size = widths.size();
    let run_bytes = RUN_HEADER + entries.len() * size;
    if run_bytes > RUN_MAX_BYTES {
        let (low, high) = entries.split_at(entries.len() / 2);
        runs_into(low, runs);
        runs_into(high, runs);
        return;
    }
    let mut run = vec![0; run_bytes];
    run[0] = byte(widths.value);
    run[1] = byte(widths.count);
    set_record_count(&mut run, entries.len());
    run[4..RUN_HEADER].copy_from_slice(&base.to_le_bytes());
    for (position, entry) in entries.iter().enumerate() {
        let at = RUN_HEADER + position * size;
        record(
            &mut run,
            at,
            widths,
            entry.value.wrapping_sub(base),
            entry.count,
        );
    }
    runs.insert(least, run);
}

/// The key of the run that holds `value` if any does: the last key that is
/// `value` or less, or the first, whose run holds what is below every key.
pub(super) fn key_of(runs: &BTreeMap<i64, Vec<u8>>, value: i64) -> i64 {
    if let Some((&key, _)) = runs.range(..=value).next_back() {
        return key;
    }
    *runs.keys().next().expect("runs hold a run")
}

/// What a run that a value left is kept as, if not as it is: in the cell,
/// once it would half fill one. Not sooner, or a cell at the edge would
/// move at every change.
pub(super) fn settled(run: &mut Vec<u8>) -> Option<ValueCounts> {
    if run.len() - RUN_HEADER <= INLINE / 2 {
        let few = Records::of(run).few();
        return Some(ValueCounts::from_ascending(few.as_slice()));
    }
    if run.len() <= run.capacity() / 2 {
        run.shrink_to_fit();
    }
    None
}
