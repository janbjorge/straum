//! Rows into and out of the store, as a load or the stream brings them.
//! A fact row goes into its keyed cells, and into its bound cells as its
//! joins resolve now. A dim row changes the projection; the bound cells
//! whose join columns name the changed dim key move to what it joins now.

use crate::projection::Projection;
use crate::rollup::{Direction, LevelBucket, LevelIndex, Rollup, RowError};
use crate::value::{ColumnType, Datum};

use super::bound::{Bound, KeyedCell, Overlay, cell_holds};
use super::{DimIndex, FactIndex, IN_SPEC, POURED, Store};

/// One dim row and whether it goes into the projection or out of it.
struct DimRowChange<'r> {
    // Borrows the row of the change at hand.
    row: &'r [Datum],
    direction: Direction,
}

impl Store {
    /// Sets the types of a row of `table`, a fact's [`Fact::columns`] or
    /// a dim's [`Dim::columns`]. Only the source knows them, so a load
    /// sets them.
    ///
    /// [`Fact::columns`]: crate::spec::Fact::columns
    /// [`Dim::columns`]: crate::spec::Dim::columns
    pub fn set_types(&mut self, table: &str, types: &[ColumnType]) -> Result<(), RowError> {
        if let Some(fact) = self.fact_index(table) {
            self.rollups[fact.0].set_types(types)?;
        } else {
            let dim = self.dim_index(table).expect(IN_SPEC);
            self.projections[dim.0].set_types(types)?;
        }
        // A bound row's types are the fact's and its dims', whichever
        // came last.
        for (rollup, bound) in self.rollups.iter().zip(&mut self.bound) {
            let Some(bound) = bound else {
                continue;
            };
            let mut types = rollup.types().to_vec();
            for reference in &bound.refs {
                let projection = &self.projections[reference.dim.0];
                for &attribute in &reference.attributes {
                    types.push(projection.attribute_type(attribute));
                }
            }
            (bound.rollup.set_types(&types)).expect("a bound row is as wide as its types");
        }
        Ok(())
    }

    /// Adds rows laid end to end, as a load reads them, each a row of
    /// `table` wide.
    pub fn add_rows(&mut self, table: &str, rows: &[Datum]) -> Result<(), RowError> {
        let added = self.add_rows_coded(table, rows);
        self.reclaim_codes();
        added
    }

    fn add_rows_coded(&mut self, table: &str, rows: &[Datum]) -> Result<(), RowError> {
        if let Some(fact) = self.fact_index(table) {
            self.rollups[fact.0].add_rows(rows)?;
            let Some(bound) = &mut self.bound[fact.0] else {
                return Ok(());
            };
            let row_width = self.rollups[fact.0].column_count();
            let mut bound_rows = Vec::new();
            for row in rows.chunks_exact(row_width) {
                bound.rows(&self.projections, row, &mut bound_rows);
            }
            return bound.rollup.add_rows(&bound_rows);
        }
        let dim = self.dim_index(table).expect(IN_SPEC);
        self.projections[dim.0].add_rows(rows)?;
        self.rebuild_dim(dim);
        Ok(())
    }

    /// Adds one row, as the stream brings it.
    pub fn add_row(&mut self, table: &str, row: &[Datum]) -> Result<(), RowError> {
        let added = self.add_row_coded(table, row);
        self.reclaim_codes();
        added
    }

    fn add_row_coded(&mut self, table: &str, row: &[Datum]) -> Result<(), RowError> {
        if let Some(fact) = self.fact_index(table) {
            self.rollups[fact.0].add(row)?;
            return self.bind(fact, row, Direction::Add);
        }
        let dim = self.dim_index(table).expect(IN_SPEC);
        let change = DimRowChange {
            row,
            direction: Direction::Add,
        };
        self.change_dim(dim, &[change])
    }

    /// Removes one row equal to the one given. A row never added is not
    /// found, so the caller stops.
    pub fn remove_row(&mut self, table: &str, row: &[Datum]) -> Result<(), RowError> {
        let removed = self.remove_row_coded(table, row);
        self.reclaim_codes();
        removed
    }

