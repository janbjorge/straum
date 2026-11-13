//! One cell's count per value, which a min or max keeps so that a removed
//! end reveals the next one. Values repeat little inside a cell, so these
//! counts are most of what a view with a min or max holds, and a cell keeps
//! each in the fewest bytes its values need.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::{hint, mem};

/// Bytes of records a cell holds in itself, where a run's pointer would
/// be. Nearly two thirds of the taxi cells hold no more.
const INLINE: usize = 28;

/// Ahead of a cell's own records: how many, a value's width, a count's.
const CELL_HEADER: usize = 3;

/// Ahead of a run's records: a value's width, a count's, how many records
/// in two bytes, and the base in eight.
const HEADER: usize = 12;

/// Most bytes in one run. Adding a value moves half the run on average,
/// which at this many costs about what finding the run among others does.
const RUN_MAX: usize = 4 << 10;

/// Values ascend. A record is a value less its run's base, in as many bytes
/// as the widest of the run takes, then the value's count less one
/// likewise: three fares seen once take two bytes each, where a pair of
/// integers took 16. The widths are fixed within a run, so a value is found
/// by halving, and a value or a count too wide for its run has the run
/// encoded anew.
#[derive(Debug)]
pub(crate) enum ValueCounts {
    /// Few values, in the cell itself behind [`CELL_HEADER`], their base
    /// zero.
    Inline([u8; CELL_HEADER + INLINE]),
    /// [`HEADER`], then the records.
    Run(Vec<u8>),
    /// Runs of at most [`RUN_MAX`] bytes. A run holds the values from its
    /// key up to the next run's, and the first those below every key too.
    Runs(BTreeMap<i64, Vec<u8>>),
}

// A column of these is a third of what a min or max holds beside its runs.
const _: () = assert!(mem::size_of::<ValueCounts>() == 32);

/// Records where they lie: `len` of them in `bytes` from `start` on.
#[derive(Clone, Copy)]
struct Records<'a> {
    base: i64,
    value_width: usize,
    count_width: usize,
    bytes: &'a [u8],
    start: usize,
    len: usize,
}

