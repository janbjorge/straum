//! Bound cells: a second rollup of a fact whose joins all read fact
//! columns, keyed by the dim columns' values instead of the join
//! columns, so an ask reads no projection (ADR-0038, ADR-0042). A fact
//! row reaches them through its join rows: per join, the dim row its key
//! finds now. A hot keyed bucket is poured in or out whole when a dim is
//! replaced or the bucket is sealed or unsealed.

use crate::identifier::Identifier;
use crate::projection::{AttributeIndex, Projection};
use crate::rollup::{Cells, ColumnIndex, Direction, PartIndex, Rollup, RowError};
use crate::spec::{Fact, Ref, RefKind, Spec, Via};
use crate::value::Datum;

use super::{CHECKED, DimIndex, dim_index, position_of};

/// A fact's bound cells and how a row of the fact reaches them: each
/// ref's dim, where in the row its key is, and which of the dim's
/// attributes it reads.
#[derive(Debug)]
pub(super) struct Bound {
    pub(super) rollup: Rollup,
    pub(super) refs: Vec<BoundRef>,
    /// Columns of a bound row: the fact's, then the attributes.
    pub(super) width: usize,
    /// Leading parts of the bound key: the `by` keys, before the attributes.
    by_parts: usize,
}

/// One ref of a fact with bound cells, as positions into the fact row,
/// the keyed rollup key and the dim.
#[derive(Debug)]
pub(super) struct BoundRef {
    pub(super) dim: DimIndex,
    kind: RefKind,
    /// Positions in a fact row, in the dim's key order.
    via: Vec<ColumnIndex>,
    /// The keyed rollup-key part of each of `via`.
    via_parts: Vec<PartIndex>,
    /// The dim's attributes, in the order the ref reads them.
    pub(super) attributes: Vec<AttributeIndex>,
}

/// A join row: one way a cell's joins resolve, per ref the dim row
/// matched, or none for a left join with no match and for a semi or anti
/// join, which keep no row.
// Borrows the projections' rows: an ask makes many join rows, and a copy
// of each dim row would cost more than the ask.
pub(super) type JoinRow<'s> = Vec<Option<&'s [u32]>>;

/// The attribute rows one dim key held, used in place of the projection.
// Borrows the key and the rows of the change at hand, for that change only.
#[derive(Clone, Copy)]
pub(super) struct Overlay<'a> {
    pub(super) dim: DimIndex,
    pub(super) key: &'a [Datum],
    pub(super) rows: &'a [Box<[u32]>],
}

/// A cell of a hot keyed bucket: where it lies among the bucket's cells.
#[derive(Clone, Copy)]
pub(super) struct KeyedCell<'c> {
    // Borrows the bucket's cells, which are the bulk of the store.
    pub(super) cells: &'c Cells,
    pub(super) cell: usize,
}

impl BoundRef {
    fn new(spec: &Spec, keyed: &Rollup, columns: &[Identifier], reference: &Ref) -> BoundRef {
        let dim = dim_index(spec, reference.dim.as_str()).expect(CHECKED);
        let mut via = Vec::with_capacity(reference.via.len());
        for column in &reference.via {
            let Via::Fact(column) = column else {
                unreachable!("every ref is direct");
            };
            via.push(ColumnIndex(position_of(columns, column)));
        }
        let mut via_parts = Vec::with_capacity(via.len());
        for column in &via {
            via_parts.push(keyed.part_of_column(*column).expect(CHECKED));
        }
        let dim_attributes = &spec.dims()[dim.0].attributes;
        let mut attributes = Vec::with_capacity(reference.reads.len());
        for read in &reference.reads {
            attributes.push(AttributeIndex(position_of(dim_attributes, &read.column)));
        }
        BoundRef {
            dim,
            kind: reference.kind,
            via,
            via_parts,
            attributes,
        }
    }

