use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::{iter, mem, panic, thread};

use crate::aggregate::{Aggregate, Reads};
use crate::codes::Codes;
use crate::hash::FastMap;
use crate::ident::Identifier;
use crate::sealed::{Sealed, SealedColumn, SealedError, SealedId, Thawed};
use crate::spec::{Fact, KeyPart};
use crate::value::{ColumnType, Datum};

const TIME: usize = 0;

const COLUMNS: &str = "Fact::columns lists every measure input";

/// One cell per rollup key, holding what the fact's measures need. Cells
/// are kept per bucket, so an ask reads only the buckets in its range, and
/// once more per wider bucket width the fact declares, so a long range
/// reads few cells.
#[derive(Debug)]
pub(crate) struct Rollup {
    /// The columns a row is read from, as [`Fact::columns`].
    columns: Vec<Identifier>,
    /// What each rollup-key part codes of a row, in [`Fact::rollup_key`]
    /// order without the bucket.
    key_columns: Vec<Coded>,
    inputs: Vec<Input>,
    /// One per rollup-key column: its values under small codes, so a cell
    /// key is a few integers.
    codes: Vec<Codes>,
    /// The `date_bin` width first, then the wider ones.
    levels: Vec<Level>,
    /// Cells at the `date_bin` width, sealed ones included.
    cells: usize,
    /// Sealed buckets, every width.
    sealed: usize,
    /// The codes of the row [`Rollup::add`] is adding, kept to spare it an
    /// allocation per row.
    key: Vec<u32>,
    types: Vec<ColumnType>,
}

/// What each part of a key codes of a row of `columns`, the bucket aside.
fn coded<'a>(columns: &[Identifier], parts: impl Iterator<Item = KeyPart<'a>>) -> Vec<Coded> {
    let position = |name: &Identifier| columns.iter().position(|c| c == name).expect(COLUMNS);
    parts
        .filter_map(|part| match part {
            KeyPart::Column(c) => Some(Coded::Value(position(c))),
            KeyPart::IsNull { column, negated } => Some(Coded::IsNull {
                column: position(column),
                negated,
            }),
            KeyPart::Bucket => None,
        })
        .collect()
}

/// What a rollup-key part codes of a row.
#[derive(Debug, Clone, Copy)]
enum Coded {
    /// The column's value.
    Value(usize),
    /// Whether the column is null, or not null when `negated`.
    IsNull { column: usize, negated: bool },
}

impl Coded {
    fn datum<'r>(self, row: &'r [Datum]) -> Cow<'r, Datum> {
        match self {
            Coded::Value(column) => Cow::Borrowed(&row[column]),
            Coded::IsNull { column, negated } => {
                Cow::Owned(Datum::Bool((row[column] == Datum::Null) != negated))
            }
        }
    }

    fn column_type(self, types: &[ColumnType]) -> ColumnType {
        match self {
            Coded::Value(column) => types[column],
            Coded::IsNull { .. } => ColumnType::Bool,
        }
    }
}

#[derive(Debug)]
struct Level {
    /// Bucket width in seconds.
    width: i64,
    /// Oldest first. `None` holds the rows without a time, which no ask reads.
    buckets: BTreeMap<Option<i64>, Bucket>,
}

#[derive(Debug)]
pub(crate) enum Bucket {
    Hot(Cells),
    /// Spilled, holding this many cells.
    Sealed {
        cells: usize,
    },
}

impl Bucket {
    pub(crate) fn len(&self) -> usize {
        match self {
            Bucket::Hot(cells) => cells.len(),
            Bucket::Sealed { cells } => *cells,
        }
    }

    /// The cells, unless they are sealed.
    pub(crate) fn cells(&self) -> Option<&Cells> {
        match self {
            Bucket::Hot(cells) => Some(cells),
            Bucket::Sealed { .. } => None,
        }
    }
}

impl Level {
    fn bucket(&self, time: &Datum) -> Option<i64> {
        match *time {
            Datum::Int(t) => Some(t.div_euclid(self.width) * self.width),
            _ => None,
        }
    }

    /// Adds a checked row under its key codes; whether its cell is new.
    /// [`Rollup::check`] turns away the rows of a sealed bucket, so none
    /// reaches one.
    fn add(&mut self, row: &[Datum], key: &[u32], inputs: &[Input]) -> bool {
        let Some((cells, i, new)) = self.cell(&row[TIME], key, inputs) else {
            return false;
        };
        cells.counts[i] += 1;
        for (column, input) in cells.states.iter_mut().zip(inputs) {
            column.add(i, &row[input.column]);
        }
        new
    }

    /// The cell `key` names in the bucket `time` falls in, made empty if
    /// it's new, and whether it is; none if the bucket is sealed.
    fn cell(
        &mut self,
        time: &Datum,
        key: &[u32],
        inputs: &[Input],
    ) -> Option<(&mut Cells, usize, bool)> {
        let bucket = self.bucket(time);
        let bucket = (self.buckets)
            .entry(bucket)
            .or_insert_with(|| Bucket::Hot(Cells::new(key.len(), inputs)));
        let Bucket::Hot(cells) = bucket else {
            return None;
        };
        let (i, new) = cells.entry(key);
        Some((cells, i, new))
    }
}

/// The cells of one bucket, side by side: cell `i` has key
/// `keys[i * key_len..][..key_len]`, `rows[i]` rows and entry `i` of each
/// column, so an ask reads each measure's states as one flat run.
#[derive(Debug)]
pub(crate) struct Cells {
    key_len: usize,
    index: FastMap<Box<[u32]>, usize>,
    keys: Vec<u32>,
    counts: Vec<u64>,
    /// One per input, in [`Rollup`] input order.
    states: Vec<State>,
}