impl<'a> Records<'a> {
    fn of(run: &'a [u8]) -> Records<'a> {
        Records {
            base: i64::from_le_bytes(run[4..HEADER].try_into().expect("a base is eight bytes")),
            value_width: run[0].into(),
            count_width: run[1].into(),
            bytes: run,
            start: HEADER,
            len: u16::from_le_bytes([run[2], run[3]]).into(),
        }
    }

    fn in_cell(cell: &'a [u8; CELL_HEADER + INLINE]) -> Records<'a> {
        Records {
            base: 0,
            value_width: cell[1].into(),
            count_width: cell[2].into(),
            bytes: cell,
            start: CELL_HEADER,
            len: cell[0].into(),
        }
    }

    fn size(self) -> usize {
        self.value_width + self.count_width
    }

    /// Where record `i` starts in the bytes.
    fn at(self, i: usize) -> usize {
        self.start + i * self.size()
    }

    fn value(self, i: usize) -> i64 {
        let end = self.at(i) + self.value_width;
        (self.base).wrapping_add(signed(self.bytes, end, self.value_width))
    }

    fn count(self, i: usize) -> u64 {
        unsigned(self.bytes, self.at(i + 1), self.count_width) + 1
    }

    /// Where `v` is, or where it would go.
    fn find(self, v: i64) -> Result<usize, usize> {
        let Records {
            base, bytes, len, ..
        } = self;
        let size = self.size();
        // By width outside the halving, so a step is one load and a shift.
        if self.value_width > 4 {
            let (first, unused) = (self.start + self.value_width - 8, 64 - 8 * self.value_width);
            return halve(len, size, v, |at| {
                base.wrapping_add(i64::from_le_bytes(word(bytes, first + at)) >> unused)
            });
        }
        // Up to four bytes never wrap, so `v` is sought as what a record
        // holds of it: less the base.
        let Some(offset) = v.checked_sub(base) else {
            return Err(if v < base { 0 } else { len });
        };
        let (first, unused) = (self.start + self.value_width - 4, 32 - 8 * self.value_width);
        halve(len, size, offset, |at| {
            (i32::from_le_bytes(word(bytes, first + at)) >> unused).into()
        })
    }

    fn iter(self) -> impl Iterator<Item = (i64, u64)> + 'a {
        (0..self.len).map(move |i| (self.value(i), self.count(i)))
    }

    /// Whether a record of these widths holds `v` counted `n` times.
    fn holds(self, v: i64, n: u64) -> bool {
        // A value further from the base than an `i64` holds wraps, which
        // eight bytes take.
        let value_width = v.checked_sub(self.base).map_or(8, signed_width);
        value_width <= self.value_width && unsigned_width(n - 1) <= self.count_width
    }

    /// How `n` more of `v` goes in: the record that counts it and its new
    /// count, or where a record of it goes. `None` when these widths don't
    /// hold that.
    fn place(self, v: i64, n: u64) -> Option<Result<(usize, u64), usize>> {
        match self.find(v) {
            Ok(i) => {
                let count = self.count(i) + n;
                self.holds(v, count).then_some(Ok((i, count)))
            }
            Err(i) => self.holds(v, n).then_some(Err(i)),
        }
    }

    /// The pairs of at most [`INLINE`] records, with a place for one more.
    fn few(self) -> ([(i64, u64); INLINE + 1], usize) {
        let mut pairs = [(0, 0); INLINE + 1];
        for (pair, held) in pairs.iter_mut().zip(self.iter()) {
            *pair = held;
        }
        (pairs, self.len)
    }

    /// The pairs with `n` more of `v`.
    fn with(self, v: i64, n: u64) -> Vec<(i64, u64)> {
        let mut pairs = Vec::with_capacity(self.len + 1);
        pairs.extend(self.iter());
        match self.find(v) {
            Ok(i) => pairs[i].1 += n,
            Err(i) => pairs.insert(i, (v, n)),
        }
        pairs
    }
}

/// Where `v` is among `len` ascending values, or where it would go;
/// `value` reads the one `size` bytes times its place from the first.
///
/// Halved without a branch on a value read: which half holds `v` is a coin
/// toss, and a processor that guesses it wrong pays more than the read
/// costs. And by powers of two, after one step that leaves such a count,
/// so the next place is an addition away, with no product to wait for.
fn halve(len: usize, size: usize, v: i64, value: impl Fn(usize) -> i64) -> Result<usize, usize> {
    if len == 0 {
        return Err(0);
    }
    // The last value that is `v` or less is among `width` from `low` on, if
    // there is one at all.
    let mut width = 1 << len.ilog2();
    let mut step = width * size;
    let above = value((len - width) * size) > v;
    let mut low = hint::select_unpredictable(above, 0, len - width);
    let mut at = low * size;
    while width > 1 {
        width /= 2;
        step /= 2;
        let above = value(at + step) > v;
        low = hint::select_unpredictable(above, low, low + width);
        at = hint::select_unpredictable(above, at, at + step);
    }
    match value(at).cmp(&v) {
        Ordering::Equal => Ok(low),
        Ordering::Less => Err(low + 1),
        Ordering::Greater => Err(low),
    }
}

/// Bytes that hold `n` in two's complement.
fn signed_width(n: i64) -> usize {
    let magnitude = (n ^ (n >> 63)).cast_unsigned();
    (65 - magnitude.leading_zeros() as usize).div_ceil(8)
}

/// Bytes that hold `n`, none for zero.
fn unsigned_width(n: u64) -> usize {
    (64 - n.leading_zeros() as usize).div_ceil(8)
}

fn count_width(pairs: &[(i64, u64)]) -> usize {
    unsigned_width(pairs.iter().map(|&(_, n)| n - 1).max().unwrap_or(0))
}

fn byte(width: usize) -> u8 {
    u8::try_from(width).expect("a width or a cell's records fit a byte")
}

/// The `N` bytes from `at`. A field of up to four bytes is read and
/// written as the four that end with it, a wider one as the eight: one
/// load or store where a copy of its own length would be a call. A header
/// is three bytes or more, so there are always that many.
fn word<const N: usize>(bytes: &[u8], at: usize) -> [u8; N] {
    bytes[at..at + N].try_into().expect("N bytes")
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

/// Writes the low `width` bytes of `n` to end at `end`.
fn write(bytes: &mut [u8], end: usize, width: usize, n: u64) {
    if width > 4 {
        let ahead = 64 - 8 * width;
        let kept = u64::from_le_bytes(word(bytes, end - 8)) & ((1 << ahead) - 1);
        bytes[end - 8..end].copy_from_slice(&(kept | n << ahead).to_le_bytes());
    } else {
        let ahead = 32 - 8 * width;
        let kept = u64::from(u32::from_le_bytes(word(bytes, end - 4))) & ((1 << ahead) - 1);
        bytes[end - 4..end].copy_from_slice(&(kept | n << ahead).to_le_bytes()[..4]);
    }
}

/// Writes a record at `at`: `offset`, then `count` less one.
fn record(bytes: &mut [u8], at: usize, widths: (usize, usize), offset: i64, count: u64) {
    write(bytes, at + widths.0, widths.0, offset.cast_unsigned());
    write(bytes, at + widths.0 + widths.1, widths.1, count - 1);
}

fn set_len(run: &mut [u8], len: usize) {
    let len = u16::try_from(len).expect("a run's records fit two bytes");
    run[2..4].copy_from_slice(&len.to_le_bytes());
}

/// Counts `n` more of `v` in the run. False, with nothing counted, when
/// the run's widths don't hold that or the run is full.
fn put(run: &mut Vec<u8>, v: i64, n: u64) -> bool {
    let records = Records::of(run);
    let (size, widths) = (records.size(), (records.value_width, records.count_width));
    let len = run.len();
    match records.place(v, n) {
        Some(Ok((i, count))) => {
            let end = records.at(i + 1);
            write(run, end, widths.1, count - 1);
        }
        Some(Err(i)) if len + size <= RUN_MAX => {
            let (at, offset, held) = (records.at(i), v.wrapping_sub(records.base), records.len);
            if len + size > run.capacity() {
                // A `Vec` doubles when full, which would leave a run half
                // empty. An allocator hands out sixteens.
                run.reserve_exact((len + size + len / 8).next_multiple_of(16) - len);
            }
            run.resize(len + size, 0);
            run.copy_within(at..len, at + size);
            record(run, at, widths, offset, n);
            set_len(run, held + 1);
        }
        _ => return false,
    }
    true
}

/// Takes `n` of `v` back out of the run: whether that was the last of
/// `v`. `None`, with nothing taken, when fewer are held.
fn take(run: &mut Vec<u8>, v: i64, n: u64) -> Option<bool> {
    let records = Records::of(run);
    let i = records.find(v).ok()?;
    let (count, at, size) = (records.count(i), records.at(i), records.size());
    let (width, held, len) = (records.count_width, records.len, run.len());
    match count.cmp(&n) {
        Ordering::Less => return None,
        Ordering::Equal => {
            run.copy_within(at + size..len, at);
            run.truncate(len - size);
            set_len(run, held - 1);
        }
        Ordering::Greater => write(run, at + size, width, count - n - 1),
    }
    Some(count == n)
}

/// `pairs`, which ascend and are not empty, as runs of at most
/// [`RUN_MAX`] bytes under the narrowest widths that hold them.
fn runs_into(pairs: &[(i64, u64)], runs: &mut BTreeMap<i64, Vec<u8>>) {
    let base = pairs[pairs.len() / 2].0;
    let (least, greatest) = (pairs[0].0, pairs[pairs.len() - 1].0);
    let value_width = match (least.checked_sub(base), greatest.checked_sub(base)) {
        (Some(below), Some(above)) => signed_width(below).max(signed_width(above)),
        // As in `Records::holds`.
        _ => 8,
    };
    let widths = (value_width, count_width(pairs));
    let size = widths.0 + widths.1;
    let len = HEADER + pairs.len() * size;
    if len > RUN_MAX {
        let (low, high) = pairs.split_at(pairs.len() / 2);
        runs_into(low, runs);
        runs_into(high, runs);
        return;
    }
    let mut run = vec![0; len];
    run[0] = byte(widths.0);
    run[1] = byte(widths.1);
    set_len(&mut run, pairs.len());
    run[4..HEADER].copy_from_slice(&base.to_le_bytes());
    for (i, &(v, n)) in pairs.iter().enumerate() {
        record(&mut run, HEADER + i * size, widths, v.wrapping_sub(base), n);
    }
    runs.insert(least, run);
}

/// The key of the run that holds `v` if any does: the last key that is
/// `v` or less, or the first, whose run holds what is below every key.
fn key_of(runs: &BTreeMap<i64, Vec<u8>>, v: i64) -> i64 {
    let run = (runs.range(..=v).next_back()).or_else(|| runs.iter().next());
    *run.expect("runs hold a run").0
}

/// What a run that a value left is kept as, if not as it is: in the cell,
/// once it would half fill one. Not sooner, or a cell at the edge would
/// move at every change.
fn settled(run: &mut Vec<u8>) -> Option<ValueCounts> {
    if run.len() - HEADER <= INLINE / 2 {
        let (pairs, len) = Records::of(run).few();
        return Some(ValueCounts::from_pairs(&pairs[..len]));
    }
    if run.len() <= run.capacity() / 2 {
        run.shrink_to_fit();
    }
    None
}

impl ValueCounts {
    pub(crate) const EMPTY: ValueCounts = ValueCounts::Inline({
        let mut cell = [0; CELL_HEADER + INLINE];
        cell[1] = 1;
        cell
    });

    /// `pairs` ascend by value, each counted once at least.
    pub(crate) fn from_pairs(pairs: &[(i64, u64)]) -> ValueCounts {
        let (Some(&(least, _)), Some(&(greatest, _))) = (pairs.first(), pairs.last()) else {
            return ValueCounts::EMPTY;
        };
        let widths = (
            signed_width(least).max(signed_width(greatest)),
            count_width(pairs),
        );
        let size = widths.0 + widths.1;
        if pairs.len() * size <= INLINE {
            let mut cell = [0; CELL_HEADER + INLINE];
            cell[..CELL_HEADER].copy_from_slice(&[
                byte(pairs.len()),
                byte(widths.0),
                byte(widths.1),
            ]);
            for (i, &(v, n)) in pairs.iter().enumerate() {
                record(&mut cell, CELL_HEADER + i * size, widths, v, n);
            }
            return ValueCounts::Inline(cell);
        }
        let mut runs = BTreeMap::new();
        runs_into(pairs, &mut runs);
        if runs.len() > 1 {
            return ValueCounts::Runs(runs);
        }
        ValueCounts::Run(runs.pop_first().expect("pairs make a run").1)
    }

    /// Counts `n` more of `v`, `n` above zero.
    pub(crate) fn add(&mut self, v: i64, n: u64) {
        match self {
            ValueCounts::Inline(cell) => {
                let held = Records::in_cell(cell);
                let (size, widths) = (held.size(), (held.value_width, held.count_width));
                let end = held.at(held.len);
                match held.place(v, n) {
                    Some(Ok((i, count))) => {
                        let end = held.at(i + 1);
                        write(cell, end, widths.1, count - 1);
                    }
                    Some(Err(i)) if end + size <= cell.len() => {
                        let at = held.at(i);
                        cell.copy_within(at..end, at + size);
                        record(cell, at, widths, v, n);
                        cell[0] += 1;
                    }
                    // Too many or too wide for the cell as it is: encoded
                    // anew they may fit it yet.
                    _ => {
                        let (mut pairs, mut len) = held.few();
                        match held.find(v) {
                            Ok(i) => pairs[i].1 += n,
                            Err(i) => {
                                pairs.copy_within(i..len, i + 1);
                                pairs[i] = (v, n);
                                len += 1;
                            }
                        }
                        *self = ValueCounts::from_pairs(&pairs[..len]);
                    }
                }
            }
            ValueCounts::Run(run) => {
                if !put(run, v, n) {
                    *self = ValueCounts::from_pairs(&Records::of(run).with(v, n));
                }
            }
            ValueCounts::Runs(runs) => {
                let key = key_of(runs, v);
                if !put(runs.get_mut(&key).expect("its key"), v, n) {
                    let run = runs.remove(&key).expect("its key");
                    runs_into(&Records::of(&run).with(v, n), runs);
                }
            }
        }
    }

    /// Takes `n` of `v` back out. False, with nothing taken, when fewer
    /// are held.
    pub(crate) fn sub(&mut self, v: i64, n: u64) -> bool {
        match self {
            ValueCounts::Inline(cell) => {
                let held = Records::in_cell(cell);
                let (size, width, end) = (held.size(), held.count_width, held.at(held.len));
                let Some((i, count)) = (held.find(v).ok())
                    .map(|i| (i, held.count(i)))
                    .filter(|&(_, count)| count >= n)
                else {
                    return false;
                };
                let at = held.at(i);
                if count > n {
                    write(cell, at + size, width, count - n - 1);
                } else if held.len > 1 {
                    cell.copy_within(at + size..end, at);
                    cell[0] -= 1;
                } else {
                    // Empty, the cell forgets the widths its values had.
                    *self = ValueCounts::EMPTY;
                }
            }
            ValueCounts::Run(run) => {
                let Some(last) = take(run, v, n) else {
                    return false;
                };
                if let Some(moved) = last.then(|| settled(run)).flatten() {
                    *self = moved;
                }
            }
            ValueCounts::Runs(runs) => {
                let key = key_of(runs, v);
                let run = runs.get_mut(&key).expect("its key");
                let Some(last) = take(run, v, n) else {
                    return false;
                };
                if run.len() == HEADER {
                    runs.remove(&key);
                }
                // One run stands without a tree.
                if last && runs.len() == 1 {
                    let mut run = runs.pop_first().expect("a run is left").1;
                    *self = settled(&mut run).unwrap_or(ValueCounts::Run(run));
                }
            }
        }
        true
    }

    pub(crate) fn add_counts(&mut self, other: &ValueCounts) {
        for (v, n) in other.iter() {
            self.add(v, n);
        }
    }

    /// Takes out what [`ValueCounts::covers`] found held.
    pub(crate) fn sub_counts(&mut self, other: &ValueCounts) {
        for (v, n) in other.iter() {
            let held = self.sub(v, n);
            assert!(held, "covers checked");
        }
    }

    pub(crate) fn covers(&self, other: &ValueCounts) -> bool {
        other.iter().all(|(v, n)| self.has(v, n))
    }

    fn has(&self, v: i64, n: u64) -> bool {
        let records = match self {
            ValueCounts::Runs(runs) => runs.get(&key_of(runs, v)).map(|run| Records::of(run)),
            _ => self.flat(),
        };
        records.is_some_and(|records| records.find(v).is_ok_and(|i| records.count(i) >= n))
    }

    /// The records of a cell that holds them in itself or in one run.
    fn flat(&self) -> Option<Records<'_>> {
        match self {
            ValueCounts::Inline(cell) => Some(Records::in_cell(cell)),
            ValueCounts::Run(run) => Some(Records::of(run)),
            ValueCounts::Runs(_) => None,
        }
    }

    /// Each value and its count, ascending by value.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (i64, u64)> + '_ {
        let runs = match self {
            ValueCounts::Runs(runs) => Some(runs.values()),
            _ => None,
        };
        (self.flat().into_iter().flat_map(Records::iter))
            .chain((runs.into_iter().flatten()).flat_map(|run| Records::of(run).iter()))
    }