    fn remove_row_coded(&mut self, table: &str, row: &[Datum]) -> Result<(), RowError> {
        if let Some(fact) = self.fact_index(table) {
            self.rollups[fact.0].remove(row)?;
            return self.bind(fact, row, Direction::Remove);
        }
        let dim = self.dim_index(table).expect(IN_SPEC);
        let change = DimRowChange {
            row,
            direction: Direction::Remove,
        };
        self.change_dim(dim, &[change])
    }

    /// An update: the old row out and the new one in. A dim row that
    /// reads the same holds no column the views read, so it changes
    /// nothing; one that keeps its key moves the bound cells once.
    pub fn update_row(
        &mut self,
        table: &str,
        old: &[Datum],
        new: &[Datum],
    ) -> Result<(), RowError> {
        if let Some(dim) = self.dim_index(table)
            && old == new
        {
            return self.projections[dim.0].check(old);
        }
        let updated = self.update_row_coded(table, old, new);
        self.reclaim_codes();
        updated
    }

    fn update_row_coded(
        &mut self,
        table: &str,
        old: &[Datum],
        new: &[Datum],
    ) -> Result<(), RowError> {
        // Add checks the new row too, but a failure after the remove
        // would drop `old` and return before the bound cells move.
        if let Some(fact) = self.fact_index(table) {
            self.rollups[fact.0].check(new)?;
        }
        if let Some(dim) = self.dim_index(table) {
            self.projections[dim.0].check(new)?;
            let key_columns = self.projections[dim.0].key_column_count();
            if old.get(..key_columns) == new.get(..key_columns) {
                let changes = [
                    DimRowChange {
                        row: old,
                        direction: Direction::Remove,
                    },
                    DimRowChange {
                        row: new,
                        direction: Direction::Add,
                    },
                ];
                return self.change_dim(dim, &changes);
            }
        }
        self.remove_row_coded(table, old)?;
        self.add_row_coded(table, new)
    }

    /// Drops the codes no cell or dim row holds any more, wherever they
    /// may be most of a column's codes. Run once a change is whole, so no
    /// code is held across it.
    fn reclaim_codes(&mut self) {
        for rollup in &mut self.rollups {
            rollup.reclaim_codes();
        }
        for bound in self.bound.iter_mut().flatten() {
            bound.rollup.reclaim_codes();
        }
        for projection in &mut self.projections {
            projection.reclaim_codes();
        }
    }

    /// Drops every row of a table after a truncate.
    pub fn clear(&mut self, table: &str) {
        if let Some(fact) = self.fact_index(table) {
            self.rollups[fact.0].clear();
            if let Some(bound) = &mut self.bound[fact.0] {
                bound.rollup.clear();
            }
            return;
        }
        let dim = self.dim_index(table).expect(IN_SPEC);
        self.projections[dim.0].clear();
        self.rebuild_dim(dim);
    }

