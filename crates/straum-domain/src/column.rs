//! A bucket's numbers, one a cell. A column keeps them in as few bytes
//! apiece as its widest takes: an hour's row counts fit two bytes and its
//! sums four, where the widest a count or a sum can be takes eight and
//! sixteen. A number its column can't hold moves the column to the next
//! width that does.

/// Room in a column for one more cell's `each` entries. A `Vec` doubles
/// when full, which leaves a column a third empty on average; an eighth
/// more leaves it a sixteenth.
pub(crate) fn grow<T>(column: &mut Vec<T>, each: usize) {
    if column.len() + each > column.capacity() {
        column.reserve_exact((column.len() / 8).max(4 * each));
    }
}

/// A new cell's entry in a column.
pub(crate) fn put<T>(column: &mut Vec<T>, entry: T) {
    grow(column, 1);
    column.push(entry);
}

/// Entry `i` set to `n`, unless the column's type doesn't hold it.
fn set<T: TryFrom<N>, N>(column: &mut [T], i: usize, n: N) -> bool {
    T::try_from(n).map(|n| column[i] = n).is_ok()
}

/// Entry `i` raised by `n`, unless the column's type doesn't hold the sum.
fn raise<T: Copy + TryFrom<N>, N: From<T> + std::ops::Add<Output = N>>(
    column: &mut [T],
    i: usize,
    n: N,
) -> bool {
    let entry = &mut column[i];
    T::try_from(N::from(*entry) + n)
        .map(|sum| *entry = sum)
        .is_ok()
}

/// Entry `i` lowered by `n`, which is at most the entry.
fn lower<T: Copy + TryFrom<u64>>(column: &mut [T], i: usize, n: u64)
where
    u64: From<T>,
{
    let entry = &mut column[i];
    let left = T::try_from(u64::from(*entry) - n);
    *entry = left.ok().expect("less than it was");
}

/// Numbers a wider column held, in a type that holds them all.
fn held<T: TryFrom<N>, N>(each: impl Iterator<Item = N>) -> Vec<T> {
    each.map(|n| {
        T::try_from(n)
            .ok()
            .expect("the type was picked to hold them")
    })
    .collect()
}

fn walk<T: Copy, N: From<T>, G: Copy>(column: &[T], beside: &[G], mut f: impl FnMut(usize, G, N)) {
    for (i, (&n, &g)) in column.iter().zip(beside).enumerate() {
        f(i, g, n.into());
    }
}

/// As [`walk`], with each cell's entry of a column of counts too: one pass
/// where reading the two apart would take the cells' places twice.
fn walk_both<T: Copy, N: From<T>, C: Copy, G: Copy>(
    column: &[T],
    counts: &[C],
    beside: &[G],
    mut f: impl FnMut(usize, G, N, u64),
) where
    u64: From<C>,
{
    for (i, ((&n, &count), &g)) in column.iter().zip(counts).zip(beside).enumerate() {
        f(i, g, n.into(), count.into());
    }
}

/// A column walked with a column of counts beside it.
fn walk_with<T: Copy, N: From<T>, G: Copy>(
    column: &[T],
    counts: &Counts,
    beside: &[G],
    mut f: impl FnMut(usize, G, N, u64),
) {
    match counts {
        Counts::Zero(_) => walk(column, beside, |i, g, n| f(i, g, n, 0)),
        Counts::U8(counts) => walk_both(column, counts, beside, f),
        Counts::U16(counts) => walk_both(column, counts, beside, f),
        Counts::U32(counts) => walk_both(column, counts, beside, f),
        Counts::U64(counts) => walk_both(column, counts, beside, f),
    }
}

/// Counts, one a cell: of its rows, or of the nulls or the trues among
/// them. None is kept while every count is zero, as the nulls of most
/// columns are.
#[derive(Debug)]
pub(crate) enum Counts {
    Zero(usize),
    U8(Vec<u8>),
    U16(Vec<u16>),
    U32(Vec<u32>),
    U64(Vec<u64>),
}