impl Cells {
    fn new(key_len: usize, inputs: &[Input]) -> Cells {
        Cells {
            key_len,
            index: FastMap::default(),
            keys: Vec::new(),
            counts: Vec::new(),
            states: inputs.iter().map(|i| State::new(i.kind)).collect(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.counts.len()
    }

    pub(crate) fn key(&self, i: usize) -> &[u32] {
        &self.keys[i * self.key_len..][..self.key_len]
    }

    pub(crate) fn code(&self, i: usize, part: usize) -> u32 {
        self.keys[i * self.key_len + part]
    }

    pub(crate) fn counts(&self) -> &[u64] {
        &self.counts
    }

    pub(crate) fn state(&self, input: usize) -> &State {
        &self.states[input]
    }

    pub(crate) fn states(&self) -> &[State] {
        &self.states
    }

    /// The position of the cell with `key`, made empty if it's new.
    /// Folds `src`'s cell `src_i` into cell `i`. False when the columns differ.
    fn absorb(&mut self, i: usize, count: u64, src: &[State], src_i: usize) -> bool {
        if self.states.len() != src.len()
            || self
                .states
                .iter()
                .zip(src)
                .any(|(dest, src)| !dest.same_kind(src))
        {
            return false;
        }
        self.counts[i] += count;
        for (dest, src) in self.states.iter_mut().zip(src) {
            dest.add_from(i, src, src_i);
        }
        true
    }

    /// Takes `src`'s cell `src_i` back out of cell `i`. False when it was
    /// never there, and the cell is left as it was.
    fn release(&mut self, i: usize, count: u64, src: &[State], src_i: usize) -> bool {
        if self.counts[i] < count || src.len() != self.states.len() {
            return false;
        }
        if src
            .iter()
            .zip(&self.states)
            .any(|(src, dest)| !dest.covers(i, src, src_i))
        {
            return false;
        }
        self.counts[i] -= count;
        for (dest, src) in self.states.iter_mut().zip(src) {
            dest.sub_from(i, src, src_i);
        }
        true
    }

    fn entry(&mut self, key: &[u32]) -> (usize, bool) {
        if let Some(&i) = self.index.get(key) {
            return (i, false);
        }
        let i = self.len();
        self.index.insert(key.into(), i);
        self.keys.extend_from_slice(key);
        self.counts.push(0);
        self.states.iter_mut().for_each(State::push);
        (i, true)
    }

    fn swap_remove(&mut self, i: usize) {
        let last = self.len() - 1;
        let key_len = self.key_len;
        let key = |n: usize| n * key_len..(n + 1) * key_len;
        self.index.remove(&self.keys[key(i)]);
        if i != last {
            *self
                .index
                .get_mut(&self.keys[key(last)])
                .expect("every cell is indexed") = i;
            self.keys.copy_within(key(last), i * key_len);
        }
        self.keys.truncate(last * key_len);
        self.counts.swap_remove(i);
        self.states.iter_mut().for_each(|c| c.swap_remove(i));
    }
}

/// A column whose values a cell keeps, and in what form. Measures share
/// inputs: sum and avg read one `Sum`, min and max one `Extent`. Filtered
/// measures keep nothing of their own, since their conditions name rollup-key
/// parts only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Input {
    kind: StateKind,
    column: usize,
}

/// A form of [`State`], which [`Aggregate::state`] picks per measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StateKind {
    Count,
    Sum,
    Extent,
    Distinct,
    Bools,
}

impl StateKind {
    pub(crate) fn reads(self) -> Reads {
        match self {
            StateKind::Count => Reads::Nullness,
            StateKind::Sum | StateKind::Extent => Reads::Numbers,
            StateKind::Distinct => Reads::Values,
            StateKind::Bools => Reads::Booleans,
        }
    }
}

#[derive(Debug)]
pub(crate) enum State {
    /// Count of the non-null values.
    Count(Vec<u64>),
    /// Sum and count of the non-null values.
    Sum { sums: Vec<i128>, counts: Vec<u64> },
    /// Count per value, so removing the min or max reveals the next one.
    /// `mins` and `maxs` mirror each cell's ends, so an ask reads them
    /// without walking a tree. An empty cell's are `i64::MAX` and
    /// `i64::MIN`, which any min or max passes over: a merge needs no test
    /// for empty.
    Extent {
        mins: Vec<i64>,
        maxs: Vec<i64>,
        counts: Vec<ValueCounts>,
    },
    /// Count per value.
    Distinct(Vec<FastMap<Datum, u64>>),
    /// Counts of true and of false, so removing the last false turns a
    /// bool_and true.
    Bools { trues: Vec<u64>, falses: Vec<u64> },
}

impl Rollup {
    /// Keyed as [`Fact::rollup_key`]: the cells an ask reads through the
    /// projections.
    pub(crate) fn new(fact: &Fact) -> Rollup {
        let columns = fact.columns();
        let key_columns = coded(&columns, fact.rollup_key());
        Rollup::build(fact, columns, key_columns)
    }

    /// Keyed by the `by` keys and every column the refs read, which a row
    /// carries after the fact's columns, so an ask reads no projection
    /// (ADR-0038). A column read is named as the view names it; a fact
    /// column of that name comes first, so the measure inputs find theirs.
    pub(crate) fn bound(fact: &Fact) -> Rollup {
        let mut columns = fact.columns();
        let mut key_columns = coded(&columns, fact.by_parts());
        for (_, attribute) in fact.attributes() {
            key_columns.push(Coded::Value(columns.len()));
            columns.push(attribute.name.clone());
        }
        Rollup::build(fact, columns, key_columns)
    }

    fn build(fact: &Fact, columns: Vec<Identifier>, key_columns: Vec<Coded>) -> Rollup {
        let position = |name: &Identifier| columns.iter().position(|c| c == name).expect(COLUMNS);
        let mut inputs = Vec::new();
        for m in &fact.measures {
            let Some((kind, name)) = m.aggregate.state() else {
                continue;
            };
            let input = Input {
                kind,
                column: position(name),
            };
            if !inputs.contains(&input) {
                inputs.push(input);
            }
        }
        let levels = iter::once(fact.time.width)
            .chain(fact.time.wider.iter().copied())
            .map(|width| Level {
                width: i64::from(width.seconds),
                buckets: BTreeMap::new(),
            })
            .collect();
        Rollup {
            levels,
            codes: key_columns.iter().map(|_| Codes::default()).collect(),
            key_columns,
            inputs,
            cells: 0,
            sealed: 0,
            key: Vec::new(),
            types: vec![ColumnType::Int; columns.len()],
            columns,
        }
    }

