use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::{iter, mem, panic, thread};

use crate::agg::Agg;
use crate::codes::Codes;
use crate::hash::FastMap;
use crate::ident::Ident;
use crate::sealed::{Sealed, SealedColumn, SealedError, SealedId, Thawed};
use crate::spec::{Fact, Slot};
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
    columns: Vec<Ident>,
    /// What each rollup-key slot codes of a row, in [`Fact::rollup_key`]
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

/// What a rollup-key slot codes of a row.
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
    fn add(&mut self, row: &[Datum], key: &[u32], inputs: &[Input]) -> bool {
        let (cells, i, new) = self.cell(&row[TIME], key, inputs);
        cells.rows[i] += 1;
        for (column, input) in cells.columns.iter_mut().zip(inputs) {
            column.add(i, &row[input.column]);
        }
        new
    }

    /// The cell `key` names in the bucket `time` falls in, made empty if
    /// it's new, and whether it is.
    fn cell(&mut self, time: &Datum, key: &[u32], inputs: &[Input]) -> (&mut Cells, usize, bool) {
        let bucket = self.bucket(time);
        let bucket = (self.buckets)
            .entry(bucket)
            .or_insert_with(|| Bucket::Hot(Cells::new(key.len(), inputs)));
        let Bucket::Hot(cells) = bucket else {
            unreachable!("Rollup::check refuses rows of sealed buckets");
        };
        let (i, new) = cells.entry(key);
        (cells, i, new)
    }
}

/// The cells of one bucket, side by side: cell `i` has key
/// `keys[i * width..][..width]`, `rows[i]` rows and entry `i` of each
/// column, so an ask reads each measure's states as one flat run.
#[derive(Debug)]
pub(crate) struct Cells {
    width: usize,
    index: FastMap<Box<[u32]>, usize>,
    keys: Vec<u32>,
    rows: Vec<u64>,
    /// One per input, in [`Rollup`] input order.
    columns: Vec<Column>,
}

impl Cells {
    fn new(width: usize, inputs: &[Input]) -> Cells {
        Cells {
            width,
            index: FastMap::default(),
            keys: Vec::new(),
            rows: Vec::new(),
            columns: inputs.iter().map(|i| Column::new(i.kind)).collect(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn key(&self, i: usize) -> &[u32] {
        &self.keys[i * self.width..][..self.width]
    }

    pub(crate) fn code(&self, i: usize, slot: usize) -> u32 {
        self.keys[i * self.width + slot]
    }

    pub(crate) fn rows(&self) -> &[u64] {
        &self.rows
    }

    pub(crate) fn column(&self, input: usize) -> &Column {
        &self.columns[input]
    }

    /// The position of the cell with `key`, made empty if it's new.
    fn entry(&mut self, key: &[u32]) -> (usize, bool) {
        if let Some(&i) = self.index.get(key) {
            return (i, false);
        }
        let i = self.len();
        self.index.insert(key.into(), i);
        self.keys.extend_from_slice(key);
        self.rows.push(0);
        self.columns.iter_mut().for_each(Column::push);
        (i, true)
    }

    fn swap_remove(&mut self, i: usize) {
        let last = self.len() - 1;
        self.index
            .remove(&self.keys[i * self.width..][..self.width]);
        if i != last {
            *self
                .index
                .get_mut(&self.keys[last * self.width..][..self.width])
                .expect("every cell is indexed") = i;
            self.keys
                .copy_within(last * self.width..(last + 1) * self.width, i * self.width);
        }
        self.keys.truncate(last * self.width);
        self.rows.swap_remove(i);
        self.columns.iter_mut().for_each(|c| c.swap_remove(i));
    }
}

/// A column whose values a cell keeps, and in what form. Measures share
/// inputs: sum and avg read one `Sum`, min and max one `Extent`. Filtered
/// measures keep nothing of their own, since their conditions name rollup-key
/// parts only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Input {
    kind: Kind,
    column: usize,
}

/// A form of [`Column`], which [`Agg::state`] picks per measure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Sum,
    Extent,
    Distinct,
}

impl Kind {
    pub(crate) fn numeric(self) -> bool {
        match self {
            Kind::Sum | Kind::Extent => true,
            Kind::Distinct => false,
        }
    }
}

#[derive(Debug)]
pub(crate) enum Column {
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
        counts: Vec<Values>,
    },
    /// Count per value.
    Distinct(Vec<FastMap<Datum, u64>>),
}