impl Counts {
    /// `len` counts in the narrowest type that holds `most`.
    fn holding(most: u64, len: usize, each: impl Iterator<Item = u64>) -> Counts {
        match most {
            0 => Counts::Zero(len),
            1..=0xff => Counts::U8(held(each)),
            0x100..=0xffff => Counts::U16(held(each)),
            0x1_0000..=0xffff_ffff => Counts::U32(held(each)),
            _ => Counts::U64(each.collect()),
        }
    }

    pub(crate) fn of(counts: &[u64]) -> Counts {
        let most = counts.iter().copied().max().unwrap_or(0);
        Counts::holding(most, counts.len(), counts.iter().copied())
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Counts::Zero(len) => *len,
            Counts::U8(column) => column.len(),
            Counts::U16(column) => column.len(),
            Counts::U32(column) => column.len(),
            Counts::U64(column) => column.len(),
        }
    }

    pub(crate) fn get(&self, i: usize) -> u64 {
        match self {
            Counts::Zero(_) => 0,
            Counts::U8(column) => column[i].into(),
            Counts::U16(column) => column[i].into(),
            Counts::U32(column) => column[i].into(),
            Counts::U64(column) => column[i],
        }
    }

    pub(crate) fn set(&mut self, i: usize, n: u64) {
        let set = match self {
            Counts::Zero(_) => n == 0,
            Counts::U8(column) => set(column, i, n),
            Counts::U16(column) => set(column, i, n),
            Counts::U32(column) => set(column, i, n),
            Counts::U64(column) => set(column, i, n),
        };
        if !set {
            *self = Counts::holding(n, self.len(), self.iter());
            self.set(i, n);
        }
    }

    pub(crate) fn add(&mut self, i: usize, n: u64) {
        let raised = match self {
            Counts::Zero(_) => n == 0,
            Counts::U8(column) => raise(column, i, n),
            Counts::U16(column) => raise(column, i, n),
            Counts::U32(column) => raise(column, i, n),
            Counts::U64(column) => raise(column, i, n),
        };
        if !raised {
            self.set(i, self.get(i) + n);
        }
    }

    /// `n` is at most the count.
    pub(crate) fn sub(&mut self, i: usize, n: u64) {
        match self {
            Counts::Zero(_) => assert_eq!(n, 0, "more than the count"),
            Counts::U8(column) => lower(column, i, n),
            Counts::U16(column) => lower(column, i, n),
            Counts::U32(column) => lower(column, i, n),
            Counts::U64(column) => lower(column, i, n),
        }
    }

    /// A new cell's count, zero.
    pub(crate) fn push(&mut self) {
        match self {
            Counts::Zero(len) => *len += 1,
            Counts::U8(column) => put(column, 0),
            Counts::U16(column) => put(column, 0),
            Counts::U32(column) => put(column, 0),
            Counts::U64(column) => put(column, 0),
        }
    }

    pub(crate) fn swap_remove(&mut self, i: usize) {
        match self {
            Counts::Zero(len) => *len -= 1,
            Counts::U8(column) => drop(column.swap_remove(i)),
            Counts::U16(column) => drop(column.swap_remove(i)),
            Counts::U32(column) => drop(column.swap_remove(i)),
            Counts::U64(column) => drop(column.swap_remove(i)),
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = u64> + '_ {
        (0..self.len()).map(|i| self.get(i))
    }

    pub(crate) fn total(&self) -> u64 {
        match self {
            Counts::Zero(_) => 0,
            Counts::U8(column) => column.iter().map(|&n| u64::from(n)).sum(),
            Counts::U16(column) => column.iter().map(|&n| u64::from(n)).sum(),
            Counts::U32(column) => column.iter().map(|&n| u64::from(n)).sum(),
            Counts::U64(column) => column.iter().sum(),
        }
    }

    /// Calls `f` with each cell's place, its entry of `beside` and its
    /// count. Not once for a column of zeros: every caller adds what it
    /// is given.
    pub(crate) fn each<G: Copy>(&self, beside: &[G], f: impl FnMut(usize, G, u64)) {
        match self {
            Counts::Zero(_) => {}
            Counts::U8(column) => walk(column, beside, f),
            Counts::U16(column) => walk(column, beside, f),
            Counts::U32(column) => walk(column, beside, f),
            Counts::U64(column) => walk(column, beside, f),
        }
    }
}