    /// Columns a row holds, as [`Fact::columns`].
    pub(crate) fn row_len(&self) -> usize {
        self.columns.len()
    }

    /// At the `date_bin` width; a cell goes when its last row is removed.
    pub(crate) fn cells(&self) -> usize {
        self.cells
    }

    /// From the start of the first `date_bin` bucket to the end of the
    /// last, sealed ones included. Rows without a time are in no bucket
    /// an ask reads, so they count for nothing here.
    pub(crate) fn range(&self) -> Option<Range<i64>> {
        let level = &self.levels[0];
        let mut starts = level.buckets.keys().flatten();
        let first = *starts.next()?;
        let last = starts.next_back().map_or(first, |&last| last);
        Some(first..last + level.width)
    }

    /// The types stay.
    pub(crate) fn clear(&mut self) {
        self.levels.iter_mut().for_each(|l| l.buckets.clear());
        self.codes.iter_mut().for_each(|c| *c = Codes::default());
        self.cells = 0;
        self.sealed = 0;
    }

    /// Only the source knows the row's types, so a load sets them; they are
    /// integers until then.
    pub(crate) fn set_types(&mut self, types: &[ColumnType]) -> Result<(), RowError> {
        if types.len() != self.types.len() {
            return Err(RowError::Arity {
                expected: self.types.len(),
                found: types.len(),
            });
        }
        self.types.copy_from_slice(types);
        Ok(())
    }

    pub(crate) fn key_type(&self, part: usize) -> ColumnType {
        self.key_columns[part].column_type(&self.types)
    }

    pub(crate) fn types(&self) -> &[ColumnType] {
        &self.types
    }

    pub(crate) fn time_type(&self) -> ColumnType {
        self.types[TIME]
    }

    /// A measure over numbers keeps its column's scale; a count has none.
    pub(crate) fn scale_of(&self, aggregate: &Aggregate) -> u32 {
        match aggregate.state() {
            Some((kind, c)) if kind.reads() == Reads::Numbers => {
                self.types[self.column(c).expect(COLUMNS)].scale()
            }
            _ => 0,
        }
    }

    pub(crate) fn add(&mut self, row: &[Datum]) -> Result<(), RowError> {
        self.check(row)?;
        let mut key = mem::take(&mut self.key);
        key.clear();
        self.code(row, &mut key);
        for (n, level) in self.levels.iter_mut().enumerate() {
            let new = level.add(row, &key, &self.inputs);
            if n == 0 {
                self.cells += usize::from(new);
            }
        }
        self.key = key;
        Ok(())
    }

    /// Adds whole rows laid end to end, as a load reads them: each width
    /// of cells on its own thread, since they share nothing but the rows.
    /// If a row fails [`Rollup::add`]'s checks, none is added.
    pub(crate) fn add_rows(&mut self, rows: &[Datum]) -> Result<(), RowError> {
        let row_len = self.row_len();
        if !rows.len().is_multiple_of(row_len) {
            return Err(RowError::Arity {
                expected: row_len,
                found: rows.len() % row_len,
            });
        }
        let rows: Vec<&[Datum]> = rows.chunks_exact(row_len).collect();
        for row in &rows {
            self.check(row)?;
        }
        let mut keys = mem::take(&mut self.key);
        keys.clear();
        for row in &rows {
            self.code(row, &mut keys);
        }
        let (key_len, inputs) = (self.key_columns.len(), &self.inputs);
        let add = |level: &mut Level| {
            (rows.iter().enumerate())
                .map(|(r, row)| {
                    let key = &keys[r * key_len..][..key_len];
                    usize::from(level.add(row, key, inputs))
                })
                .sum::<usize>()
        };
        let new = thread::scope(|scope| {
            let (first, wider) = self
                .levels
                .split_first_mut()
                .expect("a rollup has its width");
            let threads: Vec<_> = wider.iter_mut().map(|l| scope.spawn(|| add(l))).collect();
            let new = add(first);
            for thread in threads {
                thread.join().unwrap_or_else(|e| panic::resume_unwind(e));
            }
            new
        });
        self.cells += new;
        self.key = keys;
        Ok(())
    }

    /// After [`RowError::Missing`] the rollup no longer matches the table;
    /// start over.
    pub(crate) fn remove(&mut self, row: &[Datum]) -> Result<(), RowError> {
        self.check(row)?;
        let mut key = mem::take(&mut self.key);
        key.clear();
        let coded = (self.key_columns.iter().zip(&self.codes)).try_for_each(|(&coded, codes)| {
            key.push(codes.get(&coded.datum(row))?);
            Some(())
        });
        let removed = match coded {
            Some(()) => self.remove_coded(row, &key),
            None => Err(RowError::Missing),
        };
        self.key = key;
        removed
    }

    fn remove_coded(&mut self, row: &[Datum], key: &[u32]) -> Result<(), RowError> {
        for (n, level) in self.levels.iter_mut().enumerate() {
            let bucket = level.bucket(&row[TIME]);
            let cells = match level.buckets.get_mut(&bucket) {
                Some(Bucket::Hot(cells)) => cells,
                _ => return Err(RowError::Missing),
            };
            let i = *cells.index.get(key).ok_or(RowError::Missing)?;
            for (column, input) in cells.states.iter_mut().zip(&self.inputs) {
                column.remove(i, &row[input.column])?;
            }
            cells.counts[i] -= 1;
            if cells.counts[i] == 0 {
                cells.swap_remove(i);
                if n == 0 {
                    self.cells -= 1;
                }
                if cells.len() == 0 {
                    level.buckets.remove(&bucket);
                }
            }
        }
        Ok(())
    }

