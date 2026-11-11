use crate::codes::Codes;
use crate::hash::FastMap;
use crate::rollup::RowError;
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
    key_width: usize,
    /// In [`Dim::columns`] order: the key, then the attributes. Integers
    /// until the load sets them.
    types: Vec<ColumnType>,
    /// One per attribute, with `Null` under code 0, so a key with no row
    /// and a null value read alike without a lookup.
    codes: Vec<Codes>,
    /// By key: the attribute codes of each row under it, in arrival order.
    rows: FastMap<Box<[Datum]>, Vec<Box<[u32]>>>,
    /// Rows held, over every key.
    held: usize,
    /// Keys holding more than one row.
    many: usize,
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
        self.held
    }

    /// Every key holds one row at most, so a join through it never
    /// repeats a cell.
    pub(crate) fn is_unique(&self) -> bool {
        self.many == 0
    }

    /// The types stay.
    pub(crate) fn clear(&mut self) {
        self.rows.clear();
        self.held = 0;
        self.many = 0;
        self.codes.iter_mut().for_each(|c| *c = Codes::default());
        self.code_null();
    }

    /// Adds one row, [`Dim::columns`] wide. A row equal to one held counts
    /// twice, as PostgreSQL's join would.
    pub(crate) fn add(&mut self, row: &[Datum]) -> Result<(), RowError> {
        self.check(row)?;
        let (key, attrs) = row.split_at(self.key_width);
        if key.contains(&Datum::Null) {
            return Ok(());
        }
        let codes: Box<[u32]> = (attrs.iter().zip(&mut self.codes))
            .map(|(value, codes)| codes.code(value))
            .collect();
        let rows = self.rows.entry(key.into()).or_default();
        rows.push(codes);
        self.held += 1;
        if rows.len() == 2 {
            self.many += 1;
        }
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

    /// Removes one row equal to `row`; after [`RowError::Missing`] the
    /// projection no longer matches the table.
    pub(crate) fn remove(&mut self, row: &[Datum]) -> Result<(), RowError> {
        self.check(row)?;
        let (key, attrs) = row.split_at(self.key_width);
        if key.contains(&Datum::Null) {
            return Ok(());
        }
        let held = self.rows.get_mut(key).ok_or(RowError::Missing)?;
        let same = |codes: &[u32]| {
            (attrs.iter().zip(&self.codes).zip(codes))
                .all(|((value, known), &code)| known.get(value) == Some(code))
        };
        let i = held
            .iter()
            .position(|codes| same(codes))
            .ok_or(RowError::Missing)?;
        held.swap_remove(i);
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
        self.rows.get(key).map_or(&[], Vec::as_slice)
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