    /// The least and greatest value, `i64::MAX` and `i64::MIN` if none.
    pub(crate) fn ends(&self) -> (i64, i64) {
        let (first, last) = match self {
            ValueCounts::Runs(runs) => (
                runs.values().next().map(|run| Records::of(run)),
                runs.values().next_back().map(|run| Records::of(run)),
            ),
            _ => {
                let records = self.flat();
                (records, records)
            }
        };
        let held = |records: &Records| records.len > 0;
        (
            (first.filter(held)).map_or(i64::MAX, |records| records.value(0)),
            (last.filter(held)).map_or(i64::MIN, |records| records.value(records.len - 1)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// splitmix64; the tests need no crate for randomness.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn pairs_of(held: &BTreeMap<i64, u64>) -> Vec<(i64, u64)> {
        held.iter().map(|(&v, &n)| (v, n)).collect()
    }

    fn ends_of(held: &BTreeMap<i64, u64>) -> (i64, i64) {
        (
            held.keys().next().copied().unwrap_or(i64::MAX),
            held.keys().next_back().copied().unwrap_or(i64::MIN),
        )
    }

    /// The counts are the reference's, and so are they encoded from its
    /// pairs at once.
    fn check(counts: &ValueCounts, held: &BTreeMap<i64, u64>, at: &str) {
        let pairs = pairs_of(held);
        assert!(counts.iter().eq(pairs.iter().copied()), "{at}");
        assert_eq!(counts.ends(), ends_of(held), "{at}");
        let anew = ValueCounts::from_pairs(&pairs);
        assert!(anew.iter().eq(pairs.iter().copied()), "{at}: encoded anew");
        assert!(counts.covers(&anew) && anew.covers(counts), "{at}");
    }

    fn form(counts: &ValueCounts) -> &'static str {
        match counts {
            ValueCounts::Inline(_) => "cell",
            ValueCounts::Run(_) => "run",
            ValueCounts::Runs(_) => "runs",
        }
    }

    /// The values a cell draws from, and the most it counts of one at once.
    struct Shape {
        name: &'static str,
        least: i64,
        step: u64,
        values: u64,
        most: u64,
        /// The forms the cell passes through, in the order it first takes them.
        forms: &'static [&'static str],
    }