    /// Moves one keyed cell's aggregates into or out of the bound cell
    /// `key` names, at every width.
    pub(crate) fn adjust(
        &mut self,
        time: Option<i64>,
        key: &[u32],
        count: u64,
        src: &[State],
        src_i: usize,
        add: bool,
    ) -> Result<(), RowError> {
        let time = match time {
            Some(time) => Datum::Int(time),
            None => Datum::Null,
        };
        let widths = self.levels.len();
        let move_cell = MoveCell {
            key,
            count,
            src,
            src_i,
            add,
        };
        for n in 0..widths {
            let shift = adjust_level(&mut self.levels[n], &self.inputs, &time, move_cell)?;
            if n == 0 {
                match shift {
                    Shift::New => self.cells += 1,
                    Shift::Removed => self.cells -= 1,
                    Shift::Same => {}
                }
            }
        }
        Ok(())
    }

    pub(crate) fn code_part(&mut self, part: usize, datum: &Datum) -> u32 {
        self.codes[part].code(datum)
    }

    /// The rollup-key part that codes the fact column, when one does.
    pub(crate) fn part_of_column(&self, column: usize) -> Option<usize> {
        (self.key_columns.iter()).position(|coded| match coded {
            Coded::Value(c) => *c == column,
            Coded::IsNull { .. } => false,
        })
    }

    pub(crate) fn date_bin_width(&self) -> i64 {
        self.levels[0].width
    }

    /// Hot `date_bin` buckets, oldest first. `None` is the null-time
    /// bucket: no ask reads it, and a dim change still has to move it.
    pub(crate) fn hot_date_bin_starts(&self) -> Vec<Option<i64>> {
        let level = &self.levels[0];
        (level.buckets.iter())
            .filter_map(|(&start, bucket)| match bucket {
                Bucket::Hot(_) => Some(start),
                Bucket::Sealed { .. } => None,
            })
            .collect()
    }

    pub(crate) fn hot_date_bin(&self, start: Option<i64>) -> Option<&Cells> {
        self.levels[0].buckets.get(&start).and_then(Bucket::cells)
    }

    /// A sealed `date_bin` bucket overlaps `[from, to)`. Asks are aligned
    /// to that width, so a bucket in the range starts inside it.
    pub(crate) fn overlaps_sealed(&self, from: i64, to: i64) -> bool {
        let level = &self.levels[0];
        (level.buckets.range(Some(from)..Some(to)))
            .any(|(_, bucket)| matches!(bucket, Bucket::Sealed { .. }))
    }

    pub(crate) fn state_of(&self, aggregate: &Aggregate) -> Option<usize> {
        let (kind, name) = aggregate.state()?;
        let column = self.column(name)?;
        self.inputs
            .iter()
            .position(|i| *i == Input { kind, column })
    }

    /// Buckets that cover `from..to` once, in time order, each with its
    /// width and start: the widest that fit, narrower ones at the ends. With
    /// `each_bucket`, as an ask grouped by the bucket needs, only `date_bin`
    /// buckets. `from` and `to` are on `date_bin` edges.
    pub(crate) fn cover(
        &self,
        from: i64,
        to: i64,
        each_bucket: bool,
    ) -> impl Iterator<Item = (i64, i64, &Bucket)> {
        let widest = if each_bucket {
            0
        } else {
            self.levels.len() - 1
        };
        let mut runs = Vec::new();
        self.runs(widest, from, to, &mut runs);
        runs.into_iter().flat_map(move |(level, from, to)| {
            let width = self.levels[level].width;
            (self.levels[level].buckets.range(Some(from)..Some(to)))
                .filter_map(move |(&start, bucket)| Some((width, start?, bucket)))
        })
    }

    /// Splits `from..to` into runs of whole buckets of `level` and narrower,
    /// in time order: the run of `level` buckets inside it, then the ends.
    fn runs(&self, level: usize, from: i64, to: i64, runs: &mut Vec<(usize, i64, i64)>) {
        if from >= to {
            return;
        }
        if level == 0 {
            runs.push((0, from, to));
            return;
        }
        let width = self.levels[level].width;
        let first = -(-from).div_euclid(width) * width;
        let last = to.div_euclid(width) * width;
        if first >= last {
            self.runs(level - 1, from, to, runs);
            return;
        }
        self.runs(level - 1, from, first, runs);
        runs.push((level, first, last));
        self.runs(level - 1, last, to, runs);
    }

    /// Width and start of each `date_bin` bucket not sealed that ends by
    /// `before`. Wider buckets stay in memory: an ask over a long range
    /// reads them, and the narrow ones only at its ends (ADR-0035).
    pub(crate) fn sealable(&self, before: i64) -> impl Iterator<Item = (i64, i64)> {
        let level = &self.levels[0];
        (level.buckets.range(..Some(before))).filter_map(move |(&start, bucket)| {
            let start = start?;
            let ended = start.checked_add(level.width)? <= before;
            (ended && bucket.cells().is_some()).then_some((level.width, start))
        })
    }

    /// A copy of a bucket not sealed.
    pub(crate) fn sealed_copy(&self, id: &SealedId) -> Option<Sealed> {
        let cells = self
            .level(id.width)?
            .buckets
            .get(&Some(id.bucket_start))?
            .cells()?;
        Some(Sealed {
            id: id.clone(),
            keys: cells.keys.clone(),
            counts: cells.counts.clone(),
            columns: cells.states.iter().map(State::sealed).collect(),
        })
    }