    /// Applies `changes`, rows that share one key, to `dim`'s projection,
    /// then moves the bound cells that name the key once.
    fn change_dim(&mut self, dim: DimIndex, changes: &[DimRowChange<'_>]) -> Result<(), RowError> {
        let Some(first) = changes.first() else {
            return Ok(());
        };
        let key_columns = self.projections[dim.0].key_column_count();
        let key = first.row.get(..key_columns).unwrap_or(&[]);
        // Owned, so the bound cells can leave the rows the key held after
        // the projection has moved on.
        let old = self.projections[dim.0].matches(key).to_vec();
        for change in changes {
            let projection = &mut self.projections[dim.0];
            match change.direction {
                Direction::Add => projection.add(change.row)?,
                Direction::Remove => projection.remove(change.row)?,
            }
        }
        if old == self.projections[dim.0].matches(key) {
            return Ok(());
        }
        for fact in self.facts_reading(dim) {
            self.retarget(fact, dim, key, &old)?;
        }
        Ok(())
    }

    /// Adds or removes a fact row's bound rows, as the refs resolve it
    /// now.
    fn bind(
        &mut self,
        fact: FactIndex,
        row: &[Datum],
        direction: Direction,
    ) -> Result<(), RowError> {
        let Some(bound) = &mut self.bound[fact.0] else {
            return Ok(());
        };
        let mut bound_rows = Vec::new();
        bound.rows(&self.projections, row, &mut bound_rows);
        for row in bound_rows.chunks_exact(bound.width) {
            match direction {
                Direction::Add => bound.rollup.add(row)?,
                Direction::Remove => bound.rollup.remove(row)?,
            }
        }
        Ok(())
    }

    /// The facts whose bound cells read `dim`.
    fn facts_reading(&self, dim: DimIndex) -> Vec<FactIndex> {
        let mut facts = Vec::new();
        for (fact, bound) in self.bound.iter().enumerate() {
            if let Some(bound) = bound
                && bound.reads(dim)
            {
                facts.push(FactIndex(fact));
            }
        }
        facts
    }

    /// `dim`'s `key` held the `old` attribute rows before it changed.
    /// Bound cells whose join columns name `key` move; the rest stay.
    fn retarget(
        &mut self,
        fact: FactIndex,
        dim: DimIndex,
        key: &[Datum],
        old: &[Box<[u32]>],
    ) -> Result<(), RowError> {
        let Some(bound) = self.bound[fact.0].as_mut() else {
            return Ok(());
        };
        let keyed = &self.rollups[fact.0];
        let codes = bound.key_codes(keyed, dim, key);
        if codes.is_empty() {
            return Ok(());
        }
        let overlay = Overlay {
            dim,
            key,
            rows: old,
        };
        for level in 0..keyed.level_count() {
            let level = LevelIndex(level);
            for (start, cells) in keyed.buckets(level) {
                let at = LevelBucket {
                    level,
                    start: *start,
                };
                for cell in 0..cells.len() {
                    let cell = KeyedCell { cells, cell };
                    if cell_holds(cell, &codes) {
                        move_cell(bound, keyed, &self.projections, at, cell, overlay)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// The dim was replaced wholesale: pour the keyed cells into a
    /// fresh bound rollup.
    fn rebuild_dim(&mut self, dim: DimIndex) {
        for fact in self.facts_reading(dim) {
            if !self.rollups[fact.0].holds_cells() {
                continue;
            }
            self.rebuild_fact(fact);
        }
    }

    fn rebuild_fact(&mut self, fact: FactIndex) {
        let Some(bound) = self.bound[fact.0].as_mut() else {
            return;
        };
        bound.rollup.clear();
        let keyed = &self.rollups[fact.0];
        for level in 0..keyed.level_count() {
            let level = LevelIndex(level);
            for (start, cells) in keyed.buckets(level) {
                let at = LevelBucket {
                    level,
                    start: *start,
                };
                (bound.pour(keyed, &self.projections, at, cells)).expect(POURED);
            }
        }
    }
}

/// Moves a keyed cell's share of the bound cells from the join rows it
/// resolved to under `overlay`, the dim key as it was, to those it
/// resolves to now.
fn move_cell(
    bound: &mut Bound,
    keyed: &Rollup,
    projections: &[Projection],
    at: LevelBucket,
    cell: KeyedCell<'_>,
    overlay: Overlay<'_>,
) -> Result<(), RowError> {
    let old_keys = bound.keys(keyed, cell, projections, Some(overlay));
    let new_keys = bound.keys(keyed, cell, projections, None);
    if old_keys == new_keys {
        return Ok(());
    }
    let KeyedCell { cells, cell } = cell;
    let count = cells.rows().get(cell);
    for key in &old_keys {
        (bound.rollup).adjust(at, key, count, cells.states(), cell, Direction::Remove)?;
    }
    for key in &new_keys {
        (bound.rollup).adjust(at, key, count, cells.states(), cell, Direction::Add)?;
    }
    Ok(())
}