    impl Shape {
        fn value(&self, k: u64) -> i64 {
            (self.least).wrapping_add(self.step.wrapping_mul(k).cast_signed())
        }
    }

    const SHAPES: [Shape; 4] = [
        Shape {
            name: "fares",
            least: 250,
            step: 70,
            values: 12_000,
            most: 3,
            forms: &["cell", "run", "runs"],
        },
        Shape {
            name: "cents counted often",
            least: -40,
            step: 1,
            values: 90,
            most: 1 << 20,
            forms: &["cell", "run"],
        },
        Shape {
            name: "every width of value",
            least: i64::MIN,
            step: u64::MAX / 6_000,
            values: 6_000,
            most: 2,
            forms: &["cell", "run", "runs"],
        },
        Shape {
            name: "every width of count",
            least: 0,
            step: 1 << 40,
            values: 3,
            most: 1 << 58,
            forms: &["cell", "run"],
        },
    ];

    /// Adds and removes at random against a map from value to count: the
    /// pairs, the ends and every refusal are the map's, through each form
    /// a cell takes on its way up and back to empty.
    #[test]
    fn counts_follow_a_reference_through_every_form() {
        for shape in &SHAPES {
            let mut rng = Rng(11);
            let mut counts = ValueCounts::EMPTY;
            let mut held: BTreeMap<i64, u64> = BTreeMap::new();
            let mut forms = vec![form(&counts)];
            let grow = shape.values * 3;
            let mut step = 0;
            while step < grow || !held.is_empty() {
                let at = format!("{}, step {step}", shape.name);
                // Mostly adds on the way up, then removes until none is left.
                let adds = if step < grow { 80 } else { 10 };
                let value = shape.value(rng.below(shape.values));
                if rng.below(100) < adds {
                    let n = 1 + rng.below(shape.most);
                    counts.add(value, n);
                    *held.entry(value).or_default() += n;
                } else {
                    let near = held.range(value..).next().or_else(|| held.iter().next());
                    let (v, n) = match (near, rng.below(20)) {
                        // More than is held, and a value that isn't.
                        (Some((&v, &n)), 0) => (v, n + 1),
                        (None, _) | (_, 1) => (value.wrapping_sub(1), 1),
                        (Some((&v, &n)), 2..=7) => (v, n),
                        (Some((&v, &n)), _) => (v, 1 + rng.below(n)),
                    };
                    let has = held.get(&v).is_some_and(|&have| have >= n);
                    assert_eq!(counts.has(v, n), has, "{at}: has {n} of {v}");
                    assert_eq!(counts.sub(v, n), has, "{at}: takes {n} of {v}");
                    if has {
                        let have = held.get_mut(&v).unwrap();
                        *have -= n;
                        if *have == 0 {
                            held.remove(&v);
                        }
                    }
                }
                assert_eq!(counts.ends(), ends_of(&held), "{at}");
                if step % 199 == 0 || step + 1 == grow || held.is_empty() {
                    check(&counts, &held, &at);
                }
                if step < grow && forms.last() != Some(&form(&counts)) {
                    forms.push(form(&counts));
                }
                step += 1;
            }
            forms.dedup();
            assert!(forms.ends_with(shape.forms), "{}: {forms:?}", shape.name);
            assert!(
                matches!(counts, ValueCounts::Inline(cell) if cell[0] == 0),
                "{}: {} when empty",
                shape.name,
                form(&counts)
            );
        }
    }