    /// Drops a bucket's cells and keeps its place.
    pub(crate) fn seal(&mut self, width: i64, start: i64) -> Result<(), &'static str> {
        let level = (self.levels.iter_mut())
            .find(|l| l.width == width)
            .ok_or("no such width")?;
        let bucket = level
            .buckets
            .get_mut(&Some(start))
            .ok_or("no such bucket")?;
        if let Bucket::Sealed { .. } = bucket {
            return Err("sealed already");
        }
        *bucket = Bucket::Sealed {
            cells: bucket.len(),
        };
        self.sealed += 1;
        Ok(())
    }

    /// Width and start of every sealed bucket.
    pub(crate) fn sealed(&self) -> impl Iterator<Item = (i64, i64)> {
        // Most rollups have none sealed, so none is walked.
        (self.levels.iter().filter(|_| self.sealed > 0)).flat_map(|level| {
            (level.buckets.iter())
                .filter(|(_, bucket)| matches!(bucket, Bucket::Sealed { .. }))
                .filter_map(|(&start, _)| Some((level.width, start?)))
        })
    }

    /// Width and start of each sealed bucket a row of time `time` falls in.
    pub(crate) fn touched(&self, time: &Datum) -> impl Iterator<Item = (i64, i64)> {
        // Every row added asks, and most rollups have none sealed.
        (self.levels.iter().filter(|_| self.sealed > 0)).filter_map(move |level| {
            let start = level.bucket(time);
            match level.buckets.get(&start) {
                Some(Bucket::Sealed { .. }) => Some((level.width, start?)),
                _ => None,
            }
        })
    }

    /// A bucket from a spill, checked so that no ask or change on it can
    /// panic: a width this rollup has, codes it holds, rows in every cell,
    /// a column per input of its kind and length.
    pub(crate) fn thaw(&self, sealed: Sealed) -> Result<Thawed, SealedError> {
        let Sealed {
            id,
            keys,
            counts,
            columns,
        } = sealed;
        match self.thawed_cells(id.width, keys, counts, columns) {
            Ok(cells) => Ok(Thawed { id, cells }),
            Err(reason) => Err(SealedError { id, reason }),
        }
    }

    fn thawed_cells(
        &self,
        bucket_width: i64,
        keys: Vec<u32>,
        counts: Vec<u64>,
        columns: Vec<SealedColumn>,
    ) -> Result<Cells, &'static str> {
        if self.level(bucket_width).is_none() {
            return Err("no such width");
        }
        let key_len = self.key_columns.len();
        if counts.is_empty() {
            return Err("no cells");
        }
        if counts.contains(&0) {
            return Err("a cell without rows");
        }
        if Some(keys.len()) != counts.len().checked_mul(key_len) {
            return Err("keys don't fit the rollup key");
        }
        let unknown = (keys.iter().enumerate())
            .any(|(i, &code)| code as usize >= self.codes[i % key_len].len());
        if unknown {
            return Err("a code the store never gave");
        }
        if columns.len() != self.inputs.len() {
            return Err("columns don't fit the measures");
        }
        let columns = (columns.into_iter().zip(&self.inputs))
            .map(|(column, input)| State::thaw(column, input.kind, counts.len()))
            .collect::<Option<_>>()
            .ok_or("columns don't fit the measures")?;
        Ok(Cells {
            key_len,
            index: FastMap::default(),
            keys,
            counts,
            states: columns,
        })
    }

    /// Puts back the cells of a sealed bucket.
    pub(crate) fn unseal(
        &mut self,
        width: i64,
        start: i64,
        mut cells: Cells,
    ) -> Result<(), &'static str> {
        let level = (self.levels.iter_mut())
            .find(|l| l.width == width)
            .ok_or("no such width")?;
        let bucket = level.buckets.get_mut(&Some(start));
        let Some(bucket @ Bucket::Sealed { .. }) = bucket else {
            return Err("not sealed");
        };
        for i in 0..cells.len() {
            let key = cells.key(i).into();
            if cells.index.insert(key, i).is_some() {
                return Err("a key held twice");
            }
        }
        *bucket = Bucket::Hot(cells);
        self.sealed -= 1;
        Ok(())
    }

    fn column(&self, name: &Identifier) -> Option<usize> {
        self.columns.iter().position(|c| c == name)
    }

    fn level(&self, width: i64) -> Option<&Level> {
        self.levels.iter().find(|l| l.width == width)
    }

    pub(crate) fn codes(&self, part: usize) -> usize {
        self.codes[part].len()
    }

    pub(crate) fn value(&self, part: usize, code: u32) -> &Datum {
        self.codes[part].value(code)
    }

    /// Rejects rows that would leave the rollup half updated.
    fn check(&self, row: &[Datum]) -> Result<(), RowError> {
        if row.len() != self.row_len() {
            return Err(RowError::Arity {
                expected: self.row_len(),
                found: row.len(),
            });
        }
        let reading = |reads: Reads| {
            (self.inputs.iter())
                .filter(move |i| i.kind.reads() == reads)
                .map(|i| i.column)
        };
        for column in iter::once(TIME).chain(reading(Reads::Numbers)) {
            if !matches!(row[column], Datum::Null | Datum::Int(_)) {
                return Err(RowError::NotInt {
                    column: self.columns[column].clone(),
                });
            }
        }
        for column in reading(Reads::Booleans) {
            if !matches!(row[column], Datum::Null | Datum::Bool(_)) {
                return Err(RowError::NotBool {
                    column: self.columns[column].clone(),
                });
            }
        }
        if self.touched(&row[TIME]).next().is_some() {
            return Err(RowError::Sealed);
        }
        Ok(())
    }

    fn code(&mut self, row: &[Datum], key: &mut Vec<u32>) {
        key.extend(
            (self.key_columns.iter().zip(&mut self.codes))
                .map(|(&coded, codes)| codes.code(&coded.datum(row))),
        );
    }
}

enum Shift {
    Same,
    New,
    Removed,
}

#[derive(Clone, Copy)]
struct MoveCell<'a> {
    key: &'a [u32],
    count: u64,
    src: &'a [State],
    src_i: usize,
    add: bool,
}

