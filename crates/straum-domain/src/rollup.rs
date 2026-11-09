//! A fact's rollup and a dim's rows, kept in step with their tables.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::iter;
use std::num::NonZeroU64;

use crate::ident::Ident;
use crate::spec::{Agg, Dim, Fact};
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
    /// Row positions a load groups by: the time column, the rollup-key
    /// columns, then the min, max and count distinct inputs.
    grouped: Vec<usize>,
    /// Row positions of the sum inputs not grouped by, whose sums a load
    /// returns.
    summed: Vec<usize>,
    /// One per rollup-key column: its values under small codes, so a cell
    /// key is a few integers.
    codes: Vec<Codes>,
    /// The `date_bin` width first, then the wider ones.
    levels: Vec<Level>,
    /// Cells at the `date_bin` width.
    cells: usize,
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

    /// The cell `key` names in the bucket `time` falls in, made empty if
    /// it's new, and whether it is.
    fn cell(&mut self, time: &Datum, key: &[u32], inputs: &[Input]) -> (&mut Cells, usize, bool) {
        let bucket = self.bucket(time);
        let cells = (self.buckets)
            .entry(bucket)
            .or_insert_with(|| Cells::new(key.len(), inputs.len()));
        let (i, new) = cells.entry(key, inputs);
        (cells, i, new)
    }
}

