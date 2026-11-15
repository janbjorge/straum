//! A projection: one dim as the store holds it, a map from the dim's key
//! to the coded columns the views read of each row under it. A dim row
//! comes in or goes out through here; an ask or a pour looks a cell's
//! join columns up here to find the dim's rows as they are now.

use crate::codes::Codes;
use crate::hash::FastMap;
use crate::rollup::{RowError, arity, split_rows};
use crate::spec::Dim;
use crate::value::{ColumnType, Datum};

/// A dim projected to its key and the columns the views read: what a join
/// needs of it and nothing more. Cells are keyed by the fact's join
/// columns; an ask maps a cell's key through here to the dim's rows as
/// they are now, so a dim change is one entry here and the next ask sees
/// it. A key may hold several rows, as a join of any cardinality may
/// find.
#[derive(Debug)]
pub(crate) struct Projection {
    /// How many of a row's columns are its key, which comes first.
    key_columns: usize,
    /// In [`Dim::columns`] order: the key, then the attributes. Integers
    /// until the load sets them.
    types: Vec<ColumnType>,
    /// One per attribute, with `Null` under code 0, so a key with no row
    /// and a null value read alike without a lookup.
    codes: Vec<Codes>,
    /// Per dim key: the attribute codes of each row under it, in arrival
    /// order.
    rows: FastMap<Box<[Datum]>, Vec<Box<[u32]>>>,
    /// Rows held, over every key.
    held: usize,
    /// Keys holding more than one row.
    many: usize,
}

/// A position among a dim's attributes, the columns the views read
/// beside its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AttributeIndex(pub(crate) usize);

/// The code of `Null` in every attribute's codes.
pub(crate) const NULL: u32 = 0;

impl Projection {
    /// An empty projection of `dim`.
    pub(crate) fn new(dim: &Dim) -> Projection {
        let mut codes = Vec::with_capacity(dim.attributes.len());
        codes.resize_with(dim.attributes.len(), Codes::default);
        let mut projection = Projection {
            key_columns: dim.key.len(),
            types: vec![ColumnType::Int; dim.columns().count()],
            codes,
            rows: FastMap::default(),
            held: 0,
            many: 0,
        };
        projection.code_null();
        projection
    }

    fn code_null(&mut self) {
        for codes in &mut self.codes {
            assert_eq!(codes.code(&Datum::Null), NULL);
        }
    }

    /// Columns a row of the dim holds.
    fn column_count(&self) -> usize {
        self.types.len()
    }

    /// Only the source knows the row's types, so a load sets them.
    pub(crate) fn set_types(&mut self, types: &[ColumnType]) -> Result<(), RowError> {
        arity(self.column_count(), types.len())?;
        self.types.copy_from_slice(types);
        Ok(())
    }

    /// Rows held; one with a null in its key is never held, since a join
    /// on null matches nothing.
    pub(crate) fn len(&self) -> usize {
        self.held
    }

    /// How many of a row's columns are its key, which comes first.
    pub(crate) fn key_len(&self) -> usize {
        self.key_columns
    }

    /// Every key holds one row at most, so a join through it never
    /// repeats a cell.
    pub(crate) fn is_unique(&self) -> bool {
        self.many == 0
    }

    /// Drops every row and code; the types stay.
    pub(crate) fn clear(&mut self) {
        self.rows.clear();
        self.held = 0;
        self.many = 0;
        for codes in &mut self.codes {
            *codes = Codes::default();
        }
        self.code_null();
    }

    /// Adds one row, [`Dim::columns`] wide. A row equal to one held counts
    /// twice, as PostgreSQL's join would.
    pub(crate) fn add(&mut self, row: &[Datum]) -> Result<(), RowError> {
        self.check(row)?;
        self.add_checked(row);
        Ok(())
    }

    fn add_checked(&mut self, row: &[Datum]) {
        let (key, attributes) = row.split_at(self.key_columns);
        if key.contains(&Datum::Null) {
            return;
        }
        let mut codes = Vec::with_capacity(attributes.len());
        for (value, attribute_codes) in attributes.iter().zip(&mut self.codes) {
            codes.push(attribute_codes.code(value));
        }
        let rows = self.rows.entry(key.into()).or_default();
        rows.push(codes.into_boxed_slice());
        self.held += 1;
        if rows.len() == 2 {
            self.many += 1;
        }
    }

    /// Adds whole rows laid end to end, as a load reads them. If a row
    /// fails the checks, none is added.
    pub(crate) fn add_rows(&mut self, rows: &[Datum]) -> Result<(), RowError> {
        let rows = split_rows(rows, self.column_count())?;
        for row in rows.clone() {
            self.check(row)?;
        }
        for row in rows {
            self.add_checked(row);
        }
        Ok(())
    }

    /// Removes one row equal to `row`; after [`RowError::Missing`] the
    /// projection no longer matches the table.
    pub(crate) fn remove(&mut self, row: &[Datum]) -> Result<(), RowError> {
        self.check(row)?;
        let (key, attributes) = row.split_at(self.key_columns);
        if key.contains(&Datum::Null) {
            return Ok(());
        }
        let Some(held) = self.rows.get_mut(key) else {
            return Err(RowError::Missing);
        };
        let Some(position) = position_of(held, &self.codes, attributes) else {
            return Err(RowError::Missing);
        };
        held.swap_remove(position);
        self.held -= 1;
        if held.len() == 1 {
            self.many -= 1;
        }
        if held.is_empty() {
            self.rows.remove(key);
        }
        Ok(())
    }

    /// The rows under `key`, each as its attribute codes: none when no row
    /// is held under it, or when the key has a null, which joins nothing.
    pub(crate) fn matches(&self, key: &[Datum]) -> &[Box<[u32]>] {
        if key.contains(&Datum::Null) {
            return &[];
        }
        match self.rows.get(key) {
            Some(rows) => rows,
            None => &[],
        }
    }

    /// The value an attribute's code stands for.
    pub(crate) fn attribute_value(&self, attribute: AttributeIndex, code: u32) -> &Datum {
        self.codes[attribute.0].value(code)
    }

    /// Codes an attribute has, `Null`'s counted.
    pub(crate) fn attribute_codes(&self, attribute: AttributeIndex) -> usize {
        self.codes[attribute.0].len()
    }

    /// The type of an attribute's values, as the load set it.
    pub(crate) fn attribute_type(&self, attribute: AttributeIndex) -> ColumnType {
        self.types[self.key_columns + attribute.0]
    }

    /// Turns away a row that isn't [`Dim::columns`] wide.
    pub(crate) fn check(&self, row: &[Datum]) -> Result<(), RowError> {
        arity(self.column_count(), row.len())
    }
}

/// Where among `held` the row of these attribute values lies, if any
/// does.
fn position_of(held: &[Box<[u32]>], known: &[Codes], attributes: &[Datum]) -> Option<usize> {
    for (position, codes) in held.iter().enumerate() {
        if codes_match(known, attributes, codes) {
            return Some(position);
        }
    }
    None
}

/// Whether `codes` are the codes of `attributes`.
fn codes_match(known: &[Codes], attributes: &[Datum], codes: &[u32]) -> bool {
    for ((value, known), &code) in attributes.iter().zip(known).zip(codes) {
        if known.get(value) != Some(code) {
            return false;
        }
    }
    true
}