/// Sums, one a cell.
#[derive(Debug)]
pub(crate) enum Sums {
    I32(Vec<i32>),
    I64(Vec<i64>),
    I128(Vec<i128>),
}

impl Sums {
    pub(crate) const fn new() -> Sums {
        Sums::I32(Vec::new())
    }

    /// Sums in the narrowest type that holds `least` and `greatest`.
    fn holding(least: i128, greatest: i128, each: impl Iterator<Item = i128>) -> Sums {
        let holds = |low: i128, high: i128| low <= least && greatest <= high;
        if holds(i32::MIN.into(), i32::MAX.into()) {
            Sums::I32(held(each))
        } else if holds(i64::MIN.into(), i64::MAX.into()) {
            Sums::I64(held(each))
        } else {
            Sums::I128(each.collect())
        }
    }

    pub(crate) fn of(sums: &[i128]) -> Sums {
        let least = sums.iter().copied().min().unwrap_or(0);
        let greatest = sums.iter().copied().max().unwrap_or(0);
        Sums::holding(least, greatest, sums.iter().copied())
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Sums::I32(column) => column.len(),
            Sums::I64(column) => column.len(),
            Sums::I128(column) => column.len(),
        }
    }

    pub(crate) fn get(&self, i: usize) -> i128 {
        match self {
            Sums::I32(column) => column[i].into(),
            Sums::I64(column) => column[i].into(),
            Sums::I128(column) => column[i],
        }
    }

    pub(crate) fn add(&mut self, i: usize, n: i128) {
        let raised = match self {
            Sums::I32(column) => raise(column, i, n),
            Sums::I64(column) => raise(column, i, n),
            Sums::I128(column) => raise(column, i, n),
        };
        if !raised {
            // The others fit a narrower type, so the sum is the end to hold.
            let sum = self.get(i) + n;
            *self = Sums::holding(sum.min(0), sum.max(0), self.iter());
            self.add(i, n);
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

    pub(crate) fn swap_remove(&mut self, i: usize) {
        match self {
            Sums::I32(column) => drop(column.swap_remove(i)),
            Sums::I64(column) => drop(column.swap_remove(i)),
            Sums::I128(column) => drop(column.swap_remove(i)),
        }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = i128> + '_ {
        (0..self.len()).map(|i| self.get(i))
    }

    pub(crate) fn total(&self) -> i128 {
        match self {
            Sums::I32(column) => column.iter().map(|&n| i128::from(n)).sum(),
            Sums::I64(column) => column.iter().map(|&n| i128::from(n)).sum(),
            Sums::I128(column) => column.iter().sum(),
        }
    }

    /// Calls `f` with each cell's place, its entry of `beside`, its sum
    /// and its count among `counts`.
    pub(crate) fn each<G: Copy>(
        &self,
        counts: &Counts,
        beside: &[G],
        f: impl FnMut(usize, G, i128, u64),
    ) {
        match self {
            Sums::I32(column) => walk_with(column, counts, beside, f),
            Sums::I64(column) => walk_with(column, counts, beside, f),
            Sums::I128(column) => walk_with(column, counts, beside, f),
        }
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

    /// What a column of counts read beside `beside` gives each cell.
    fn read(counts: &Counts, beside: &[u8]) -> Vec<(usize, u8, u64)> {
        let mut seen = Vec::new();
        counts.each(beside, |i, g, n| seen.push((i, g, n)));
        seen
    }

    /// Cells come and go and their counts rise and fall, against a plain
    /// list: every count read is the list's, one at a time and all at
    /// once, and the column is as wide as the widest count it has held.
    #[test]
    fn counts_follow_a_list_at_every_width() {
        for most in [1, 200, 60_000, 4_000_000_000, 1 << 50] {
            let mut rng = Rng(most);
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
                    (3, len) => {
                        let i = rng.below(len as u64) as usize;
                        counts.swap_remove(i);
                        list.swap_remove(i);
                    }
                    (4..=7, len) => {
                        let (i, n) = (rng.below(len as u64) as usize, rng.below(most));
                        counts.add(i, n);
                        list[i] += n;
                    }
                    (_, len) => {
                        let i = rng.below(len as u64) as usize;
                        let n = rng.below(list[i] + 1);
                        counts.sub(i, n);
                        list[i] -= n;
                    }
                }
                widest = widest.max(list.iter().copied().max().unwrap_or(0));
                assert_eq!(counts.len(), list.len(), "{at}");
                assert_eq!(width(&counts), width(&Counts::of(&[widest])), "{at}");
                if step % 97 == 0 {
                    assert!(counts.iter().eq(list.iter().copied()), "{at}");
                    assert_eq!(counts.total(), list.iter().sum::<u64>(), "{at}");
                    let beside: Vec<u8> = (0..list.len()).map(|i| i as u8).collect();
                    let all: Vec<_> = (list.iter().enumerate())
                        .map(|(i, &n)| (i, i as u8, n))
                        .collect();
                    // A column of zeros is passed over whole.
                    let expected = if widest == 0 { Vec::new() } else { all };
                    assert_eq!(read(&counts, &beside), expected, "{at}");
                    let anew = Counts::of(&list);
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
            assert_eq!(width(&Counts::of(&[0, count, 0])), bytes, "{count}");
            let mut counts = Counts::of(&[0, 0, 0]);
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
            let mut rng = Rng(7);
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
                    (3, len) => {
                        let i = rng.below(len as u64) as usize;
                        sums.swap_remove(i);
                        list.swap_remove(i);
                    }
                    (_, len) => {
                        let i = rng.below(len as u64) as usize;
                        let n = i128::from(rng.next()) % (2 * most + 1) - most;
                        sums.add(i, n);
                        list[i] += n;
                        (least, greatest) = (least.min(list[i]), greatest.max(list[i]));
                    }
                }
                assert_eq!(sums.len(), list.len(), "{at}");
                assert_eq!(
                    sum_width(&sums),
                    sum_width(&Sums::of(&[least, greatest])),
                    "{at}"
                );
                if step % 97 == 0 {
                    assert!(sums.iter().eq(list.iter().copied()), "{at}");
                    assert_eq!(sums.total(), list.iter().sum::<i128>(), "{at}");
                    let beside: Vec<u8> = (0..list.len()).map(|i| i as u8).collect();
                    for counts in [0, 9, 300, 70_000, 5_000_000_000] {
                        let rows = Counts::of(&vec![counts; list.len()]);
                        let mut seen = Vec::new();
                        sums.each(&rows, &beside, |i, g, sum, n| seen.push((i, g, sum, n)));
                        let all: Vec<_> = (list.iter().enumerate())
                            .map(|(i, &sum)| (i, i as u8, sum, counts))
                            .collect();
                        assert_eq!(seen, all, "{at}, beside {counts}");
                    }
                    assert!(Sums::of(&list).iter().eq(list.iter().copied()), "{at}");
                }
            }
        }
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
            assert_eq!(sum_width(&Sums::of(&[1, sum, -1])), bytes, "{sum}");
            let mut sums = Sums::of(&[1, 0, -1]);
            sums.add(1, sum);
            assert_eq!(sum_width(&sums), bytes, "{sum} added");
            assert!(sums.iter().eq([1, sum, -1]), "{sum} added");
        }
    }

    /// A column grows by an eighth, not by doubling.
    #[test]
    fn a_column_grows_by_an_eighth() {
        let mut column: Vec<u64> = Vec::new();
        for n in 0..10_000 {
            put(&mut column, n);
            assert!(column.capacity() <= column.len() + (column.len() / 8).max(4));
        }
        assert!(column.iter().copied().eq(0..10_000));
    }
}