/// A rollup-key column's values, each under the code of its first add.
/// Codes outlive their cells until [`Rollup::clear`].
#[derive(Debug, Default)]
struct Codes {
    codes: HashMap<Datum, u32>,
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
/// `keys[i * width..][..width]`, `rows[i]` rows and states
/// `states[i * inputs..][..inputs]`.
#[derive(Debug)]
pub(crate) struct Cells {
    /// Codes per key.
    width: usize,
    /// States per cell.
    inputs: usize,
    /// Cell position by key.
    index: HashMap<Box<[u32]>, usize>,
    keys: Vec<u32>,
    rows: Vec<u64>,
    states: Vec<State>,
}

impl Cells {
    fn new(width: usize, inputs: usize) -> Cells {
        Cells {
            width,
            inputs,
            index: HashMap::new(),
            keys: Vec::new(),
            rows: Vec::new(),
            states: Vec::new(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// Each cell's key codes, rows and states.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&[u32], u64, &[State])> {
        (0..self.len()).map(|i| {
            (
                &self.keys[i * self.width..][..self.width],
                self.rows[i],
                &self.states[i * self.inputs..][..self.inputs],
            )
        })
    }

    /// The position of the cell with `key`, made empty if it's new.
    fn entry(&mut self, key: &[u32], inputs: &[Input]) -> (usize, bool) {
        if let Some(&i) = self.index.get(key) {
            return (i, false);
        }
        let i = self.len();
        self.index.insert(key.into(), i);
        self.keys.extend_from_slice(key);
        self.rows.push(0);
        self.states
            .extend(inputs.iter().map(|input| State::new(input.kind)));
        (i, true)
    }

    fn states_mut(&mut self, i: usize) -> &mut [State] {
        &mut self.states[i * self.inputs..][..self.inputs]
    }

    /// Drops cell `i`; the last cell takes its place.
    fn swap_remove(&mut self, i: usize) {
        let last = self.len() - 1;
        let key = &self.keys[i * self.width..][..self.width];
        self.index.remove(key);
        if i != last {
            let moved = self.keys[last * self.width..][..self.width].to_vec();
            self.keys
                .copy_within(last * self.width..(last + 1) * self.width, i * self.width);
            *self
                .index
                .get_mut(moved.as_slice())
                .expect("every cell is indexed") = i;
            for j in 0..self.inputs {
                self.states
                    .swap(i * self.inputs + j, last * self.inputs + j);
            }
        }
        self.keys.truncate(last * self.width);
        self.states.truncate(last * self.inputs);
        self.rows.swap_remove(i);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Sum,
    Extent,
    Distinct,
}

impl Kind {
    /// The state a measure reads, and from which column; `None` for count.
    fn of(agg: &Agg) -> Option<(Kind, &Ident)> {
        match agg {
            Agg::Count => None,
            Agg::Sum(c) | Agg::Avg(c) => Some((Kind::Sum, c)),
            Agg::Min(c) | Agg::Max(c) => Some((Kind::Extent, c)),
            Agg::CountDistinct(c) => Some((Kind::Distinct, c)),
        }
    }
}

#[derive(Debug)]
pub(crate) enum State {
    /// Sum and count of the non-null values.
    Sum { sum: i128, count: u64 },
    /// Count per value, so removing the min or max reveals the next one.
    /// `min` and `max` mirror its ends, so an ask reads them without walking
    /// the tree; they mean nothing while it is empty.
    Extent {
        counts: BTreeMap<i64, u64>,
        min: i64,
        max: i64,
    },
    /// Count per value.
    Distinct(HashMap<Datum, u64>),
}

impl Rollup {
    /// An empty rollup for the fact.
    pub fn new(fact: &Fact) -> Rollup {
        let columns = fact.columns();
        let key_len = fact.by.len() + fact.refs.len();
        let mut inputs = Vec::new();
        for m in &fact.measures {
            let Some((kind, name)) = Kind::of(&m.agg) else {
                continue;
            };
            let column = columns.iter().position(|c| c == name).expect(COLUMNS);
            let input = Input { kind, column };
            if !inputs.contains(&input) {
                inputs.push(input);
            }
        }
        let mut grouped: Vec<usize> = (0..=key_len).collect();
        for input in &inputs {
            if input.kind != Kind::Sum && !grouped.contains(&input.column) {
                grouped.push(input.column);
            }
        }
        let summed = inputs
            .iter()
            .filter(|i| i.kind == Kind::Sum && !grouped.contains(&i.column))
            .map(|i| i.column)
            .collect();
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
            grouped,
            summed,
            codes: (0..key_len).map(|_| Codes::default()).collect(),
            cells: 0,
            scales: vec![0; columns.len()],
            columns,
        }
    }

    /// The columns a row carries, in order.
    pub fn columns(&self) -> &[Ident] {
        &self.columns
    }

    /// The columns a load groups by: the time column, whose bucket the load
    /// returns, the rollup-key columns, then the min, max and count distinct
    /// inputs.
    pub fn grouped(&self) -> impl Iterator<Item = &Ident> {
        self.grouped.iter().map(|&c| &self.columns[c])
    }

    /// The sum and avg inputs not among [`Rollup::grouped`], whose sum and
    /// count of non-null values a load returns per group.
    pub fn summed(&self) -> impl Iterator<Item = &Ident> {
        self.summed.iter().map(|&c| &self.columns[c])
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

    /// The scale a measure answers in: its input's, or 0 for a count.
    pub(crate) fn scale_of(&self, agg: &Agg) -> u32 {
        match agg {
            Agg::Count | Agg::CountDistinct(_) => 0,
            Agg::Sum(c) | Agg::Min(c) | Agg::Max(c) | Agg::Avg(c) => {
                let column = self.columns.iter().position(|n| n == c).expect(COLUMNS);
                self.scales[column]
            }
        }
    }

    pub fn add(&mut self, row: &[Datum]) -> Result<(), RowError> {
        self.check(row)?;
        let key = self.code(row);
        for (n, level) in self.levels.iter_mut().enumerate() {
            let (cells, i, new) = level.cell(&row[TIME], &key, &self.inputs);
            if n == 0 {
                self.cells += usize::from(new);
            }
            cells.rows[i] += 1;
            for (state, input) in cells.states_mut(i).iter_mut().zip(&self.inputs) {
                state.add(&row[input.column], 1);
            }
        }
        Ok(())
    }

    /// Adds `rows` rows at once, as one row of a `GROUP BY` load: `values`
    /// holds one value per [`Rollup::grouped`] column, `sums` one per
    /// [`Rollup::summed`] column.
    pub fn add_group(
        &mut self,
        values: &[Datum],
        rows: NonZeroU64,
        sums: &[Sum],
    ) -> Result<(), RowError> {
        let lengths = [
            (self.grouped.len(), values.len()),
            (self.summed.len(), sums.len()),
        ];
        for (expected, found) in lengths {
            if expected != found {
                return Err(RowError::Arity { expected, found });
            }
        }
        let mut row = vec![Datum::Null; self.columns.len()];
        for (&column, value) in self.grouped.iter().zip(values) {
            row[column] = value.clone();
        }
        self.check(&row)?;
        let key = self.code(&row);
        for (n, level) in self.levels.iter_mut().enumerate() {
            let (cells, i, new) = level.cell(&row[TIME], &key, &self.inputs);
            if n == 0 {
                self.cells += usize::from(new);
            }
            cells.rows[i] += rows.get();
            let mut sums = sums.iter();
            for (state, input) in cells.states_mut(i).iter_mut().zip(&self.inputs) {
                match state {
                    State::Sum { sum, count } if !self.grouped.contains(&input.column) => {
                        let group = sums.next().expect("one per summed column");
                        *sum += group.sum;
                        *count += group.count;
                    }
                    _ => state.add(&row[input.column], rows.get()),
                }
            }
        }
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
            for (state, input) in cells.states_mut(i).iter_mut().zip(&self.inputs) {
                state.remove(&row[input.column])?;
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
        let (kind, name) = Kind::of(agg)?;
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
            .filter(|i| i.kind != Kind::Distinct)
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

    /// The key codes of a checked row, new values getting new codes.
    fn code(&mut self, row: &[Datum]) -> Vec<u32> {
        (row[1..=self.key_len].iter())
            .zip(&mut self.codes)
            .map(|(value, codes)| codes.code(value))
            .collect()
    }
}

impl State {
    fn new(kind: Kind) -> State {
        match kind {
            Kind::Sum => State::Sum { sum: 0, count: 0 },
            Kind::Extent => State::Extent {
                counts: BTreeMap::new(),
                min: 0,
                max: 0,
            },
            Kind::Distinct => State::Distinct(HashMap::new()),
        }
    }

    /// Adds a value that `n` rows hold. Nulls are skipped, as in SQL.
    fn add(&mut self, value: &Datum, n: u64) {
        match (self, value) {
            (_, Datum::Null) => {}
            (State::Sum { sum, count }, Datum::Int(v)) => {
                *sum += i128::from(*v) * i128::from(n);
                *count += n;
            }
            (State::Extent { counts, min, max }, &Datum::Int(v)) => {
                if counts.is_empty() {
                    (*min, *max) = (v, v);
                } else {
                    (*min, *max) = ((*min).min(v), (*max).max(v));
                }
                *counts.entry(v).or_default() += n;
            }
            (State::Distinct(counts), v) => *counts.entry(v.clone()).or_default() += n,
            _ => unreachable!("Rollup::check lets only integers through"),
        }
    }

    fn remove(&mut self, value: &Datum) -> Result<(), RowError> {
        match (self, value) {
            (_, Datum::Null) => {}
            (State::Sum { sum, count }, Datum::Int(v)) => {
                *count = count.checked_sub(1).ok_or(RowError::Missing)?;
                *sum -= i128::from(*v);
            }
            (State::Extent { counts, min, max }, Datum::Int(v)) => {
                let count = counts.get_mut(v).ok_or(RowError::Missing)?;
                *count -= 1;
                if *count == 0 {
                    counts.remove(v);
                    if let (Some((&first, _)), Some((&last, _))) =
                        (counts.first_key_value(), counts.last_key_value())
                    {
                        (*min, *max) = (first, last);
                    }
                }
            }
            (State::Distinct(counts), v) => {
                let count = counts.get_mut(v).ok_or(RowError::Missing)?;
                *count -= 1;
                if *count == 0 {
                    counts.remove(v);
                }
            }
            _ => unreachable!("Rollup::check lets only integers through"),
        }
        Ok(())
    }
}

/// The sum of a column's non-null values over a group, and how many there
/// are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sum {
    pub sum: i128,
    pub count: u64,
}

/// A dim's rows by key. A row is the key, then the attributes in
/// [`Dim::attrs`] order.
#[derive(Debug)]
pub struct DimTable {
    width: usize,
    rows: HashMap<Datum, Box<[Datum]>>,
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
        self.rows.insert(row[0].clone(), row.into());
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
    }

    pub(crate) fn get(&self, key: &Datum) -> Option<&[Datum]> {
        self.rows.get(key).map(|r| &**r)
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