fn adjust_level(
    level: &mut Level,
    inputs: &[Input],
    time: &Datum,
    move_cell: MoveCell<'_>,
) -> Result<Shift, RowError> {
    let MoveCell {
        key,
        count,
        src,
        src_i,
        add,
    } = move_cell;
    if add {
        let Some((cells, i, new)) = level.cell(time, key, inputs) else {
            return Err(RowError::Sealed);
        };
        if !cells.absorb(i, count, src, src_i) {
            return Err(RowError::Missing);
        }
        return Ok(if new { Shift::New } else { Shift::Same });
    }
    let at = level.bucket(time);
    let empty = {
        let Some(bucket) = level.buckets.get_mut(&at) else {
            return Err(RowError::Missing);
        };
        let cells = match bucket {
            Bucket::Hot(cells) => cells,
            Bucket::Sealed { .. } => return Err(RowError::Sealed),
        };
        let Some(i) = cells.index.get(key).copied() else {
            return Err(RowError::Missing);
        };
        if !cells.release(i, count, src, src_i) {
            return Err(RowError::Missing);
        }
        if cells.counts[i] != 0 {
            return Ok(Shift::Same);
        }
        cells.swap_remove(i);
        cells.len() == 0
    };
    if empty {
        level.buckets.remove(&at);
    }
    Ok(Shift::Removed)
}

impl State {
    fn new(kind: StateKind) -> State {
        match kind {
            StateKind::Count => State::Count(Vec::new()),
            StateKind::Sum => State::Sum {
                sums: Vec::new(),
                counts: Vec::new(),
            },
            StateKind::Extent => State::Extent {
                mins: Vec::new(),
                maxs: Vec::new(),
                counts: Vec::new(),
            },
            StateKind::Distinct => State::Distinct(Vec::new()),
            StateKind::Bools => State::Bools {
                trues: Vec::new(),
                falses: Vec::new(),
            },
        }
    }

    fn push(&mut self) {
        match self {
            State::Count(counts) => counts.push(0),
            State::Sum { sums, counts } => {
                sums.push(0);
                counts.push(0);
            }
            State::Extent { mins, maxs, counts } => {
                mins.push(i64::MAX);
                maxs.push(i64::MIN);
                counts.push(ValueCounts::EMPTY);
            }
            State::Distinct(counts) => counts.push(FastMap::default()),
            State::Bools { trues, falses } => {
                trues.push(0);
                falses.push(0);
            }
        }
    }

    fn swap_remove(&mut self, i: usize) {
        match self {
            State::Count(counts) => drop(counts.swap_remove(i)),
            State::Sum { sums, counts } => {
                sums.swap_remove(i);
                counts.swap_remove(i);
            }
            State::Extent { mins, maxs, counts } => {
                mins.swap_remove(i);
                maxs.swap_remove(i);
                counts.swap_remove(i);
            }
            State::Distinct(counts) => drop(counts.swap_remove(i)),
            State::Bools { trues, falses } => {
                trues.swap_remove(i);
                falses.swap_remove(i);
            }
        }
    }

    fn sealed(&self) -> SealedColumn {
        match self {
            State::Count(counts) => SealedColumn::Count {
                counts: counts.clone(),
            },
            State::Sum { sums, counts } => SealedColumn::Sum {
                sums: sums.clone(),
                counts: counts.clone(),
            },
            State::Extent { counts, .. } => SealedColumn::Extent {
                values: counts.iter().map(|c| c.iter().collect()).collect(),
            },
            State::Distinct(counts) => SealedColumn::Distinct {
                values: (counts.iter())
                    .map(|counts| {
                        let mut pairs: Vec<(Datum, u64)> =
                            counts.iter().map(|(v, &n)| (v.clone(), n)).collect();
                        pairs.sort();
                        pairs
                    })
                    .collect(),
            },
            State::Bools { trues, falses } => SealedColumn::Bools {
                trues: trues.clone(),
                falses: falses.clone(),
            },
        }
    }

    /// `None` unless the column is of `kind`, `len` long, and counts each
    /// value it holds at least once, an extent's values ascending.
    fn thaw(column: SealedColumn, kind: StateKind, len: usize) -> Option<State> {
        let counted = |n: &u64| *n > 0;
        let ascending = |pairs: &Vec<(i64, u64)>| pairs.is_sorted_by(|a, b| a.0 < b.0);
        match (column, kind) {
            (SealedColumn::Count { counts }, StateKind::Count) if counts.len() == len => {
                Some(State::Count(counts))
            }
            (SealedColumn::Sum { sums, counts }, StateKind::Sum)
                if sums.len() == len && counts.len() == len =>
            {
                Some(State::Sum { sums, counts })
            }
            (SealedColumn::Extent { values }, StateKind::Extent)
                if values.len() == len
                    && values.iter().all(ascending)
                    && values.iter().flatten().all(|(_, n)| counted(n)) =>
            {
                let counts: Vec<ValueCounts> =
                    values.into_iter().map(ValueCounts::from_pairs).collect();
                let (mins, maxs) = counts.iter().map(ValueCounts::ends).unzip();
                Some(State::Extent { mins, maxs, counts })
            }
            (SealedColumn::Distinct { values }, StateKind::Distinct)
                if values.len() == len && values.iter().flatten().all(|(_, n)| counted(n)) =>
            {
                Some(State::Distinct(
                    values
                        .into_iter()
                        .map(|v| v.into_iter().collect())
                        .collect(),
                ))
            }
            (SealedColumn::Bools { trues, falses }, StateKind::Bools)
                if trues.len() == len && falses.len() == len =>
            {
                Some(State::Bools { trues, falses })
            }
            _ => None,
        }
    }

    /// Nulls are skipped, as in SQL.
    fn add(&mut self, i: usize, value: &Datum) {
        match (self, value) {
            (_, Datum::Null) => {}
            (State::Count(counts), _) => counts[i] += 1,
            (State::Sum { sums, counts }, &Datum::Int(v)) => {
                sums[i] += i128::from(v);
                counts[i] += 1;
            }
            (State::Extent { mins, maxs, counts }, &Datum::Int(v)) => {
                mins[i] = mins[i].min(v);
                maxs[i] = maxs[i].max(v);
                counts[i].add(v, 1);
            }
            (State::Distinct(counts), v) => *counts[i].entry(v.clone()).or_default() += 1,
            (State::Bools { trues, .. }, Datum::Bool(true)) => trues[i] += 1,
            (State::Bools { falses, .. }, Datum::Bool(false)) => falses[i] += 1,
            _ => unreachable!("Rollup::check lets through only what a state reads"),
        }
    }

