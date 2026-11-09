//! A fact's rollup and a dim's rows, kept in step with their tables.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::{iter, mem, panic, thread};

use crate::agg::Agg;
use crate::hash::FastMap;
use crate::ident::Ident;
use crate::spec::{Dim, Fact};
use crate::value::Datum;

/// Row position of the time column.
const TIME: usize = 0;

/// Panic message for measure inputs, which `Fact::columns` lists.
const COLUMNS: &str = "Fact::columns lists every measure input";

/// One cell per rollup key, holding what the fact's measures need. Cells
/// are kept per bucket, so an ask reads only the buckets in its range, and
/// once more per wider bucket width the fact declares, so a long range
/// reads few cells.
#[derive(Debug)]
pub struct Rollup {
    /// The time column, the by columns and ref columns in rollup-key order,
    /// then the measure columns not already among them.
    columns: Vec<Ident>,
    /// By columns plus refs; they sit at row positions `1..=key_len`.
    key_len: usize,
    /// What each cell keeps, one state per input.
    inputs: Vec<Input>,
    /// One per rollup-key column: its values under small codes, so a cell
    /// key is a few integers.
    codes: Vec<Codes>,
    /// The `date_bin` width first, then the wider ones.
    levels: Vec<Level>,
    /// Cells at the `date_bin` width.
    cells: usize,
    /// The codes of the row [`Rollup::add`] is adding, kept to spare it an
    /// allocation per row.
    key: Vec<u32>,
    /// Per column; see [`Rollup::set_scales`].
    scales: Vec<u32>,
}

/// The cells of every bucket of one width.
#[derive(Debug)]
struct Level {
    /// Bucket width in seconds.
    width: i64,
    /// Cells by bucket start, oldest first. `None` holds the rows without a
    /// time, which no ask reads.
    buckets: BTreeMap<Option<i64>, Cells>,
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
        let cells = (self.buckets)
            .entry(bucket)
            .or_insert_with(|| Cells::new(key.len(), inputs));
        let (i, new) = cells.entry(key);
        (cells, i, new)
    }
}

/// A rollup-key column's values, each under the code of its first add.
/// Codes outlive their cells until [`Rollup::clear`].
#[derive(Debug, Default)]
struct Codes {
    codes: FastMap<Datum, u32>,
    values: Vec<Datum>,
}

impl Codes {
    fn code(&mut self, value: &Datum) -> u32 {
        if let Some(&code) = self.codes.get(value) {
            return code;
        }
        let code = u32::try_from(self.values.len()).expect("fewer than 2^32 values fit in memory");
        self.values.push(value.clone());
        self.codes.insert(value.clone(), code);
        code
    }
}

/// The cells of one bucket, side by side: cell `i` has key
/// `keys[i * width..][..width]`, `rows[i]` rows and entry `i` of each
/// column, so an ask reads each measure's states as one flat run.
#[derive(Debug)]
pub(crate) struct Cells {
    /// Codes per key.
    width: usize,
    /// Cell position by key.
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

    /// Cell `i`'s key codes.
    pub(crate) fn key(&self, i: usize) -> &[u32] {
        &self.keys[i * self.width..][..self.width]
    }

    /// Cell `i`'s code in key position `slot`.
    pub(crate) fn code(&self, i: usize, slot: usize) -> u32 {
        self.keys[i * self.width + slot]
    }

    /// Each cell's row count.
    pub(crate) fn rows(&self) -> &[u64] {
        &self.rows
    }

    /// Each cell's state of input `input`.
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

    /// Drops cell `i`; the last cell takes its place.
    fn swap_remove(&mut self, i: usize) {
        let last = self.len() - 1;
        self.index
            .remove(&self.keys[i * self.width..][..self.width]);
        if i != last {
            let moved = self.key(last).to_vec();
            self.keys
                .copy_within(last * self.width..(last + 1) * self.width, i * self.width);
            *self
                .index
                .get_mut(moved.as_slice())
                .expect("every cell is indexed") = i;
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
    /// Row position.
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
    /// Whether the state reads integers.
    pub(crate) fn numeric(self) -> bool {
        match self {
            Kind::Sum | Kind::Extent => true,
            Kind::Distinct => false,
        }
    }
}

/// One input's state per cell of a bucket, entry `i` being cell `i`'s.
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
    Distinct(Vec<HashMap<Datum, u64>>),
}

impl Rollup {
    /// An empty rollup for the fact.
    pub fn new(fact: &Fact) -> Rollup {
        let columns = fact.columns();
        let key_len = fact.by.len() + fact.refs.len();
        let mut inputs = Vec::new();
        for m in &fact.measures {
            let Some((kind, name)) = m.agg.state() else {
                continue;
            };
            let column = columns.iter().position(|c| c == name).expect(COLUMNS);
            let input = Input { kind, column };
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
            key_len,
            inputs,
            codes: (0..key_len).map(|_| Codes::default()).collect(),
            cells: 0,
            key: Vec::new(),
            scales: vec![0; columns.len()],
            columns,
        }
    }