    /// Values that arrive in order: each new least value goes to the first
    /// run, below its key, and each greatest to the end of the last.
    #[test]
    fn values_in_order_find_their_runs() {
        for descending in [false, true] {
            let mut counts = ValueCounts::EMPTY;
            let mut held = BTreeMap::new();
            for k in 0..20_000_i64 {
                let v = if descending { -k * 1_000 } else { k * 1_000 };
                counts.add(v, 1);
                held.insert(v, 1);
                assert_eq!(counts.ends(), ends_of(&held), "{v}");
            }
            assert_eq!(form(&counts), "runs");
            check(&counts, &held, "in order");
            // Out again from the least: every run's key leaves ahead of its run.
            for (v, n) in pairs_of(&held) {
                assert!(counts.sub(v, n), "{v}");
                held.remove(&v);
                assert_eq!(counts.ends(), ends_of(&held), "{v}");
            }
            assert_eq!(form(&counts), "cell");
        }
    }

    /// A cell's counts fold into another's and come back out, as when a
    /// dim row moves its cells; a cell covers only what it holds.
    #[test]
    fn counts_fold_in_and_come_out_again() {
        for (ours, theirs) in [(3, 2), (300, 40), (9_000, 9_000), (0, 5)] {
            let mut rng = Rng(5);
            let mut draw = |values: u64| {
                let mut held = BTreeMap::new();
                for _ in 0..values {
                    *held
                        .entry(rng.below(20_000).cast_signed() * 70)
                        .or_default() += 1 + rng.below(3);
                }
                held
            };
            let (a, b) = (draw(ours), draw(theirs));
            let mut counts = ValueCounts::from_pairs(&pairs_of(&a));
            let other = ValueCounts::from_pairs(&pairs_of(&b));
            counts.add_counts(&other);
            let mut both = a.clone();
            for (&v, &n) in &b {
                *both.entry(v).or_default() += n;
            }
            check(&counts, &both, "folded in");
            assert!(counts.covers(&other));
            counts.sub_counts(&other);
            check(&counts, &a, "taken out");
            // One more of a value than is left, and a value never held.
            let (&v, &n) = b.iter().next().unwrap();
            let more = a.get(&v).copied().unwrap_or(0) + n + 1;
            assert!(!counts.covers(&ValueCounts::from_pairs(&[(v, more)])));
            assert!(!counts.covers(&ValueCounts::from_pairs(&[(-7, 1)])));
        }
    }