    /// The rows of this ref's dim under `via`, or the overlay's rows when
    /// it stands in for that key.
    // The rows borrow the projections or the overlay, so both share `'a`.
    fn matched<'a>(
        &self,
        projections: &'a [Projection],
        via: &[Datum],
        overlay: Option<Overlay<'a>>,
    ) -> &'a [Box<[u32]>] {
        if let Some(overlay) = overlay
            && overlay.dim == self.dim
            && overlay.key == via
        {
            return overlay.rows;
        }
        projections[self.dim.0].matches(via)
    }

    /// Whether `cell` joins `key` of this ref's dim.
    fn hits(&self, keyed: &Rollup, cell: KeyedCell<'_>, key: &[Datum]) -> bool {
        let mut hits = self.via.len() == key.len();
        for (&part, datum) in self.via_parts.iter().zip(key) {
            hits = hits && keyed.value(part, cell.cells.code(cell.cell, part)) == datum;
        }
        hits
    }
}

impl Bound {
    /// A fact's bound cells, when it should keep them.
    pub(super) fn new(spec: &Spec, fact: &Fact, keyed: &Rollup) -> Option<Bound> {
        if fact.refs.is_empty() || !fact.is_direct() || !fact.vias_add_parts() {
            return None;
        }
        let columns = fact.columns();
        let mut refs = Vec::with_capacity(fact.refs.len());
        for reference in &fact.refs {
            refs.push(BoundRef::new(spec, keyed, &columns, reference));
        }
        Some(Bound {
            rollup: Rollup::bound(fact),
            refs,
            width: columns.len() + fact.attributes().len(),
            by_parts: fact.by_parts().len(),
        })
    }

    /// Appends to `out` one bound row per join row the refs resolve `row` to
    /// through the projections as they are now: the row, then the
    /// attributes read, null where a left join finds none.
    pub(super) fn rows(&self, projections: &[Projection], row: &[Datum], out: &mut Vec<Datum>) {
        let mut via_keys = Vec::with_capacity(self.refs.len());
        for reference in &self.refs {
            let mut via_key = Vec::with_capacity(reference.via.len());
            for column in &reference.via {
                via_key.push(row[column.0].clone());
            }
            via_keys.push(via_key);
        }
        for join_row in self.join_rows(projections, &via_keys, None) {
            out.extend_from_slice(row);
            for (reference, found) in self.refs.iter().zip(&join_row) {
                let projection = &projections[reference.dim.0];
                for &attribute in &reference.attributes {
                    let value = match found {
                        Some(codes) => projection.attribute_value(attribute, codes[attribute.0]),
                        None => &Datum::Null,
                    };
                    out.push(value.clone());
                }
            }
        }
    }