    /// The columns a row carries, in order.
    pub fn columns(&self) -> &[Ident] {
        &self.columns
    }

    /// Number of cells at the `date_bin` width; a cell goes when its last
    /// row is removed.
    pub fn cells(&self) -> usize {
        self.cells
    }

    /// Drops every cell, as when the table is emptied. The scales stay.
    pub fn clear(&mut self) {
        self.levels.iter_mut().for_each(|l| l.buckets.clear());
        self.codes.iter_mut().for_each(|c| *c = Codes::default());
        self.cells = 0;
    }

    /// Sets the scale of each of [`Rollup::columns`]: a numeric column's
    /// values are integers in units of `10^-scale`. Only the source knows
    /// the column types, so a load sets them; they are 0 until then.
    /// Answers carry them.
    pub fn set_scales(&mut self, scales: &[u32]) -> Result<(), RowError> {
        set_scales(&mut self.scales, scales)
    }

    /// The scale of the rollup-key column in cell key position `slot`.
    pub(crate) fn key_scale(&self, slot: usize) -> u32 {
        self.scales[1 + slot]
    }

    /// The scale a measure answers in, from its input's.
    pub(crate) fn scale_of(&self, agg: &Agg) -> u32 {
        let input = agg.input().map_or(0, |c| {
            self.scales[self.columns.iter().position(|n| n == c).expect(COLUMNS)]
        });
        agg.scale(input)
    }