    fn remove(&mut self, i: usize, value: &Datum) -> Result<(), RowError> {
        match (self, value) {
            (_, Datum::Null) => {}
            (State::Count(counts), _) => {
                counts[i] = counts[i].checked_sub(1).ok_or(RowError::Missing)?;
            }
            (State::Sum { sums, counts }, &Datum::Int(v)) => {
                counts[i] = counts[i].checked_sub(1).ok_or(RowError::Missing)?;
                sums[i] -= i128::from(v);
            }
            (State::Extent { mins, maxs, counts }, &Datum::Int(v)) => {
                if !counts[i].sub(v, 1) {
                    return Err(RowError::Missing);
                }
                (mins[i], maxs[i]) = counts[i].ends();
            }
            (State::Distinct(counts), v) => {
                let tree = &mut counts[i];
                let count = tree.get_mut(v).ok_or(RowError::Missing)?;
                *count -= 1;
                if *count == 0 {
                    tree.remove(v);
                }
            }
            (State::Bools { trues: counts, .. }, Datum::Bool(true))
            | (State::Bools { falses: counts, .. }, Datum::Bool(false)) => {
                counts[i] = counts[i].checked_sub(1).ok_or(RowError::Missing)?;
            }
            _ => unreachable!("Rollup::check lets through only what a state reads"),
        }
        Ok(())
    }

    fn same_kind(&self, other: &State) -> bool {
        matches!(
            (self, other),
            (State::Count(_), State::Count(_))
                | (State::Sum { .. }, State::Sum { .. })
                | (State::Extent { .. }, State::Extent { .. })
                | (State::Distinct(_), State::Distinct(_))
                | (State::Bools { .. }, State::Bools { .. })
        )
    }

    /// Whether cell `i` holds `src`'s cell `src_i`, so [`State::sub_from`]
    /// can take it out.
    fn covers(&self, i: usize, src: &State, src_i: usize) -> bool {
        match (self, src) {
            (State::Count(counts), State::Count(src_counts)) => counts[i] >= src_counts[src_i],
            (
                State::Sum { counts, .. },
                State::Sum {
                    counts: src_counts, ..
                },
            ) => counts[i] >= src_counts[src_i],
            (
                State::Extent { counts, .. },
                State::Extent {
                    counts: src_counts, ..
                },
            ) => counts[i].covers(&src_counts[src_i]),
            (State::Distinct(counts), State::Distinct(src_counts)) => (src_counts[src_i].iter())
                .all(|(value, n)| counts[i].get(value).is_some_and(|have| have >= n)),
            (
                State::Bools { trues, falses },
                State::Bools {
                    trues: src_trues,
                    falses: src_falses,
                },
            ) => trues[i] >= src_trues[src_i] && falses[i] >= src_falses[src_i],
            _ => false,
        }
    }

    fn add_from(&mut self, i: usize, src: &State, src_i: usize) {
        match (self, src) {
            (State::Count(counts), State::Count(src_counts)) => counts[i] += src_counts[src_i],
            (
                State::Sum { sums, counts },
                State::Sum {
                    sums: src_sums,
                    counts: src_counts,
                },
            ) => {
                sums[i] += src_sums[src_i];
                counts[i] += src_counts[src_i];
            }
            (
                State::Extent { mins, maxs, counts },
                State::Extent {
                    counts: src_counts, ..
                },
            ) => {
                counts[i].add_counts(&src_counts[src_i]);
                (mins[i], maxs[i]) = counts[i].ends();
            }
            (State::Distinct(counts), State::Distinct(src_counts)) => {
                for (value, n) in &src_counts[src_i] {
                    *counts[i].entry(value.clone()).or_default() += *n;
                }
            }
            (
                State::Bools { trues, falses },
                State::Bools {
                    trues: src_trues,
                    falses: src_falses,
                },
            ) => {
                trues[i] += src_trues[src_i];
                falses[i] += src_falses[src_i];
            }
            _ => unreachable!("Cells::absorb checks the column kinds"),
        }
    }

    fn sub_from(&mut self, i: usize, src: &State, src_i: usize) {
        match (self, src) {
            (State::Count(counts), State::Count(src_counts)) => counts[i] -= src_counts[src_i],
            (
                State::Sum { sums, counts },
                State::Sum {
                    sums: src_sums,
                    counts: src_counts,
                },
            ) => {
                sums[i] -= src_sums[src_i];
                counts[i] -= src_counts[src_i];
            }
            (
                State::Extent { mins, maxs, counts },
                State::Extent {
                    counts: src_counts, ..
                },
            ) => {
                counts[i].sub_counts(&src_counts[src_i]);
                (mins[i], maxs[i]) = counts[i].ends();
            }
            (State::Distinct(counts), State::Distinct(src_counts)) => {
                for (value, n) in &src_counts[src_i] {
                    let have = counts[i].get_mut(value).expect("covers checked");
                    *have -= *n;
                    if *have == 0 {
                        counts[i].remove(value);
                    }
                }
            }
            (
                State::Bools { trues, falses },
                State::Bools {
                    trues: src_trues,
                    falses: src_falses,
                },
            ) => {
                trues[i] -= src_trues[src_i];
                falses[i] -= src_falses[src_i];
            }
            _ => unreachable!("Cells::release checks the column kinds"),
        }
    }
}

/// Most values a cell keeps in one run. Adding a value moves half the
/// run on average, 8 KiB of it at this many.
const SORTED_MAX: usize = 1024;

/// One cell's count per value. Most cells hold a single value, kept here
/// without allocating: a tree per cell was half of a load. A cell of a few
/// values keeps them in one run, ascending, at 16 bytes a value, half of
/// what a tree's nodes cost. A run moves to take a new value, so one that
/// outgrows [`SORTED_MAX`] becomes a tree.
#[derive(Debug)]
pub(crate) enum ValueCounts {
    One(i64, u64),
    Sorted(Vec<(i64, u64)>),
    Tree(BTreeMap<i64, u64>),
}