    /// Nothing leaves a cell that holds less, whichever form it has, and
    /// the cell is as it was.
    #[test]
    fn a_cell_gives_only_what_it_holds() {
        for values in [0, 3, 400, 20_000] {
            let pairs: Vec<(i64, u64)> = (0..values).map(|v| (v * 70, 2)).collect();
            let mut counts = ValueCounts::from_pairs(&pairs);
            for (v, n) in [(-70, 1), (35, 1), (values * 70, 1), (0, 3), (70, u64::MAX)] {
                assert!(!counts.has(v, n), "{values} values: has {n} of {v}");
                assert!(!counts.sub(v, n), "{values} values: gave {n} of {v}");
            }
            assert!(counts.iter().eq(pairs.iter().copied()), "{values} values");
        }
    }

    /// Where a cell keeps its values, and in how many bytes.
    #[test]
    fn a_cell_keeps_its_values_in_the_fewest_bytes() {
        let shape = |pairs: &[(i64, u64)]| match ValueCounts::from_pairs(pairs) {
            ValueCounts::Inline(cell) => ("cell", usize::from(cell[0]), cell[1], cell[2]),
            ValueCounts::Run(run) => ("run", run.len(), run[0], run[1]),
            ValueCounts::Runs(runs) => ("runs", runs.len(), 0, 0),
        };
        // Three fares seen once: two bytes each, and none for the counts.
        assert_eq!(shape(&[(1280, 1), (1350, 1), (1420, 1)]), ("cell", 3, 2, 0));
        // A fare seen twice gives every count a byte.
        assert_eq!(shape(&[(1280, 2), (1350, 1), (1420, 1)]), ("cell", 3, 2, 1));
        assert_eq!(shape(&[]), ("cell", 0, 1, 0));
        let once = |values: i64, least: i64, step: i64| -> Vec<(i64, u64)> {
            (0..values).map(|v| (least + v * step, 1)).collect()
        };
        // What a cell holds itself, and one value more.
        assert_eq!(shape(&once(28, 0, 1)), ("cell", 28, 1, 0));
        assert_eq!(shape(&once(29, 0, 1)), ("run", HEADER + 29, 1, 0));
        // A run's values are kept from its middle one, so far from zero
        // they are as narrow as near it.
        assert_eq!(shape(&once(40, 0, 70)), ("run", HEADER + 80, 2, 0));
        assert_eq!(shape(&once(40, 1 << 50, 70)), ("run", HEADER + 80, 2, 0));
        // A run takes RUN_MAX bytes, and one record more makes two.
        let full = (RUN_MAX - HEADER) / 2;
        assert_eq!(shape(&once(full as i64, 0, 5)), ("run", RUN_MAX, 2, 0));
        assert_eq!(shape(&once(full as i64 + 1, 0, 5)), ("runs", 2, 0, 0));
    }

