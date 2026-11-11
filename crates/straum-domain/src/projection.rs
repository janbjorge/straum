use crate::codes::Codes;
use crate::hash::FastMap;
use crate::rollup::RowError;
use crate::spec::Dim;
use crate::value::{ColumnType, Datum};

/// A dim projected to its key and the columns the views read: what a join
/// needs of it and nothing more. Cells are keyed by the fact's join
/// columns; an ask maps a cell's key through here to the dim's values as
/// they are now, so a dim change is one entry here and the next ask sees
/// it.
#[derive(Debug)]
pub(crate) struct Projection {
    key_width: usize,
    /// In [`Dim::columns`] order: the key, then the attributes. Integers
    /// until the load sets them.
    types: Vec<ColumnType>,
    /// One per attribute, with `Null` under code 0, so a key with no row
    /// and a null value read alike without a lookup.
    codes: Vec<Codes>,
    /// By key: the attribute codes of the one row under it.
    rows: FastMap<Box<[Datum]>, Box<[u32]>>,
}

/// The code of `Null` in every attribute's codes.
pub(crate) const NULL: u32 = 0;

impl Projection {
    pub(crate) fn new(dim: &Dim) -> Projection {
        let mut projection = Projection {
            key_width: dim.key.len(),
            types: vec![ColumnType::Int; dim.columns().count()],
            codes: dim.attrs.iter().map(|_| Codes::default()).collect(),
            rows: FastMap::default(),
        };
        projection.code_null();
        projection
    }

    fn code_null(&mut self) {
        for codes in &mut self.codes {
            assert_eq!(codes.code(&Datum::Null), NULL);
        }
    }

    fn width(&self) -> usize {
        self.types.len()
    }

    /// Only the source knows the row's types, so a load sets them.
    pub(crate) fn set_types(&mut self, types: &[ColumnType]) -> Result<(), RowError> {
        if types.len() != self.width() {
            return Err(RowError::Arity {
                expected: self.width(),
                found: types.len(),
            });
        }
        self.types.copy_from_slice(types);
        Ok(())
    }

    /// Rows held; one with a null in its key is never held, since a join
    /// on null matches nothing.
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// The types stay.
    pub(crate) fn clear(&mut self) {
        self.rows.clear();
        self.codes.iter_mut().for_each(|c| *c = Codes::default());
        self.code_null();
    }

    /// Adds one row, [`Dim::columns`] wide. A key already held is
    /// [`RowError::Present`]: the stream removes before it adds.
    pub(crate) fn add(&mut self, row: &[Datum]) -> Result<(), RowError> {
        self.check(row)?;
        let (key, attrs) = row.split_at(self.key_width);
        if key.contains(&Datum::Null) {
            return Ok(());
        }
        if self.rows.contains_key(key) {
            return Err(RowError::Present);
        }
        let codes: Box<[u32]> = (attrs.iter().zip(&mut self.codes))
            .map(|(value, codes)| codes.code(value))
            .collect();
        self.rows.insert(key.into(), codes);
        Ok(())
    }

    /// Adds whole rows laid end to end, as a load reads them. If a row
    /// fails the checks, none is added.
    pub(crate) fn add_rows(&mut self, rows: &[Datum]) -> Result<(), RowError> {
        let width = self.width();
        if !rows.len().is_multiple_of(width) {
            return Err(RowError::Arity {
                expected: width,
                found: rows.len() % width,
            });
        }
        for row in rows.chunks_exact(width) {
            self.check(row)?;
        }
        for row in rows.chunks_exact(width) {
            self.add(row)?;
        }
        Ok(())
    }

    /// Removes the row, which must be held as given; after
    /// [`RowError::Missing`] the projection no longer matches the table.
    pub(crate) fn remove(&mut self, row: &[Datum]) -> Result<(), RowError> {
        self.check(row)?;
        let (key, attrs) = row.split_at(self.key_width);
        if key.contains(&Datum::Null) {
            return Ok(());
        }
        let held = self.rows.get(key).ok_or(RowError::Missing)?;
        let same = (attrs.iter().zip(&self.codes).zip(held.iter()))
            .all(|((value, codes), &code)| codes.get(value) == Some(code));
        if !same {
            return Err(RowError::Missing);
        }
        self.rows.remove(key);
        Ok(())
    }

    /// The code of attribute `attr` under `key`: [`NULL`] when no row is
    /// held under it, as a `LEFT OUTER JOIN` gives, or when the key has a
    /// null, which joins nothing.
    pub(crate) fn attr_code(&self, key: &[Datum], attr: usize) -> u32 {
        if key.contains(&Datum::Null) {
            return NULL;
        }
        self.rows.get(key).map_or(NULL, |codes| codes[attr])
    }

    pub(crate) fn attr_value(&self, attr: usize, code: u32) -> &Datum {
        self.codes[attr].value(code)
    }

    /// Codes an attribute has, `Null`'s counted.
    pub(crate) fn attr_codes(&self, attr: usize) -> usize {
        self.codes[attr].len()
    }

    pub(crate) fn attr_type(&self, attr: usize) -> ColumnType {
        self.types[self.key_width + attr]
    }

    fn check(&self, row: &[Datum]) -> Result<(), RowError> {
        if row.len() != self.width() {
            return Err(RowError::Arity {
                expected: self.width(),
                found: row.len(),
            });
        }
        Ok(())
    }
}