    pub fn add(&mut self, row: &[Datum]) -> Result<(), RowError> {
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

    /// Adds rows of [`Rollup::columns`] laid end to end, as a load reads
    /// them: each width of cells on its own thread, since they share
    /// nothing but the rows. If a row fails [`Rollup::add`]'s checks, none
    /// is added.
    pub fn add_rows(&mut self, rows: &[Datum]) -> Result<(), RowError> {
        let width = self.columns.len();
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
        let (key_len, inputs) = (self.key_len, &self.inputs);
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

    /// Takes back a row given to [`Rollup::add`]. After [`RowError::Missing`]
    /// the rollup no longer matches the table; start over.
    pub fn remove(&mut self, row: &[Datum]) -> Result<(), RowError> {
        self.check(row)?;
        let key: Vec<u32> = (row[1..=self.key_len].iter())
            .zip(&self.codes)
            .map(|(value, codes)| codes.codes.get(value).copied())
            .collect::<Option<_>>()
            .ok_or(RowError::Missing)?;
        for (n, level) in self.levels.iter_mut().enumerate() {
            let bucket = level.bucket(&row[TIME]);
            let cells = level.buckets.get_mut(&bucket).ok_or(RowError::Missing)?;
            let i = *cells.index.get(key.as_slice()).ok_or(RowError::Missing)?;
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

    /// Where a measure reads its cell state; `None` for count, which reads
    /// the row count.
    pub(crate) fn state_of(&self, agg: &Agg) -> Option<usize> {
        let (kind, name) = agg.state()?;
        let column = self.columns.iter().position(|c| c == name)?;
        self.inputs
            .iter()
            .position(|i| *i == Input { kind, column })
    }

    /// Buckets that cover `from..to` once, in time order, each with its
    /// start and cells: the widest that fit, narrower ones at the ends. With
    /// `each_bucket`, as an ask grouped by the bucket needs, only `date_bin`
    /// buckets. `from` and `to` are on `date_bin` edges.
    pub(crate) fn cover(
        &self,
        from: i64,
        to: i64,
        each_bucket: bool,
    ) -> impl Iterator<Item = (i64, &Cells)> {
        let widest = if each_bucket {
            0
        } else {
            self.levels.len() - 1
        };
        let mut runs = Vec::new();
        self.runs(widest, from, to, &mut runs);
        runs.into_iter().flat_map(move |(level, from, to)| {
            (self.levels[level].buckets.range(Some(from)..Some(to)))
                .filter_map(|(&bucket, cells)| Some((bucket?, cells)))
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

    /// How many codes rollup-key column `slot` has given out.
    pub(crate) fn codes(&self, slot: usize) -> usize {
        self.codes[slot].values.len()
    }

    /// The value under `code` in rollup-key column `slot`.
    pub(crate) fn value(&self, slot: usize, code: u32) -> &Datum {
        &self.codes[slot].values[code as usize]
    }

    /// Rejects rows that would leave the rollup half updated.
    fn check(&self, row: &[Datum]) -> Result<(), RowError> {
        if row.len() != self.columns.len() {
            return Err(RowError::Arity {
                expected: self.columns.len(),
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
        Ok(())
    }

    /// Appends the key codes of a checked row to `key`, new values getting
    /// new codes.
    fn code(&mut self, row: &[Datum], key: &mut Vec<u32>) {
        key.extend(
            (row[1..=self.key_len].iter())
                .zip(&mut self.codes)
                .map(|(value, codes)| codes.code(value)),
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

    /// Adds an empty cell's state.
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
            Column::Distinct(counts) => counts.push(HashMap::new()),
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

    /// Adds a value to cell `i`. Nulls are skipped, as in SQL.
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

/// A dim's rows by key. A row is the key, then the attributes in
/// [`Dim::attrs`] order, kept as codes of each column's values: an ask
/// numbers an attribute's values by their codes, hashing none of them.
#[derive(Debug)]
pub struct DimTable {
    width: usize,
    rows: HashMap<Datum, Box<[u32]>>,
    /// One per column. Codes outlive their rows until [`DimTable::clear`].
    codes: Vec<Codes>,
    /// Per column; see [`DimTable::set_scales`].
    scales: Vec<u32>,
}

impl DimTable {
    /// An empty table for the dim.
    pub fn new(dim: &Dim) -> DimTable {
        let width = 1 + dim.attrs.len();
        DimTable {
            width,
            rows: HashMap::new(),
            codes: (0..width).map(|_| Codes::default()).collect(),
            scales: vec![0; width],
        }
    }

    /// As [`Rollup::set_scales`], one per column of a row.
    pub fn set_scales(&mut self, scales: &[u32]) -> Result<(), RowError> {
        set_scales(&mut self.scales, scales)
    }

    pub(crate) fn scale(&self, column: usize) -> u32 {
        self.scales[column]
    }

    /// Inserts the row, or replaces the one with the same key.
    pub fn upsert(&mut self, row: &[Datum]) -> Result<(), RowError> {
        if row.len() != self.width {
            return Err(RowError::Arity {
                expected: self.width,
                found: row.len(),
            });
        }
        let codes = (row.iter().zip(&mut self.codes))
            .map(|(value, codes)| codes.code(value))
            .collect();
        self.rows.insert(row[0].clone(), codes);
        Ok(())
    }

    pub fn delete(&mut self, key: &Datum) -> Result<(), RowError> {
        self.rows.remove(key).map(drop).ok_or(RowError::Missing)
    }

    pub fn rows(&self) -> usize {
        self.rows.len()
    }

    /// Drops every row. The scales stay.
    pub fn clear(&mut self) {
        self.rows.clear();
        self.codes.iter_mut().for_each(|c| *c = Codes::default());
    }

    /// The codes of the row with `key`, one per column.
    pub(crate) fn get(&self, key: &Datum) -> Option<&[u32]> {
        self.rows.get(key).map(|r| &**r)
    }

    /// How many codes column `column` has given out.
    pub(crate) fn codes(&self, column: usize) -> usize {
        self.codes[column].values.len()
    }

    /// The code of null in column `column`, as for a key with no row: its
    /// code if a row holds null there, else the next.
    pub(crate) fn null(&self, column: usize) -> u32 {
        let codes = &self.codes[column];
        (codes.codes.get(&Datum::Null).copied())
            .unwrap_or_else(|| u32::try_from(codes.values.len()).expect("codes are u32"))
    }

    /// The value under `code` in column `column`.
    pub(crate) fn value(&self, column: usize, code: u32) -> &Datum {
        &self.codes[column].values[code as usize]
    }
}

fn set_scales(to: &mut [u32], scales: &[u32]) -> Result<(), RowError> {
    if scales.len() != to.len() {
        return Err(RowError::Arity {
            expected: to.len(),
            found: scales.len(),
        });
    }
    to.copy_from_slice(scales);
    Ok(())
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
}

impl fmt::Display for RowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RowError::Arity { expected, found } => {
                write!(f, "row has {found} columns, expected {expected}")
            }
            RowError::NotInt { column } => write!(f, "column {column} is not an integer"),
            RowError::Missing => f.write_str("removed row was never added"),
        }
    }
}

impl std::error::Error for RowError {}