    /// The join rows the refs resolve `via_keys` to. `overlay` stands in
    /// for one dim key, so a change can expand the rows it held before the
    /// projection moved.
    // The join rows borrow from the projections or the overlay, whichever
    // they came from, so both share one lifetime.
    fn join_rows<'a>(
        &self,
        projections: &'a [Projection],
        via_keys: &[Vec<Datum>],
        overlay: Option<Overlay<'a>>,
    ) -> Vec<JoinRow<'a>> {
        let mut joined: Vec<JoinRow<'a>> = vec![Vec::new()];
        for (reference, via) in self.refs.iter().zip(via_keys) {
            let matches = reference.matched(projections, via, overlay);
            let mut next = Vec::new();
            for join_row in &joined {
                keep(reference.kind, matches, join_row, &mut next);
            }
            joined = next;
        }
        joined
    }

    /// Whether a ref reads `dim`.
    pub(super) fn reads(&self, dim: DimIndex) -> bool {
        let mut reads = false;
        for reference in &self.refs {
            reads = reads || reference.dim == dim;
        }
        reads
    }

    /// Whether `cell` joins `key` of `dim` through any ref.
    pub(super) fn cell_hits(
        &self,
        keyed: &Rollup,
        cell: KeyedCell<'_>,
        dim: DimIndex,
        key: &[Datum],
    ) -> bool {
        for reference in &self.refs {
            if reference.dim == dim && reference.hits(keyed, cell, key) {
                return true;
            }
        }
        false
    }

    /// Moves every cell of a hot keyed bucket into the bound cells its
    /// joins resolve to now, or back out of them.
    pub(super) fn pour(
        &mut self,
        keyed: &Rollup,
        projections: &[Projection],
        start: Option<i64>,
        cells: &Cells,
        direction: Direction,
    ) -> Result<(), RowError> {
        for cell in 0..cells.len() {
            let count = cells.rows().get(cell);
            let keyed_cell = KeyedCell { cells, cell };
            for key in &self.keys(keyed, keyed_cell, projections, None) {
                (self.rollup).adjust(start, key, count, cells.states(), cell, direction)?;
            }
        }
        Ok(())
    }

    /// The bound-key codes of each join row `cell` resolves to. `overlay`
    /// supplies one key's rows; `None` reads the projections as they are.
    pub(super) fn keys(
        &mut self,
        keyed: &Rollup,
        cell: KeyedCell<'_>,
        projections: &[Projection],
        overlay: Option<Overlay<'_>>,
    ) -> Vec<Vec<u32>> {
        let mut via_keys = Vec::with_capacity(self.refs.len());
        for reference in &self.refs {
            let mut via_key = Vec::with_capacity(reference.via_parts.len());
            for &part in &reference.via_parts {
                via_key.push(keyed.value(part, cell.cells.code(cell.cell, part)).clone());
            }
            via_keys.push(via_key);
        }
        let join_rows = self.join_rows(projections, &via_keys, overlay);
        let mut keys = Vec::with_capacity(join_rows.len());
        for join_row in &join_rows {
            keys.push(self.key(keyed, cell, projections, join_row));
        }
        keys
    }

    /// The bound-key codes of one join row of `cell`: its `by` key
    /// values, then the attributes the join row found.
    fn key(
        &mut self,
        keyed: &Rollup,
        cell: KeyedCell<'_>,
        projections: &[Projection],
        join_row: &JoinRow<'_>,
    ) -> Vec<u32> {
        let mut key = Vec::with_capacity(self.by_parts + join_row.len());
        for part in 0..self.by_parts {
            let part = PartIndex(part);
            let datum = keyed.value(part, cell.cells.code(cell.cell, part));
            key.push(self.rollup.code_part(part, datum));
        }
        let mut part = self.by_parts;
        for (reference, found) in self.refs.iter().zip(join_row) {
            for &attribute in &reference.attributes {
                let datum = match found {
                    Some(codes) => {
                        projections[reference.dim.0].attribute_value(attribute, codes[attribute.0])
                    }
                    None => &Datum::Null,
                };
                key.push(self.rollup.code_part(PartIndex(part), datum));
                part += 1;
            }
        }
        key
    }
}

/// What a join of `kind` keeps of `row`, a join row the refs before it
/// resolved, given the dim rows `matches` under its key: pushed onto
/// `next` once per row kept, as SQL's join kinds repeat or drop a row.
// The rows kept borrow what `row` and `matches` borrow, hence one lifetime.
pub(super) fn keep<'s>(
    kind: RefKind,
    matches: &'s [Box<[u32]>],
    row: &JoinRow<'s>,
    next: &mut Vec<JoinRow<'s>>,
) {
    match kind {
        RefKind::Left if matches.is_empty() => next.push(extended(row, None)),
        RefKind::Left | RefKind::Inner => {
            for found in matches {
                next.push(extended(row, Some(found)));
            }
        }
        // A semi join keeps the row when the key finds one, an anti join
        // when it finds none.
        RefKind::Semi | RefKind::Anti => {
            if matches.is_empty() == (kind == RefKind::Anti) {
                next.push(extended(row, None));
            }
        }
    }
}

/// `row` with one more ref's dim row, or none.
// The new row borrows what `row` and `found` borrow, hence one lifetime.
fn extended<'s>(row: &JoinRow<'s>, found: Option<&'s [u32]>) -> JoinRow<'s> {
    let mut row = row.clone();
    row.push(found);
    row
}