    /// Each width a value or a count takes, at its ends, read back as
    /// written whether encoded at once or added one at a time.
    #[test]
    fn values_and_counts_read_back_at_every_width() {
        for width in 1..=8_u32 {
            let greatest = i64::MAX >> (64 - 8 * width);
            let most = u64::MAX >> (64 - 8 * width);
            for pairs in [
                vec![(greatest, 1)],
                vec![(-greatest - 1, 1)],
                vec![(-greatest - 1, most), (0, 1), (greatest, most)],
                vec![
                    (-greatest - 1, 1),
                    (-1, most),
                    (greatest - 1, 2),
                    (greatest, 1),
                ],
            ] {
                let at = format!("{width} bytes: {pairs:?}");
                let counts = ValueCounts::from_pairs(&pairs);
                assert!(counts.iter().eq(pairs.iter().copied()), "{at}");
                let mut added = ValueCounts::EMPTY;
                for &(v, n) in pairs.iter().rev() {
                    added.add(v, n);
                }
                assert!(added.iter().eq(pairs.iter().copied()), "{at}: added");
                assert_eq!(added.ends(), (pairs[0].0, pairs[pairs.len() - 1].0), "{at}");
            }
        }
    }

    /// A run widens when a value or a count outgrows it, whatever it holds.
    #[test]
    fn a_run_widens_for_a_value_or_a_count_it_cannot_hold() {
        let narrow: Vec<(i64, u64)> = (0..100).map(|v| (v, 1)).collect();
        for (v, n) in [
            (1 << 20, 1),
            (-(1 << 40), 1),
            (i64::MAX, 1),
            (i64::MIN, 1),
            (50, 1),
            (50, 1 << 30),
            (200, u64::MAX),
        ] {
            let mut counts = ValueCounts::from_pairs(&narrow);
            let mut held: BTreeMap<i64, u64> = narrow.iter().copied().collect();
            counts.add(v, n);
            *held.entry(v).or_default() += n;
            check(&counts, &held, &format!("{n} of {v}"));
            assert!(counts.sub(v, n));
            *held.get_mut(&v).unwrap() -= n;
            held.retain(|_, n| *n > 0);
            check(&counts, &held, &format!("{n} of {v} taken"));
        }
    }
}