impl Rollup {
    pub(crate) fn new(fact: &Fact) -> Rollup {
        let columns = fact.columns();
        let position = |name: &Ident| columns.iter().position(|c| c == name).expect(COLUMNS);
        let key_columns: Vec<Coded> = fact
            .rollup_key()
            .filter_map(|slot| match slot {
                Slot::Column(c) => Some(Coded::Value(position(c))),
                Slot::IsNull { column, negated } => Some(Coded::IsNull {
                    column: position(column),
                    negated,
                }),
                Slot::Bucket => None,
            })
            .collect();
        let mut inputs = Vec::new();
        for m in &fact.measures {
            let Some((kind, name)) = m.agg.state() else {
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
        let levels = iter::once(fact.time.grain)
            .chain(fact.time.buckets.iter().copied())
            .map(|grain| Level {
                width: i64::from(grain.0),
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
    fn width(&self) -> usize {
        self.columns.len()
    }

    /// At the `date_bin` width; a cell goes when its last row is removed.
    pub(crate) fn cells(&self) -> usize {
        self.cells
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

    pub(crate) fn key_type(&self, slot: usize) -> ColumnType {
        self.key_columns[slot].column_type(&self.types)
    }

    pub(crate) fn time_type(&self) -> ColumnType {
        self.types[TIME]
    }

    /// A measure over numbers keeps its column's scale; a count has none.
    pub(crate) fn scale_of(&self, agg: &Agg) -> u32 {
        match agg.state() {
            Some((kind, c)) if kind.numeric() => self.types[self.column(c).expect(COLUMNS)].scale(),
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
        let width = self.width();
        if !rows.len().is_multiple_of(width) {
            return Err(RowError::Arity {
                expected: width,
                found: rows.len() % width,
            });
        }
        let rows: Vec<&[Datum]> = rows.chunks_exact(width).collect();
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
                .expect("a rollup has its grain");
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
            let Some(Bucket::Hot(cells)) = level.buckets.get_mut(&bucket) else {
                return Err(RowError::Missing);
            };
            let i = *cells.index.get(key).ok_or(RowError::Missing)?;
            for (column, input) in cells.columns.iter_mut().zip(&self.inputs) {
                column.remove(i, &row[input.column])?;
            }
            cells.rows[i] -= 1;
            if cells.rows[i] == 0 {
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

    pub(crate) fn state_of(&self, agg: &Agg) -> Option<usize> {
        let (kind, name) = agg.state()?;
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
            .get(&Some(id.start))?
            .cells()?;
        Some(Sealed {
            id: id.clone(),
            keys: cells.keys.clone(),
            rows: cells.rows.clone(),
            columns: cells.columns.iter().map(Column::sealed).collect(),
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
            rows,
            columns,
        } = sealed;
        match self.thawed_cells(id.width, keys, rows, columns) {
            Ok(cells) => Ok(Thawed { id, cells }),
            Err(reason) => Err(SealedError { id, reason }),
        }
    }

    fn thawed_cells(
        &self,
        bucket_width: i64,
        keys: Vec<u32>,
        rows: Vec<u64>,
        columns: Vec<SealedColumn>,
    ) -> Result<Cells, &'static str> {
        if self.level(bucket_width).is_none() {
            return Err("no such width");
        }
        let width = self.key_columns.len();
        if rows.is_empty() {
            return Err("no cells");
        }
        if rows.contains(&0) {
            return Err("a cell without rows");
        }
        if Some(keys.len()) != rows.len().checked_mul(width) {
            return Err("keys don't fit the rollup key");
        }
        let unknown = (keys.iter().enumerate())
            .any(|(i, &code)| code as usize >= self.codes[i % width].len());
        if unknown {
            return Err("a code the store never gave");
        }
        if columns.len() != self.inputs.len() {
            return Err("columns don't fit the measures");
        }
        let columns = (columns.into_iter().zip(&self.inputs))
            .map(|(column, input)| Column::thaw(column, input.kind, rows.len()))
            .collect::<Option<_>>()
            .ok_or("columns don't fit the measures")?;
        Ok(Cells {
            width,
            index: FastMap::default(),
            keys,
            rows,
            columns,
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

    fn column(&self, name: &Ident) -> Option<usize> {
        self.columns.iter().position(|c| c == name)
    }

    fn level(&self, width: i64) -> Option<&Level> {
        self.levels.iter().find(|l| l.width == width)
    }

    pub(crate) fn codes(&self, slot: usize) -> usize {
        self.codes[slot].len()
    }

    pub(crate) fn value(&self, slot: usize, code: u32) -> &Datum {
        self.codes[slot].value(code)
    }

    /// Rejects rows that would leave the rollup half updated.
    fn check(&self, row: &[Datum]) -> Result<(), RowError> {
        if row.len() != self.width() {
            return Err(RowError::Arity {
                expected: self.width(),
                found: row.len(),
            });
        }
        let ints = self
            .inputs
            .iter()
            .filter(|i| i.kind.numeric())
            .map(|i| i.column);
        for column in iter::once(TIME).chain(ints) {
            if !matches!(row[column], Datum::Null | Datum::Int(_)) {
                return Err(RowError::NotInt {
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

impl Column {
    fn new(kind: Kind) -> Column {
        match kind {
            Kind::Sum => Column::Sum {
                sums: Vec::new(),
                counts: Vec::new(),
            },
            Kind::Extent => Column::Extent {
                mins: Vec::new(),
                maxs: Vec::new(),
                counts: Vec::new(),
            },
            Kind::Distinct => Column::Distinct(Vec::new()),
        }
    }

    fn push(&mut self) {
        match self {
            Column::Sum { sums, counts } => {
                sums.push(0);
                counts.push(0);
            }
            Column::Extent { mins, maxs, counts } => {
                mins.push(i64::MAX);
                maxs.push(i64::MIN);
                counts.push(Values::EMPTY);
            }
            Column::Distinct(counts) => counts.push(FastMap::default()),
        }
    }

    fn swap_remove(&mut self, i: usize) {
        match self {
            Column::Sum { sums, counts } => {
                sums.swap_remove(i);
                counts.swap_remove(i);
            }
            Column::Extent { mins, maxs, counts } => {
                mins.swap_remove(i);
                maxs.swap_remove(i);
                counts.swap_remove(i);
            }
            Column::Distinct(counts) => drop(counts.swap_remove(i)),
        }
    }

    fn sealed(&self) -> SealedColumn {
        match self {
            Column::Sum { sums, counts } => SealedColumn::Sum {
                sums: sums.clone(),
                counts: counts.clone(),
            },
            Column::Extent { counts, .. } => SealedColumn::Extent {
                values: counts.iter().map(Values::pairs).collect(),
            },
            Column::Distinct(counts) => SealedColumn::Distinct {
                values: (counts.iter())
                    .map(|counts| {
                        let mut pairs: Vec<(Datum, u64)> =
                            counts.iter().map(|(v, &n)| (v.clone(), n)).collect();
                        pairs.sort();
                        pairs
                    })
                    .collect(),
            },
        }
    }

    /// `None` unless the column is of `kind`, `len` long, and counts each
    /// value it holds at least once.
    fn thaw(column: SealedColumn, kind: Kind, len: usize) -> Option<Column> {
        let counted = |n: &u64| *n > 0;
        match (column, kind) {
            (SealedColumn::Sum { sums, counts }, Kind::Sum)
                if sums.len() == len && counts.len() == len =>
            {
                Some(Column::Sum { sums, counts })
            }
            (SealedColumn::Extent { values }, Kind::Extent)
                if values.len() == len && values.iter().flatten().all(|(_, n)| counted(n)) =>
            {
                let counts: Vec<Values> = values.into_iter().map(Values::from_pairs).collect();
                let (mins, maxs) = counts.iter().map(Values::ends).unzip();
                Some(Column::Extent { mins, maxs, counts })
            }
            (SealedColumn::Distinct { values }, Kind::Distinct)
                if values.len() == len && values.iter().flatten().all(|(_, n)| counted(n)) =>
            {
                Some(Column::Distinct(
                    values
                        .into_iter()
                        .map(|v| v.into_iter().collect())
                        .collect(),
                ))
            }
            _ => None,
        }
    }

    /// Nulls are skipped, as in SQL.
    fn add(&mut self, i: usize, value: &Datum) {
        match (self, value) {
            (_, Datum::Null) => {}
            (Column::Sum { sums, counts }, &Datum::Int(v)) => {
                sums[i] += i128::from(v);
                counts[i] += 1;
            }
            (Column::Extent { mins, maxs, counts }, &Datum::Int(v)) => {
                mins[i] = mins[i].min(v);
                maxs[i] = maxs[i].max(v);
                counts[i].add(v);
            }
            (Column::Distinct(counts), v) => *counts[i].entry(v.clone()).or_default() += 1,
            _ => unreachable!("Rollup::check lets only integers through"),
        }
    }

    fn remove(&mut self, i: usize, value: &Datum) -> Result<(), RowError> {
        match (self, value) {
            (_, Datum::Null) => {}
            (Column::Sum { sums, counts }, &Datum::Int(v)) => {
                counts[i] = counts[i].checked_sub(1).ok_or(RowError::Missing)?;
                sums[i] -= i128::from(v);
            }
            (Column::Extent { mins, maxs, counts }, &Datum::Int(v)) => {
                counts[i].remove(v)?;
                (mins[i], maxs[i]) = counts[i].ends();
            }
            (Column::Distinct(counts), v) => {
                let tree = &mut counts[i];
                let count = tree.get_mut(v).ok_or(RowError::Missing)?;
                *count -= 1;
                if *count == 0 {
                    tree.remove(v);
                }
            }
            _ => unreachable!("Rollup::check lets only integers through"),
        }
        Ok(())
    }
}

/// One cell's count per value. Most cells hold a single value, kept here
/// without allocating: a tree per cell was half of a load.
#[derive(Debug)]
pub(crate) enum Values {
    One(i64, u64),
    Many(BTreeMap<i64, u64>),
}

impl Values {
    const EMPTY: Values = Values::Many(BTreeMap::new());

    fn add(&mut self, v: i64) {
        match self {
            Values::One(w, n) if *w == v => *n += 1,
            Values::One(w, n) => *self = Values::Many(BTreeMap::from([(*w, *n), (v, 1)])),
            Values::Many(tree) if tree.is_empty() => *self = Values::One(v, 1),
            Values::Many(tree) => *tree.entry(v).or_default() += 1,
        }
    }

    fn remove(&mut self, v: i64) -> Result<(), RowError> {
        match self {
            Values::One(w, n) if *w == v => {
                *n -= 1;
                if *n == 0 {
                    *self = Values::EMPTY;
                }
            }
            Values::One(..) => return Err(RowError::Missing),
            Values::Many(tree) => {
                let count = tree.get_mut(&v).ok_or(RowError::Missing)?;
                *count -= 1;
                if *count == 0 {
                    tree.remove(&v);
                }
            }
        }
        Ok(())
    }

    fn pairs(&self) -> Vec<(i64, u64)> {
        match self {
            Values::One(v, n) => vec![(*v, *n)],
            Values::Many(tree) => tree.iter().map(|(&v, &n)| (v, n)).collect(),
        }
    }

    fn from_pairs(pairs: Vec<(i64, u64)>) -> Values {
        match pairs[..] {
            [(v, n)] => Values::One(v, n),
            _ => Values::Many(pairs.into_iter().collect()),
        }
    }

    /// The least and greatest value, `i64::MAX` and `i64::MIN` if none.
    fn ends(&self) -> (i64, i64) {
        match self {
            Values::One(v, _) => (*v, *v),
            Values::Many(tree) => (
                tree.first_key_value().map_or(i64::MAX, |(&v, _)| v),
                tree.last_key_value().map_or(i64::MIN, |(&v, _)| v),
            ),
        }
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
        column: Ident,
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
            RowError::Missing => f.write_str("removed row was never added"),
            RowError::Sealed => f.write_str("row falls in a sealed bucket"),
        }
    }
}

impl std::error::Error for RowError {}
