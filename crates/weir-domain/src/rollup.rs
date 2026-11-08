//! A fact's rollup and a dim's rows, kept in step with their tables.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::iter;

use crate::ident::Ident;
use crate::spec::{Agg, Dim, Fact};
use crate::value::Datum;

/// Row position of the time column.
const TIME: usize = 0;

/// One cell per rollup key, holding what the fact's measures need.
#[derive(Debug)]
pub struct Rollup {
    /// The time column, the by columns and ref columns in rollup-key order,
    /// then the measure columns not already among them.
    columns: Vec<Ident>,
    grain: i64,
    /// By columns plus refs; they sit at row positions `1..=key_len`.
    key_len: usize,
    /// What each cell keeps, one state per input.
    inputs: Vec<Input>,
    /// Keyed on the by values, the ref ids, then the bucket.
    cells: HashMap<Box<[Datum]>, Cell>,
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
pub(crate) struct Cell {
    pub(crate) rows: u64,
    /// One per `Rollup::inputs`.
    pub(crate) state: Box<[State]>,
}

#[derive(Debug)]
pub(crate) enum State {
    /// Sum and count of the non-null values.
    Sum { sum: i128, count: u64 },
    /// Count per value, so removing the min or max reveals the next one.
    Extent(BTreeMap<i64, u64>),
    /// Count per value.
    Distinct(HashMap<Datum, u64>),
}

impl Rollup {
    pub(crate) fn new(fact: &Fact) -> Rollup {
        let mut columns: Vec<Ident> = iter::once(&fact.time.column)
            .chain(&fact.by)
            .chain(fact.refs.iter().map(|r| &r.via))
            .cloned()
            .collect();
        let key_len = columns.len() - 1;
        let mut inputs = Vec::new();
        for m in &fact.measures {
            let Some((kind, name)) = Kind::of(&m.agg) else {
                continue;
            };
            let column = columns.iter().position(|c| c == name).unwrap_or_else(|| {
                columns.push(name.clone());
                columns.len() - 1
            });
            let input = Input { kind, column };
            if !inputs.contains(&input) {
                inputs.push(input);
            }
        }
        Rollup {
            columns,
            grain: i64::from(fact.time.grain.0),
            key_len,
            inputs,
            cells: HashMap::new(),
        }
    }

    /// The columns a row carries, in order.
    pub fn columns(&self) -> &[Ident] {
        &self.columns
    }

    /// Number of cells; a cell goes when its last row is removed.
    pub fn cells(&self) -> usize {
        self.cells.len()
    }

    /// Drops every cell, as when the table is emptied.
    pub fn clear(&mut self) {
        self.cells.clear();
    }

    pub fn add(&mut self, row: &[Datum]) -> Result<(), RowError> {
        self.check(row)?;
        let cell = self.cells.entry(self.key(row)).or_insert_with(|| Cell {
            rows: 0,
            state: self.inputs.iter().map(|i| State::new(i.kind)).collect(),
        });
        cell.rows += 1;
        for (state, input) in cell.state.iter_mut().zip(&self.inputs) {
            state.add(&row[input.column]);
        }
        Ok(())
    }

    /// Takes back a row given to [`Rollup::add`]. After [`RowError::Missing`]
    /// the rollup no longer matches the table; reload the fact.
    pub fn remove(&mut self, row: &[Datum]) -> Result<(), RowError> {
        self.check(row)?;
        let key = self.key(row);
        let cell = self.cells.get_mut(&key).ok_or(RowError::Missing)?;
        for (state, input) in cell.state.iter_mut().zip(&self.inputs) {
            state.remove(&row[input.column])?;
        }
        cell.rows -= 1;
        if cell.rows == 0 {
            self.cells.remove(&key);
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

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&[Datum], &Cell)> {
        self.cells.iter().map(|(k, c)| (&**k, c))
    }

    /// Position of the bucket in a cell key.
    pub(crate) fn bucket_slot(&self) -> usize {
        self.key_len
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

    fn key(&self, row: &[Datum]) -> Box<[Datum]> {
        let bucket = match row[TIME] {
            Datum::Int(t) => Datum::Int(t.div_euclid(self.grain) * self.grain),
            _ => Datum::Null,
        };
        row[1..=self.key_len]
            .iter()
            .cloned()
            .chain(iter::once(bucket))
            .collect()
    }
}

impl State {
    fn new(kind: Kind) -> State {
        match kind {
            Kind::Sum => State::Sum { sum: 0, count: 0 },
            Kind::Extent => State::Extent(BTreeMap::new()),
            Kind::Distinct => State::Distinct(HashMap::new()),
        }
    }

    /// Nulls are skipped, as in SQL.
    fn add(&mut self, value: &Datum) {
        match (self, value) {
            (_, Datum::Null) => {}
            (State::Sum { sum, count }, Datum::Int(v)) => {
                *sum += i128::from(*v);
                *count += 1;
            }
            (State::Extent(counts), Datum::Int(v)) => *counts.entry(*v).or_default() += 1,
            (State::Distinct(counts), v) => *counts.entry(v.clone()).or_default() += 1,
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
            (State::Extent(counts), Datum::Int(v)) => {
                let count = counts.get_mut(v).ok_or(RowError::Missing)?;
                *count -= 1;
                if *count == 0 {
                    counts.remove(v);
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

/// A dim's rows by key. A row is the key, then the attributes in
/// [`Dim::attrs`] order.
#[derive(Debug)]
pub struct DimTable {
    width: usize,
    rows: HashMap<Datum, Box<[Datum]>>,
}

impl DimTable {
    pub(crate) fn new(dim: &Dim) -> DimTable {
        DimTable {
            width: 1 + dim.attrs.len(),
            rows: HashMap::new(),
        }
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

    pub fn clear(&mut self) {
        self.rows.clear();
    }

    pub(crate) fn get(&self, key: &Datum) -> Option<&[Datum]> {
        self.rows.get(key).map(|r| &**r)
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