impl ValueCounts {
    const EMPTY: ValueCounts = ValueCounts::Sorted(Vec::new());

    /// Counts `n` more of `v`, `n` above zero.
    fn add(&mut self, v: i64, n: u64) {
        match self {
            ValueCounts::One(w, c) if *w == v => *c += n,
            ValueCounts::One(w, c) => {
                let (held, new) = ((*w, *c), (v, n));
                *self = ValueCounts::Sorted(vec![held.min(new), held.max(new)]);
            }
            ValueCounts::Sorted(pairs) if pairs.is_empty() => *self = ValueCounts::One(v, n),
            ValueCounts::Sorted(pairs) => match pairs.binary_search_by_key(&v, |&(w, _)| w) {
                Ok(at) => pairs[at].1 += n,
                Err(_) if pairs.len() == SORTED_MAX => {
                    let mut tree: BTreeMap<i64, u64> = mem::take(pairs).into_iter().collect();
                    tree.insert(v, n);
                    *self = ValueCounts::Tree(tree);
                }
                Err(at) => {
                    // A `Vec` doubles when full, which would leave a run
                    // half empty.
                    if pairs.len() == pairs.capacity() {
                        pairs.reserve_exact(1 + pairs.len() / 8);
                    }
                    pairs.insert(at, (v, n));
                }
            },
            ValueCounts::Tree(tree) => *tree.entry(v).or_default() += n,
        }
    }

    /// Takes `n` of `v` back out. False, with nothing taken, when fewer
    /// are held.
    fn sub(&mut self, v: i64, n: u64) -> bool {
        if !self.has(v, n) {
            return false;
        }
        let emptied = match self {
            ValueCounts::One(_, c) => {
                *c -= n;
                *c == 0
            }
            ValueCounts::Sorted(pairs) => {
                let at = (pairs.binary_search_by_key(&v, |&(w, _)| w)).expect("has it");
                pairs[at].1 -= n;
                if pairs[at].1 == 0 {
                    pairs.remove(at);
                    if pairs.len() <= pairs.capacity() / 2 {
                        pairs.shrink_to_fit();
                    }
                }
                pairs.is_empty()
            }
            ValueCounts::Tree(tree) => {
                let count = tree.get_mut(&v).expect("has it");
                *count -= n;
                if *count == 0 {
                    tree.remove(&v);
                }
                tree.is_empty()
            }
        };
        if emptied {
            *self = ValueCounts::EMPTY;
        }
        true
    }

    fn add_counts(&mut self, other: &ValueCounts) {
        for (v, n) in other.iter() {
            self.add(v, n);
        }
    }

    /// Takes out what [`ValueCounts::covers`] found held.
    fn sub_counts(&mut self, other: &ValueCounts) {
        for (v, n) in other.iter() {
            let held = self.sub(v, n);
            assert!(held, "covers checked");
        }
    }

    fn covers(&self, other: &ValueCounts) -> bool {
        other.iter().all(|(v, n)| self.has(v, n))
    }

    fn has(&self, v: i64, n: u64) -> bool {
        match self {
            ValueCounts::One(w, c) => *w == v && *c >= n,
            ValueCounts::Sorted(pairs) => {
                (pairs.binary_search_by_key(&v, |&(w, _)| w)).is_ok_and(|at| pairs[at].1 >= n)
            }
            ValueCounts::Tree(tree) => tree.get(&v).is_some_and(|c| *c >= n),
        }
    }

    /// Each value and its count, ascending by value.
    fn iter(&self) -> impl Iterator<Item = (i64, u64)> + '_ {
        let (one, sorted, tree) = match self {
            ValueCounts::One(v, n) => (Some((*v, *n)), None, None),
            ValueCounts::Sorted(pairs) => (None, Some(pairs), None),
            ValueCounts::Tree(tree) => (None, None, Some(tree)),
        };
        (one.into_iter())
            .chain(sorted.into_iter().flatten().copied())
            .chain(tree.into_iter().flatten().map(|(&v, &n)| (v, n)))
    }

    /// `pairs` ascend by value, each counted once at least.
    fn from_pairs(mut pairs: Vec<(i64, u64)>) -> ValueCounts {
        match pairs[..] {
            [(v, n)] => ValueCounts::One(v, n),
            _ if pairs.len() <= SORTED_MAX => {
                pairs.shrink_to_fit();
                ValueCounts::Sorted(pairs)
            }
            _ => ValueCounts::Tree(pairs.into_iter().collect()),
        }
    }

    /// The least and greatest value, `i64::MAX` and `i64::MIN` if none.
    fn ends(&self) -> (i64, i64) {
        let (least, greatest) = match self {
            ValueCounts::One(v, _) => (Some(*v), Some(*v)),
            ValueCounts::Sorted(pairs) => (
                pairs.first().map(|&(v, _)| v),
                pairs.last().map(|&(v, _)| v),
            ),
            ValueCounts::Tree(tree) => (
                tree.first_key_value().map(|(&v, _)| v),
                tree.last_key_value().map(|(&v, _)| v),
            ),
        };
        (least.unwrap_or(i64::MAX), greatest.unwrap_or(i64::MIN))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowError {
    Arity {
        expected: usize,
        found: usize,
    },
    /// The time column or a sum, avg, min or max column holds a non-integer.
    NotInt {
        column: Identifier,
    },
    /// A bool_and or bool_or column holds a non-boolean.
    NotBool {
        column: Identifier,
    },
    /// A removed row was never added.
    Missing,
    /// The row falls in a sealed bucket, which takes no change until it is
    /// unsealed.
    Sealed,
}

impl fmt::Display for RowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RowError::Arity { expected, found } => {
                write!(f, "row has {found} columns, expected {expected}")
            }
            RowError::NotInt { column } => write!(f, "column {column} is not an integer"),
            RowError::NotBool { column } => write!(f, "column {column} is not a boolean"),
            RowError::Missing => f.write_str("removed row was never added"),
            RowError::Sealed => f.write_str("row falls in a sealed bucket"),
        }
    }
}

impl std::error::Error for RowError {}
