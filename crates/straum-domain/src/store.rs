use std::borrow::Cow;
use std::ops::Range;

use crate::aggregate::Partials;
use crate::ask::{Ask, AskError, AskKey};
use crate::hash::FastMap;
use crate::identifier::Identifier;
use crate::projection::{AttributeIndex, NULL, Projection};
use crate::rollup::{
    BucketId, Cells, ColumnIndex, Cover, CoveredBucket, Direction, InputIndex, PartIndex, Rollup,
    RowError,
};

use crate::sealed::{Sealed, SealedError, SealedId, Thawed};
use crate::spec::{
    Condition, Fact, Key, KeyPart, Literal, Measure, Ref, RefKind, Spec, Via, Width,
};
use crate::value::{ColumnType, Datum, Value};

const CHECKED: &str = "checked by Spec::new or Spec::check";
const IN_SPEC: &str = "the table is in the spec";
const HOT: &str = "the bucket is hot";
const POURED: &str = "a bound cell takes any cell of its fact";

/// Thawed cells an ask reads, by bucket.
// Borrowed from the thawed buckets the caller holds, which may be large.
type ThawedCells<'t> = FastMap<BucketId, &'t Cells>;

/// A position among the spec's facts, and so among the store's rollups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FactIndex(usize);

/// A position among the spec's dims, and so among the store's
/// projections.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DimIndex(usize);

/// The cells of every fact, and every dim projected to its key and the
/// columns the views read. Nothing else: a cell is keyed by the fact's
/// join columns, and an ask reads a dim's rows through them, as the dim
/// is now. A fact that joins on a column no `by` key reads keeps bound
/// cells beside them, keyed by the dim columns themselves, which are
/// fewer and answer its asks (ADR-0038, ADR-0042). A dim change moves
/// the bound cells whose join columns name the changed key.
#[derive(Debug)]
pub struct Store {
    spec: Spec,
    rollups: Vec<Rollup>,
    projections: Vec<Projection>,
    /// Per fact. None for a fact without refs, whose keyed cells are
    /// bound already, for one whose refs all join on `by` keys, whose
    /// bound cells would be as many as its keyed cells, and for one with
    /// a ref through another dim.
    bound: Vec<Option<Bound>>,
}

/// A fact's bound cells and how a row of the fact reaches them: each
/// ref's dim, where in the row its key is, and which of the dim's
/// attributes it reads.
#[derive(Debug)]
struct Bound {
    rollup: Rollup,
    refs: Vec<BoundRef>,
    /// Columns of a bound row: the fact's, then the attributes.
    width: usize,
    /// Leading parts of the bound key: the `by` keys, before the attributes.
    by_len: usize,
}

#[derive(Debug)]
struct BoundRef {
    dim: DimIndex,
    kind: RefKind,
    /// Positions in a fact row, in the dim's key order.
    via: Vec<ColumnIndex>,
    /// The keyed rollup-key part of each of `via`.
    via_parts: Vec<PartIndex>,
    /// The dim's attributes, in the order the ref reads them.
    attributes: Vec<AttributeIndex>,
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
}

fn position_of(names: &[Identifier], name: &Identifier) -> usize {
    for (position, each) in names.iter().enumerate() {
        if each == name {
            return position;
        }
    }
    panic!("{CHECKED}")
}

impl Bound {
    fn new(spec: &Spec, fact: &Fact, keyed: &Rollup) -> Option<Bound> {
        if fact.refs.is_empty() || !fact.is_direct() || !fact.vias_add_parts() {
            return None;
        }
        let columns = fact.columns();
        let refs = (fact.refs.iter())
            .map(|reference| BoundRef::new(spec, keyed, &columns, reference))
            .collect();
        Some(Bound {
            rollup: Rollup::bound(fact),
            refs,
            width: columns.len() + fact.attributes().count(),
            by_len: fact.by_parts().count(),
        })
    }

    /// Appends to `out` one bound row per join row the refs resolve `row` to
    /// through the projections as they are now: the row, then the
    /// attributes read, null where a left join finds none.
    fn rows(&self, projections: &[Projection], row: &[Datum], out: &mut Vec<Datum>) {
        let via_keys: Vec<Vec<Datum>> = (self.refs.iter())
            .map(|reference| {
                reference
                    .via
                    .iter()
                    .map(|&column| row[column.0].clone())
                    .collect()
            })
            .collect();
        for join_row in self.join_rows(projections, &via_keys, None) {
            out.extend_from_slice(row);
            for (reference, found) in self.refs.iter().zip(&join_row) {
                let projection = &projections[reference.dim.0];
                out.extend(reference.attributes.iter().map(|&attribute| {
                    match found {
                        Some(codes) => projection
                            .attribute_value(attribute, codes[attribute.0])
                            .clone(),
                        None => Datum::Null,
                    }
                }));
            }
        }
    }

    /// The join rows the refs resolve `via_keys` to. `overlay` stands in
    /// for one dim key, so a change can expand the rows it held before the
    /// projection moved.
    fn join_rows<'a>(
        &self,
        projections: &'a [Projection],
        via_keys: &[Vec<Datum>],
        overlay: Option<Overlay<'a>>,
    ) -> Vec<JoinRow<'a>> {
        let mut joined: Vec<JoinRow<'a>> = vec![Vec::new()];
        for (reference, via) in self.refs.iter().zip(via_keys) {
            let matches = matched(projections, reference, via, overlay);
            let mut next = Vec::new();
            for join_row in &joined {
                keep(reference.kind, matches, join_row, &mut next);
            }
            joined = next;
        }
        joined
    }

    fn reads(&self, dim: DimIndex) -> bool {
        self.refs.iter().any(|reference| reference.dim == dim)
    }
}

/// One row per group, sorted by key: the values of the ask's `by`, then its
/// measures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// One per value of a row, the `by` keys then the measures.
    pub columns: Vec<AnswerColumn>,
    pub rows: Vec<AnswerRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerRow {
    /// The values of the ask's `by`.
    pub keys: Vec<Datum>,
    /// One per measure the ask names.
    pub values: Vec<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerColumn {
    /// As the ask names it.
    pub name: String,
    /// Numeric values are integers in units of `10^-scale`.
    pub scale: u32,
    /// A time key, whose values are the second its bucket starts.
    pub is_time: bool,
}

impl Store {
    pub fn new(spec: Spec) -> Store {
        let rollups: Vec<Rollup> = spec.facts().iter().map(Rollup::new).collect();
        Store {
            projections: spec.dims().iter().map(Projection::new).collect(),
            bound: (spec.facts().iter().zip(&rollups))
                .map(|(fact, keyed)| Bound::new(&spec, fact, keyed))
                .collect(),
            rollups,
            spec,
        }
    }

    /// Sets the types of a row of `table`, a fact's [`Fact::columns`] or
    /// a dim's [`Dim::columns`]. Only the source knows them, so a load
    /// sets them.
    pub fn set_types(&mut self, table: &str, types: &[ColumnType]) -> Result<(), RowError> {
        if let Some(fact) = self.fact_index(table) {
            self.rollups[fact.0].set_types(types)?;
        } else {
            let dim = self.dim_index(table).expect(IN_SPEC);
            self.projections[dim.0].set_types(types)?;
        }
        // A bound row's types are the fact's and its dims', whichever
        // came last.
        let Store {
            rollups,
            projections,
            bound,
            ..
        } = self;
        for (rollup, bound) in rollups.iter().zip(bound) {
            let Some(bound) = bound else { continue };
            let mut types = rollup.types().to_vec();
            for reference in &bound.refs {
                types.extend(
                    reference
                        .attributes
                        .iter()
                        .map(|&attribute| projections[reference.dim.0].attribute_type(attribute)),
                );
            }
            (bound.rollup.set_types(&types)).expect("a bound row is as wide as its types");
        }
        Ok(())
    }

    /// Adds rows laid end to end, as a load reads them, each a row of
    /// `table` wide.
    pub fn add_rows(&mut self, table: &str, rows: &[Datum]) -> Result<(), RowError> {
        if let Some(fact) = self.fact_index(table) {
            self.rollups[fact.0].add_rows(rows)?;
            let Some(bound) = &mut self.bound[fact.0] else {
                return Ok(());
            };
            let row_len = self.rollups[fact.0].row_len();
            let mut out = Vec::new();
            for row in rows.chunks_exact(row_len) {
                bound.rows(&self.projections, row, &mut out);
            }
            return bound.rollup.add_rows(&out);
        }
        let dim = self.dim_index(table).expect(IN_SPEC);
        self.projections[dim.0].add_rows(rows)?;
        self.rebuild_dim(dim);
        Ok(())
    }

    /// Adds one row, as the stream brings it.
    pub fn add_row(&mut self, table: &str, row: &[Datum]) -> Result<(), RowError> {
        if let Some(fact) = self.fact_index(table) {
            self.rollups[fact.0].add(row)?;
            return self.bind(fact, row, Direction::Add);
        }
        self.change_dim(table, row, Projection::add)
    }

    /// Removes one row equal to the one given. A row never added is not
    /// found, so the caller starts over.
    pub fn remove_row(&mut self, table: &str, row: &[Datum]) -> Result<(), RowError> {
        if let Some(fact) = self.fact_index(table) {
            self.rollups[fact.0].remove(row)?;
            return self.bind(fact, row, Direction::Remove);
        }
        self.change_dim(table, row, Projection::remove)
    }

    fn change_dim(
        &mut self,
        table: &str,
        row: &[Datum],
        change: fn(&mut Projection, &[Datum]) -> Result<(), RowError>,
    ) -> Result<(), RowError> {
        let dim = self.dim_index(table).expect(IN_SPEC);
        let key_len = self.projections[dim.0].key_len();
        let key = row.get(..key_len).unwrap_or(&[]);
        // Owned, so the bound cells can leave the rows the key held after
        // the projection has moved on.
        let old = self.projections[dim.0].matches(key).to_vec();
        change(&mut self.projections[dim.0], row)?;
        if old == self.projections[dim.0].matches(key) {
            return Ok(());
        }
        for fact in self.facts_reading(dim) {
            self.retarget(fact, dim, key, &old)?;
        }
        Ok(())
    }

    /// An update: the old row out and the new one in. A dim row that
    /// reads the same holds no column the views read, so it changes
    /// nothing.
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
        self.remove_row(table, old)?;
        self.add_row(table, new)
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
        let mut out = Vec::new();
        bound.rows(&self.projections, row, &mut out);
        for row in out.chunks_exact(bound.width) {
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
        let Store {
            rollups,
            projections,
            bound,
            ..
        } = self;
        let Some(bound) = bound[fact.0].as_mut() else {
            return Ok(());
        };
        let keyed = &rollups[fact.0];
        let overlay = Overlay {
            dim,
            key,
            rows: old,
        };
        for start in keyed.hot_date_bin_starts() {
            let Some(cells) = keyed.hot_date_bin(start) else {
                continue;
            };
            for cell in 0..cells.len() {
                if !cell_hits(bound, keyed, cells, cell, dim, key) {
                    continue;
                }
                let old_keys = bound_keys(bound, keyed, cells, cell, projections, Some(overlay));
                let new_keys = bound_keys(bound, keyed, cells, cell, projections, None);
                if old_keys == new_keys {
                    continue;
                }
                let count = cells.rows().get(cell);
                for key in &old_keys {
                    bound.rollup.adjust(
                        start,
                        key,
                        count,
                        cells.states(),
                        cell,
                        Direction::Remove,
                    )?;
                }
                for key in &new_keys {
                    bound
                        .rollup
                        .adjust(start, key, count, cells.states(), cell, Direction::Add)?;
                }
            }
        }
        Ok(())
    }

    /// The dim was replaced wholesale: pour the hot keyed cells into a
    /// fresh bound rollup.
    fn rebuild_dim(&mut self, dim: DimIndex) {
        for fact in self.facts_reading(dim) {
            if self.rollups[fact.0].cells() == 0 {
                continue;
            }
            self.rebuild_fact(fact);
        }
    }

    fn rebuild_fact(&mut self, fact: FactIndex) {
        let Store {
            rollups,
            projections,
            bound,
            ..
        } = self;
        let Some(bound) = bound[fact.0].as_mut() else {
            return;
        };
        bound.rollup.clear();
        let keyed = &rollups[fact.0];
        for start in keyed.hot_date_bin_starts() {
            let cells = keyed.hot_date_bin(start).expect(HOT);
            pour(bound, keyed, projections, start, cells, Direction::Add).expect(POURED);
        }
    }

    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    /// Keyed cells at the `date_bin` width.
    pub fn cells(&self, fact: &str) -> Option<usize> {
        Some(self.rollup(fact)?.1.cells())
    }

    /// Bound cells at the `date_bin` width, while the fact has them.
    pub fn bound_cells(&self, fact: &str) -> Option<usize> {
        let index = self.fact_index(fact)?;
        Some(self.bound[index.0].as_ref()?.rollup.cells())
    }

    /// Sealed buckets, every fact and width.
    pub fn sealed_count(&self) -> usize {
        self.rollups.iter().map(Rollup::sealed_count).sum()
    }

    pub fn dim_rows(&self, dim: &str) -> Option<usize> {
        Some(self.projections[self.dim_index(dim)?.0].len())
    }

    /// From the start of the fact's first `date_bin` bucket to the end of
    /// its last, hot or sealed, in Unix seconds; none while it has no
    /// bucket.
    pub fn range(&self, fact: &str) -> Option<Range<i64>> {
        self.rollup(fact)?.1.range()
    }

    /// The type of a key the view names, as the load set it.
    pub fn key_type(&self, fact: &str, key: &Identifier) -> Option<ColumnType> {
        let (fact, rollup) = self.rollup(fact)?;
        Some(match fact.resolve(key)? {
            Key::ByKey(by) => rollup.key_type(key_part_of(fact, by.key_part())),
            Key::Attribute(reference, attribute) => {
                let dim = self.spec.dim(reference.dim.as_str()).expect(CHECKED);
                let read = AttributeIndex(position_of(&dim.attributes, &attribute.column));
                self.projection(reference.dim.as_str()).attribute_type(read)
            }
        })
    }

    pub fn cells_read(&self, ask: &Ask) -> Result<usize, Vec<AskError>> {
        let (_, read) = self.reading(ask)?;
        let mut cells = 0;
        for covered in buckets(read.rollup(), ask) {
            cells += covered.bucket.len();
        }
        Ok(cells)
    }

    /// Each `date_bin` bucket not sealed that ends by `before`, Unix
    /// seconds, every fact.
    pub fn sealable(&self, before: i64) -> Vec<SealedId> {
        let mut sealable = Vec::new();
        for (fact, rollup) in self.spec.facts().iter().zip(&self.rollups) {
            sealable.extend(sealed_ids(fact, &rollup.sealable(before)));
        }
        sealable
    }

    /// A copy of a bucket's cells, for a spill to keep before
    /// [`Store::seal`] drops them. `None` if it is sealed or has no cells.
    pub fn sealed_copy(&self, id: &SealedId) -> Option<Sealed> {
        self.rollup(id.fact.as_str())?.1.sealed_copy(id)
    }

    /// Drops a bucket's cells, keeping its place, so asks and changes ask
    /// for it back. What the bucket put into the bound cells leaves them,
    /// at every width, until [`Store::unseal`] binds it again: the bound
    /// cells hold the hot buckets and no other, so a dim change moves all
    /// of them, and an ask over a sealed bucket reads the keyed cells.
    pub fn seal(&mut self, id: &SealedId) -> Result<(), SealedError> {
        let Some(fact) = self.fact_index(id.fact.as_str()) else {
            return Err(sealed_error(id, "unknown fact"));
        };
        if self
            .pour_bucket(fact, id.width, id.bucket_start, Direction::Remove)
            .is_err()
        {
            return Err(sealed_error(id, "bound cells don't hold the bucket"));
        }
        match self.rollups[fact.0].seal(id.width, id.bucket_start) {
            Ok(()) => Ok(()),
            Err(reason) => Err(sealed_error(id, reason)),
        }
    }

    /// Binds a hot `date_bin` bucket's cells into the bound cells, or
    /// takes them back out, as the dims are now. A wider bucket has no
    /// rows of its own to bind.
    fn pour_bucket(
        &mut self,
        fact: FactIndex,
        width: i64,
        start: i64,
        direction: Direction,
    ) -> Result<(), RowError> {
        let Store {
            rollups,
            projections,
            bound,
            ..
        } = self;
        let (Some(bound), keyed) = (bound[fact.0].as_mut(), &rollups[fact.0]) else {
            return Ok(());
        };
        if width != keyed.date_bin_width() {
            return Ok(());
        }
        match keyed.hot_date_bin(Some(start)) {
            Some(cells) => pour(bound, keyed, projections, Some(start), cells, direction),
            None => Ok(()),
        }
    }

    /// Every sealed bucket of a fact, which a truncate drops with the rest.
    pub fn sealed(&self, fact: &str) -> Vec<SealedId> {
        let (fact, rollup) = self.rollup(fact).expect(IN_SPEC);
        sealed_ids(fact, &rollup.sealed())
    }

    /// The sealed buckets [`Store::ask_with`] needs for `ask`; none when
    /// the bound cells answer it, which are never sealed.
    pub fn sealed_read(&self, ask: &Ask) -> Result<Vec<SealedId>, Vec<AskError>> {
        let (fact, read) = self.reading(ask)?;
        let mut sealed = Vec::new();
        for covered in buckets(read.rollup(), ask) {
            if covered.bucket.cells().is_none() {
                sealed.push(covered.id);
            }
        }
        Ok(sealed_ids(fact, &sealed))
    }

    /// The sealed buckets a fact row falls in, which must be unsealed
    /// before the row is added or removed.
    pub fn sealed_touched(&self, fact: &str, row: &[Datum]) -> Vec<SealedId> {
        let (fact, rollup) = self.rollup(fact).expect(IN_SPEC);
        match row.first() {
            Some(time) => sealed_ids(fact, &rollup.touched(time)),
            None => Vec::new(),
        }
    }

    /// Checks a sealed bucket against the store, so neither an ask nor
    /// [`Store::unseal`] can panic on it.
    pub fn thaw(&self, sealed: Sealed) -> Result<Thawed, SealedError> {
        match self.rollup(sealed.id.fact.as_str()) {
            Some((_, rollup)) => rollup.thaw(sealed),
            None => Err(SealedError {
                id: sealed.id,
                reason: "unknown fact",
            }),
        }
    }

    /// Puts a sealed bucket's cells back, so changes reach them again,
    /// and binds them as the dims are now.
    pub fn unseal(&mut self, thawed: Thawed) -> Result<(), SealedError> {
        let Thawed { id, cells } = thawed;
        let fact = self.fact_index(id.fact.as_str()).expect(IN_SPEC);
        match self.rollups[fact.0].unseal(id.width, id.bucket_start, cells) {
            Ok(()) => {
                let poured = self.pour_bucket(fact, id.width, id.bucket_start, Direction::Add);
                poured.expect(POURED);
                Ok(())
            }
            Err(reason) => Err(SealedError { id, reason }),
        }
    }

    /// Every key is a part of the rollup key or a dim value reached
    /// through the parts of a ref's via columns, so the cell loop only
    /// looks up what each code maps to, once per code. A view whose joins
    /// find one row a key at most walks the cells once each, passing over
    /// those an inner, semi or anti join drops; one with a key of several
    /// rows, or a join through another dim, walks each cell once per row
    /// its joins find, as PostgreSQL repeats the fact row.
    pub fn ask(&self, ask: &Ask) -> Result<Answer, Vec<AskError>> {
        self.ask_with(ask, &[])
    }

    /// As [`Store::ask`], reading sealed buckets from `thawed`, which holds
    /// every one [`Store::sealed_read`] names.
    pub fn ask_with(&self, ask: &Ask, thawed: &[Thawed]) -> Result<Answer, Vec<AskError>> {
        let (fact, read) = self.reading(ask)?;
        let rollup = read.rollup();
        self.check_types(fact, &read, ask)?;
        let mut loaded: ThawedCells<'_> = FastMap::default();
        for bucket in thawed {
            if bucket.id.fact == fact.name {
                let id = BucketId {
                    width: bucket.id.width,
                    start: bucket.id.bucket_start,
                };
                loaded.insert(id, &bucket.cells);
            }
        }
        let buckets = buckets(rollup, ask).count();
        let mut by: Vec<Grouping> = (ask.keys.iter())
            .map(|key| match key {
                AskKey::Key(name) => Grouping::lookup(self.lookup(fact, &read, name), rollup),
                AskKey::Bucket => Grouping::time(fact.time.width, buckets),
            })
            .collect();
        let mut groups = Groups::new(&by);
        let mut filter = self.condition_checks(fact, &read, &fact.filter);
        filter.extend(self.condition_checks(fact, &read, &ask.filter));
        let asked: Vec<&Measure> = (ask.measures.iter())
            .map(|name| fact.measure(name.as_str()).expect(CHECKED))
            .collect();
        let mut measures: Vec<Asked> = (asked.iter())
            .map(|measure| Asked {
                partials: Partials::new(&measure.aggregate),
                state: rollup.state_of(&measure.aggregate),
                filter: self.condition_checks(fact, &read, &measure.filter),
            })
            .collect();
        let columns = answer_columns(ask, rollup, &by, &asked);

        if by.is_empty() {
            // As in SQL without GROUP BY: one row even when nothing matches.
            groups.numbers.clear();
            groups.insert(&by, rollup);
        }
        let mut cell_loop = CellLoop {
            rollup,
            by: &mut by,
            groups: &mut groups,
            filter: &mut filter,
            measures: &mut measures,
        };
        let expander = match read {
            Read::Bound(_) => None,
            Read::Keyed(_) => self.expander(fact),
        };
        match expander {
            None => {
                // The bound cells hold only what the joins keep.
                let mut joins = match read {
                    Read::Bound(_) => Vec::new(),
                    Read::Keyed(_) => self.join_checks(fact, rollup),
                };
                ask_once(&mut cell_loop, &mut joins, cover(rollup, ask, &loaded));
            }
            Some(mut expander) => {
                ask_expanded(&mut cell_loop, cover(rollup, ask, &loaded), &mut expander);
            }
        }
        let rows = answer_rows(groups, &mut measures, by.len());
        Ok(Answer { columns, rows })
    }

    /// Only the load knows the types, so this check comes after
    /// [`Spec::check`].
    fn check_types(&self, fact: &Fact, read: &Read<'_>, ask: &Ask) -> Result<(), Vec<AskError>> {
        let mut wrong = Vec::new();
        for condition in &ask.filter {
            let lookup = self.lookup(fact, read, &condition.key);
            let column = lookup.column_type(read.rollup());
            for value in &condition.values {
                if !column.fits(value) {
                    wrong.push(AskError::WrongType {
                        key: condition.key.clone(),
                        column,
                        value: value.clone(),
                    });
                }
            }
        }
        if wrong.is_empty() { Ok(()) } else { Err(wrong) }
    }

    /// How a cell's joins repeat it, when any can: a key with several rows
    /// under a left or inner join, or a join through another dim. `None`
    /// when every cell counts once or not at all, which
    /// [`Store::join_checks`] tells apart.
    fn expander(&self, fact: &Fact) -> Option<Expander<'_>> {
        let plans: Vec<RefPlan<'_>> = (fact.refs.iter())
            .map(|reference| self.ref_plan(fact, reference))
            .collect();
        if plans.iter().all(RefPlan::counts_once) {
            return None;
        }
        let mut parts = Vec::new();
        for via in plans.iter().flat_map(|plan| &plan.via) {
            if let ViaFrom::KeyPart(part) = via
                && !parts.contains(part)
            {
                parts.push(*part);
            }
        }
        Some(Expander {
            plans,
            parts,
            memo: FastMap::default(),
        })
    }

    fn ref_plan(&self, fact: &Fact, reference: &Ref) -> RefPlan<'_> {
        let mut via = Vec::with_capacity(reference.via.len());
        for column in &reference.via {
            via.push(self.via_from(fact, column));
        }
        RefPlan {
            kind: reference.kind,
            projection: self.projection(reference.dim.as_str()),
            via,
        }
    }

    fn via_from(&self, fact: &Fact, via: &Via) -> ViaFrom {
        match via {
            Via::Fact(column) => ViaFrom::KeyPart(key_part_of(fact, KeyPart::Column(column))),
            Via::Attribute { through, column } => {
                let parent = self.spec.dim(fact.refs[*through].dim.as_str());
                ViaFrom::Through {
                    reference: *through,
                    attribute: AttributeIndex(position_of(
                        &parent.expect(CHECKED).attributes,
                        column,
                    )),
                }
            }
        }
    }

    /// The joins that keep or drop a whole cell, for a view that counts
    /// each cell once: every ref but the left ones, which keep them all.
    fn join_checks<'s>(&'s self, fact: &Fact, rollup: &Rollup) -> Vec<JoinCheck<'s>> {
        (fact.refs.iter())
            .filter(|reference| reference.kind != RefKind::Left)
            .map(|reference| JoinCheck {
                joined: self.joined(fact, reference, rollup),
                keeps_found: reference.kind != RefKind::Anti,
            })
            .collect()
    }

    /// How a cell's key finds a ref's dim rows, through the rollup-key
    /// parts of its fact-side via columns.
    fn joined<'s>(&'s self, fact: &Fact, reference: &Ref, rollup: &Rollup) -> Joined<'s> {
        let mut via = Vec::with_capacity(reference.via.len());
        for column in &reference.via {
            if let Via::Fact(column) = column {
                via.push(key_part_of(fact, KeyPart::Column(column)));
            }
        }
        let memo = match via[..] {
            [part] => Memo::Dense(vec![UNKNOWN; rollup.codes(part)]),
            _ => Memo::Sparse(FastMap::default()),
        };
        Joined {
            projection: self.projection(reference.dim.as_str()),
            via,
            key: Vec::new(),
            tuple: Vec::new(),
            memo,
        }
    }

    fn fact_index(&self, name: &str) -> Option<FactIndex> {
        for (position, fact) in self.spec.facts().iter().enumerate() {
            if fact.name.as_str() == name {
                return Some(FactIndex(position));
            }
        }
        None
    }

    fn dim_index(&self, name: &str) -> Option<DimIndex> {
        dim_index(&self.spec, name)
    }

    fn rollup(&self, fact: &str) -> Option<(&Fact, &Rollup)> {
        let index = self.fact_index(fact)?;
        Some((&self.spec.facts()[index.0], &self.rollups[index.0]))
    }

    fn projection(&self, dim: &str) -> &Projection {
        &self.projections[self.dim_index(dim).expect(IN_SPEC).0]
    }

    /// The ask's fact and the cells that answer it: the bound cells,
    /// unless a sealed `date_bin` bucket overlaps the range. The bound
    /// cells hold nothing of a sealed bucket at any width, so that ask
    /// reads the keyed cells for the whole range.
    fn reading(&self, ask: &Ask) -> Result<(&Fact, Read<'_>), Vec<AskError>> {
        let fact = self.spec.check_ask(ask)?;
        let index = self.fact_index(fact.name.as_str()).expect(CHECKED);
        let keyed = &self.rollups[index.0];
        let read = match &self.bound[index.0] {
            Some(bound) if !keyed.overlaps_sealed(ask.from, ask.to) => Read::Bound(&bound.rollup),
            _ => Read::Keyed(keyed),
        };
        Ok((fact, read))
    }

    /// Where a key's values come from: a part of the cells read, or a
    /// dim's projection reached through a ref.
    fn lookup(&self, fact: &Fact, read: &Read<'_>, name: &Identifier) -> Lookup<'_> {
        let rollup = match read {
            Read::Bound(_) => return Lookup::KeyPart(bound_part_of(fact, name)),
            Read::Keyed(rollup) => rollup,
        };
        match fact.resolve(name).expect(CHECKED) {
            Key::ByKey(by) => Lookup::KeyPart(key_part_of(fact, by.key_part())),
            Key::Attribute(reference, attribute) => {
                let dim = self.spec.dim(reference.dim.as_str()).expect(CHECKED);
                let at = (fact.refs.iter()).position(|other| other.alias == reference.alias);
                Lookup::Attribute(AttributeLookup {
                    reference: at.expect(CHECKED),
                    attribute: AttributeIndex(position_of(&dim.attributes, &attribute.column)),
                    joined: self.joined(fact, reference, rollup),
                })
            }
        }
    }

    fn condition_checks<'a>(
        &self,
        fact: &Fact,
        read: &Read<'_>,
        filter: &'a [Condition],
    ) -> Vec<ConditionCheck<'_, 'a>> {
        let mut checks = Vec::with_capacity(filter.len());
        for condition in filter {
            let lookup = self.lookup(fact, read, &condition.key);
            checks.push(ConditionCheck {
                memo: vec![Known::Unknown; lookup.bound(read.rollup())],
                lookup,
                values: &condition.values,
            });
        }
        checks
    }
}

/// The cells an ask reads: a fact's bound cells, where every key the
/// view names is a part of the cell key, or its keyed cells, where a dim
/// column is read through the projection.
enum Read<'s> {
    Bound(&'s Rollup),
    Keyed(&'s Rollup),
}

impl<'s> Read<'s> {
    fn rollup(&self) -> &'s Rollup {
        match self {
            Read::Bound(rollup) | Read::Keyed(rollup) => rollup,
        }
    }
}

/// What the cell loops fill in.
struct CellLoop<'e, 's, 'a> {
    rollup: &'s Rollup,
    by: &'e mut [Grouping<'s>],
    groups: &'e mut Groups<'s>,
    filter: &'e mut [ConditionCheck<'s, 'a>],
    measures: &'e mut [Asked<'s, 'a>],
}

/// The cell loop for a view that counts each cell once or not at all:
/// one row a key at most, none joined through another dim. `joins` drop
/// the cells an inner, semi or anti join does.
fn ask_once<'s>(
    cell_loop: &mut CellLoop<'_, 's, '_>,
    joins: &mut [JoinCheck<'s>],
    cover: impl Iterator<Item = (i64, &'s Cells)>,
) {
    let rollup = cell_loop.rollup;
    // Per cell of a bucket: 1 while it may pass, then its group; 0 for
    // cells passed over. Again per measure with a filter of its own.
    let mut cell_groups = Vec::new();
    let mut measure_groups = Vec::new();
    for (bucket, cells) in cover {
        cell_loop
            .by
            .iter_mut()
            .for_each(|grouping| grouping.enter(bucket));
        cell_groups.clear();
        cell_groups.resize(cells.len(), 1);
        for join in joins.iter_mut() {
            join.pass(rollup, cells, &mut cell_groups);
        }
        for test in cell_loop.filter.iter_mut() {
            test.pass(rollup, cells, &mut cell_groups);
        }
        cell_loop
            .groups
            .assign(cell_loop.by, rollup, cells, &mut cell_groups);
        for measure in cell_loop.measures.iter_mut() {
            measure.partials.grow(cell_loop.groups.len());
            let read = if measure.filter.is_empty() {
                &cell_groups
            } else {
                measure_groups.clone_from(&cell_groups);
                for test in &mut measure.filter {
                    test.pass(rollup, cells, &mut measure_groups);
                }
                &measure_groups
            };
            let column = measure.state.map(|state| cells.state(state));
            measure.partials.merge(read, cells.rows(), column);
        }
    }
}

/// The cell loop for a view whose joins repeat a cell: each cell once per
/// join row its joins resolve it to.
fn ask_expanded<'s>(
    cell_loop: &mut CellLoop<'_, 's, '_>,
    cover: impl Iterator<Item = (i64, &'s Cells)>,
    expander: &mut Expander<'s>,
) {
    let rollup = cell_loop.rollup;
    for (bucket, cells) in cover {
        cell_loop
            .by
            .iter_mut()
            .for_each(|grouping| grouping.enter(bucket));
        for cell in 0..cells.len() {
            for row in expander.expand(rollup, cells.key(cell)) {
                if !cell_loop
                    .filter
                    .iter_mut()
                    .all(|test| test.holds(rollup, cells, cell, row))
                {
                    continue;
                }
                let groups = &mut *cell_loop.groups;
                groups.numbers.clear();
                let numbers =
                    (cell_loop.by.iter()).map(|grouping| grouping.number_in(cells, cell, row));
                groups.numbers.extend(numbers);
                let group = groups.insert(cell_loop.by, rollup);
                for measure in cell_loop.measures.iter_mut() {
                    measure.partials.grow(groups.len());
                    if (measure.filter.iter_mut()).all(|test| test.holds(rollup, cells, cell, row))
                    {
                        let column = measure.state.map(|state| cells.state(state));
                        measure
                            .partials
                            .merge_cell(group, cell, cells.rows(), column);
                    }
                }
            }
        }
    }
}

/// One row per group met, sorted by its keys.
fn answer_rows(
    groups: Groups<'_>,
    measures: &mut [Asked<'_, '_>],
    key_count: usize,
) -> Vec<AnswerRow> {
    let group_count = groups.len();
    for measure in measures.iter_mut() {
        measure.partials.grow(group_count);
    }
    let mut keys = groups.keys.into_iter().map(Cow::into_owned);
    let mut rows = Vec::with_capacity(groups.made);
    for group in 1..=groups.made {
        let values = (measures.iter_mut())
            .map(|measure| measure.partials.finish(group))
            .collect();
        rows.push(AnswerRow {
            keys: keys.by_ref().take(key_count).collect(),
            values,
        });
    }
    rows.sort_by(|left, right| left.keys.cmp(&right.keys));
    rows
}

/// The `by` keys, then the measures, as the ask names them.
fn answer_columns(
    ask: &Ask,
    rollup: &Rollup,
    by: &[Grouping<'_>],
    measures: &[&Measure],
) -> Vec<AnswerColumn> {
    let keys = (ask.keys.iter().zip(by)).map(|(key, grouping)| AnswerColumn {
        name: key.to_string(),
        scale: grouping.column_type(rollup).scale(),
        is_time: matches!(key, AskKey::Bucket),
    });
    let values = (ask.measures.iter().zip(measures)).map(|(name, measure)| AnswerColumn {
        name: name.to_string(),
        scale: rollup.scale_of(&measure.aggregate),
        is_time: false,
    });
    keys.chain(values).collect()
}

fn sealed_error(id: &SealedId, reason: &'static str) -> SealedError {
    SealedError {
        id: id.clone(),
        reason,
    }
}

fn sealed_ids(fact: &Fact, buckets: &[BucketId]) -> Vec<SealedId> {
    let mut ids = Vec::with_capacity(buckets.len());
    for bucket in buckets {
        ids.push(SealedId {
            fact: fact.name.clone(),
            width: bucket.width,
            bucket_start: bucket.start,
        });
    }
    ids
}

fn dim_index(spec: &Spec, name: &str) -> Option<DimIndex> {
    for (position, dim) in spec.dims().iter().enumerate() {
        if dim.name.as_str() == name {
            return Some(DimIndex(position));
        }
    }
    None
}

fn key_part_of(fact: &Fact, part: KeyPart<'_>) -> PartIndex {
    for (position, key) in fact.rollup_key().enumerate() {
        if key == part {
            return PartIndex(position);
        }
    }
    panic!("{CHECKED}")
}

/// The part of the bound cell key the view names so: the `by` keys'
/// parts, then the attributes in [`Fact::attributes`] order, as
/// [`Rollup::bound`] lays them.
fn bound_part_of(fact: &Fact, name: &Identifier) -> PartIndex {
    match fact.resolve(name).expect(CHECKED) {
        Key::ByKey(by) => {
            for (position, part) in fact.by_parts().enumerate() {
                if part == by.key_part() {
                    return PartIndex(position);
                }
            }
            panic!("{CHECKED}")
        }
        Key::Attribute(reference, attribute) => {
            let by_count = fact.by_parts().count();
            for (position, (other, read)) in fact.attributes().enumerate() {
                if other.alias == reference.alias && read.name == attribute.name {
                    return PartIndex(by_count + position);
                }
            }
            panic!("{CHECKED}")
        }
    }
}

fn buckets<'r>(rollup: &'r Rollup, ask: &Ask) -> impl Iterator<Item = CoveredBucket<'r>> {
    let mut cover = Cover::WidestFit;
    for key in &ask.keys {
        if *key == AskKey::Bucket {
            cover = Cover::EachDateBin;
        }
    }
    rollup.cover(ask.from, ask.to, cover)
}

/// The cells of a covered bucket, a sealed one's from `loaded`.
fn cells_of<'r>(covered: CoveredBucket<'r>, loaded: &ThawedCells<'r>) -> &'r Cells {
    match covered.bucket.cells() {
        Some(cells) => cells,
        None => loaded
            .get(&covered.id)
            .expect("ask_with is given every bucket sealed_read names"),
    }
}

/// Each bucket's start and cells, a sealed one's from `loaded`.
fn cover<'r>(
    rollup: &'r Rollup,
    ask: &Ask,
    loaded: &'r ThawedCells<'r>,
) -> impl Iterator<Item = (i64, &'r Cells)> {
    buckets(rollup, ask).map(|covered| (covered.id.start, cells_of(covered, loaded)))
}

/// A join row: one way a cell's joins resolve, per ref the dim row
/// matched, or none for a left join with no match and for a semi or anti
/// join, which keep no row.
type JoinRow<'s> = Vec<Option<&'s [u32]>>;

/// The attribute rows one dim key held, used in place of the projection.
#[derive(Clone, Copy)]
struct Overlay<'a> {
    dim: DimIndex,
    key: &'a [Datum],
    rows: &'a [Box<[u32]>],
}

fn matched<'a>(
    projections: &'a [Projection],
    reference: &BoundRef,
    via: &[Datum],
    overlay: Option<Overlay<'a>>,
) -> &'a [Box<[u32]>] {
    if let Some(overlay) = overlay
        && overlay.dim == reference.dim
        && overlay.key == via
    {
        return overlay.rows;
    }
    projections[reference.dim.0].matches(via)
}

/// Whether `cell` joins `key` of `dim` through any ref.
fn cell_hits(
    bound: &Bound,
    keyed: &Rollup,
    cells: &Cells,
    cell: usize,
    dim: DimIndex,
    key: &[Datum],
) -> bool {
    bound.refs.iter().any(|reference| {
        reference.dim == dim
            && reference.via.len() == key.len()
            && (reference.via_parts.iter().zip(key))
                .all(|(&part, datum)| keyed.value(part, cells.code(cell, part)) == datum)
    })
}

/// Moves every cell of a hot keyed bucket into the bound cells its joins
/// resolve to now, or back out of them.
fn pour(
    bound: &mut Bound,
    keyed: &Rollup,
    projections: &[Projection],
    start: Option<i64>,
    cells: &Cells,
    direction: Direction,
) -> Result<(), RowError> {
    for cell in 0..cells.len() {
        let count = cells.rows().get(cell);
        for key in &bound_keys(bound, keyed, cells, cell, projections, None) {
            (bound.rollup).adjust(start, key, count, cells.states(), cell, direction)?;
        }
    }
    Ok(())
}

/// The bound-key codes of each join row `cell` resolves to. `overlay`
/// supplies one key's rows; `None` reads the projections as they are.
fn bound_keys(
    bound: &mut Bound,
    keyed: &Rollup,
    cells: &Cells,
    cell: usize,
    projections: &[Projection],
    overlay: Option<Overlay<'_>>,
) -> Vec<Vec<u32>> {
    let via_keys: Vec<Vec<Datum>> = (bound.refs.iter())
        .map(|reference| {
            (reference.via_parts.iter())
                .map(|&part| keyed.value(part, cells.code(cell, part)).clone())
                .collect()
        })
        .collect();
    let join_rows = bound.join_rows(projections, &via_keys, overlay);
    let mut keys = Vec::with_capacity(join_rows.len());
    for join_row in &join_rows {
        let mut key = Vec::with_capacity(bound.by_len + join_row.len());
        for part in 0..bound.by_len {
            let part = PartIndex(part);
            let datum = keyed.value(part, cells.code(cell, part));
            key.push(bound.rollup.code_part(part, datum));
        }
        let mut part = bound.by_len;
        for (reference, found) in bound.refs.iter().zip(join_row) {
            for &attribute in &reference.attributes {
                let datum = match found {
                    Some(codes) => {
                        projections[reference.dim.0].attribute_value(attribute, codes[attribute.0])
                    }
                    None => &Datum::Null,
                };
                key.push(bound.rollup.code_part(PartIndex(part), datum));
                part += 1;
            }
        }
        keys.push(key);
    }
    keys
}

/// Enumerates how each cell's joins resolve, once per distinct tuple of
/// its join codes.
struct Expander<'s> {
    plans: Vec<RefPlan<'s>>,
    /// The rollup-key parts the refs read, each once.
    parts: Vec<PartIndex>,
    memo: FastMap<Box<[u32]>, Vec<JoinRow<'s>>>,
}

struct RefPlan<'s> {
    kind: RefKind,
    projection: &'s Projection,
    via: Vec<ViaFrom>,
}

impl RefPlan<'_> {
    /// Whether the ref keeps or drops a cell without repeating it: its key
    /// finds one row at most, or its join keeps no row, and its key comes
    /// from the cell alone.
    fn counts_once(&self) -> bool {
        let repeats = match self.kind {
            RefKind::Left | RefKind::Inner => !self.projection.is_unique(),
            RefKind::Semi | RefKind::Anti => false,
        };
        let from_cell = (self.via.iter()).all(|via| matches!(via, ViaFrom::KeyPart(_)));
        !repeats && from_cell
    }
}

/// Where a ref's key column comes from: a cell's part, or an attribute
/// of a ref before it.
enum ViaFrom {
    KeyPart(PartIndex),
    Through {
        /// Into [`Fact::refs`].
        reference: usize,
        attribute: AttributeIndex,
    },
}

impl<'s> Expander<'s> {
    fn expand(&mut self, rollup: &Rollup, key: &[u32]) -> &[JoinRow<'s>] {
        let tuple: Box<[u32]> = self.parts.iter().map(|&part| key[part.0]).collect();
        if !self.memo.contains_key(&tuple) {
            let rows = self.resolve(rollup, key);
            self.memo.insert(tuple.clone(), rows);
        }
        &self.memo[&tuple]
    }

    /// Every join row the refs resolve a cell to, in ref order: a ref's key
    /// comes from the cell or from a ref before it, and its kind says what
    /// of the rows matched goes on.
    fn resolve(&self, rollup: &Rollup, key: &[u32]) -> Vec<JoinRow<'s>> {
        let mut rows: Vec<JoinRow<'s>> = vec![Vec::new()];
        let mut dim_key = Vec::new();
        for plan in &self.plans {
            let mut next = Vec::new();
            for row in &rows {
                dim_key.clear();
                for via in &plan.via {
                    dim_key.push(match *via {
                        ViaFrom::KeyPart(part) => rollup.value(part, key[part.0]).clone(),
                        ViaFrom::Through {
                            reference,
                            attribute,
                        } => match row[reference] {
                            Some(found) => (self.plans[reference].projection)
                                .attribute_value(attribute, found[attribute.0]),
                            None => &Datum::Null,
                        }
                        .clone(),
                    });
                }
                keep(plan.kind, plan.projection.matches(&dim_key), row, &mut next);
            }
            rows = next;
        }
        rows
    }
}

/// What a join of `kind` keeps of `row`, a join row the refs before it
/// resolved, given the dim rows `matches` under its key: pushed onto
/// `next` once per row kept, as SQL's join kinds repeat or drop a row.
fn keep<'s>(
    kind: RefKind,
    matches: &'s [Box<[u32]>],
    row: &JoinRow<'s>,
    next: &mut Vec<JoinRow<'s>>,
) {
    match kind {
        RefKind::Left if matches.is_empty() => next.push(extended(row, None)),
        RefKind::Left | RefKind::Inner => {
            next.extend(matches.iter().map(|found| extended(row, Some(found))));
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

fn extended<'s>(row: &JoinRow<'s>, found: Option<&'s [u32]>) -> JoinRow<'s> {
    let mut row = row.clone();
    row.push(found);
    row
}

/// Where a key's values come from.
enum Lookup<'s> {
    KeyPart(PartIndex),
    Attribute(AttributeLookup<'s>),
}

/// A dim value read through a ref.
struct AttributeLookup<'s> {
    /// Into [`Fact::refs`].
    reference: usize,
    attribute: AttributeIndex,
    joined: Joined<'s>,
}

/// A ref's dim rows as a cell's key finds them, in a view that counts
/// each cell once: what is read of them is worked out once per tuple of
/// the cell's via codes.
struct Joined<'s> {
    projection: &'s Projection,
    /// The rollup-key parts of the ref's fact-side via columns, in the
    /// dim's key order.
    via: Vec<PartIndex>,
    /// The key of one lookup, kept to spare an allocation per lookup.
    key: Vec<Datum>,
    /// The via codes of one lookup, kept likewise.
    tuple: Vec<u32>,
    memo: Memo,
}

/// A via code not looked up yet.
const UNKNOWN: u32 = u32::MAX;

/// What was read per via code tuple: by the one code when the ref joins
/// on one column, else by the tuple.
enum Memo {
    Dense(Vec<u32>),
    Sparse(FastMap<Box<[u32]>, u32>),
}

impl Joined<'_> {
    /// What `read` makes of the rows under the key of the cell with
    /// `key`, below [`UNKNOWN`].
    fn read(
        &mut self,
        rollup: &Rollup,
        key: &[u32],
        read: impl FnOnce(&[Box<[u32]>]) -> u32,
    ) -> u32 {
        let known = match &mut self.memo {
            Memo::Dense(codes) => &mut codes[key[self.via[0].0] as usize],
            Memo::Sparse(map) => {
                self.tuple.clear();
                self.tuple.extend(self.via.iter().map(|&part| key[part.0]));
                // Looked up before inserting, so only a new tuple allocates.
                if !map.contains_key(self.tuple.as_slice()) {
                    map.insert(self.tuple.as_slice().into(), UNKNOWN);
                }
                map.get_mut(self.tuple.as_slice()).expect("inserted above")
            }
        };
        if *known == UNKNOWN {
            self.key.clear();
            (self.key).extend(
                self.via
                    .iter()
                    .map(|&part| rollup.value(part, key[part.0]).clone()),
            );
            *known = read(self.projection.matches(&self.key));
        }
        *known
    }
}

/// A join that keeps or drops a whole cell, in a view that counts each
/// cell once: an inner or semi join keeps the cells whose key finds a
/// row, an anti join the ones whose key finds none.
struct JoinCheck<'s> {
    joined: Joined<'s>,
    keeps_found: bool,
}

impl JoinCheck<'_> {
    /// Selects, not branches, as [`ConditionCheck::pass`] does.
    fn pass(&mut self, rollup: &Rollup, cells: &Cells, groups: &mut [u32]) {
        let keeps_found = self.keeps_found;
        for (cell, group) in groups.iter_mut().enumerate() {
            let kept = (self.joined).read(rollup, cells.key(cell), |rows| {
                u32::from(rows.is_empty() != keeps_found)
            });
            *group = if kept == 1 { *group } else { 0 };
        }
    }
}

impl AttributeLookup<'_> {
    /// The attribute's code for the cell with `key`: the one row under the
    /// key, or [`NULL`].
    fn code(&mut self, rollup: &Rollup, key: &[u32]) -> u32 {
        let attribute = self.attribute;
        (self.joined).read(rollup, key, |rows| {
            rows.first().map_or(NULL, |row| row[attribute.0])
        })
    }

    /// The attribute's code under one join row.
    fn code_in(&self, row: &JoinRow<'_>) -> u32 {
        row[self.reference].map_or(NULL, |found| found[self.attribute.0])
    }
}

impl<'s> Lookup<'s> {
    /// The code of the key's value for `cell`, in a view that counts each
    /// cell once.
    fn code(&mut self, rollup: &Rollup, cells: &Cells, cell: usize) -> u32 {
        match self {
            Lookup::KeyPart(part) => cells.code(cell, *part),
            Lookup::Attribute(lookup) => lookup.code(rollup, cells.key(cell)),
        }
    }

    /// The code of the key's value for `cell` under one join row.
    fn code_in(&self, cells: &Cells, cell: usize, row: &JoinRow<'_>) -> u32 {
        match self {
            Lookup::KeyPart(part) => cells.code(cell, *part),
            Lookup::Attribute(lookup) => lookup.code_in(row),
        }
    }

    /// Borrowed from the rollup or the projection, which outlive the ask.
    fn value<'r>(&self, rollup: &'r Rollup, code: u32) -> &'r Datum
    where
        's: 'r,
    {
        match self {
            Lookup::KeyPart(part) => rollup.value(*part, code),
            Lookup::Attribute(lookup) => lookup
                .joined
                .projection
                .attribute_value(lookup.attribute, code),
        }
    }

    /// Codes the key's values have.
    fn bound(&self, rollup: &Rollup) -> usize {
        match self {
            Lookup::KeyPart(part) => rollup.codes(*part),
            Lookup::Attribute(lookup) => lookup.joined.projection.attribute_codes(lookup.attribute),
        }
    }

    fn column_type(&self, rollup: &Rollup) -> ColumnType {
        match self {
            Lookup::KeyPart(part) => rollup.key_type(*part),
            Lookup::Attribute(lookup) => lookup.joined.projection.attribute_type(lookup.attribute),
        }
    }
}

/// A [`Condition`] of an ask, worked out once per code of its key as the ask
/// meets the code.
struct ConditionCheck<'s, 'a> {
    lookup: Lookup<'s>,
    values: &'a [Literal],
    /// Per code of the key.
    memo: Vec<Known>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Known {
    Unknown,
    Holds,
    Fails,
}

impl ConditionCheck<'_, '_> {
    /// Whether a cell passes is as likely as not, so the loop selects rather
    /// than branches; only a code met first branches.
    fn pass(&mut self, rollup: &Rollup, cells: &Cells, groups: &mut [u32]) {
        for (cell, group) in groups.iter_mut().enumerate() {
            let code = self.lookup.code(rollup, cells, cell);
            let mut known = self.memo[code as usize];
            if known == Known::Unknown && *group != 0 {
                known = self.resolve(rollup, code);
            }
            *group = if known == Known::Holds { *group } else { 0 };
        }
    }

    /// Whether `cell` passes under one join row.
    fn holds(&mut self, rollup: &Rollup, cells: &Cells, cell: usize, row: &JoinRow<'_>) -> bool {
        let code = self.lookup.code_in(cells, cell, row);
        let mut known = self.memo[code as usize];
        if known == Known::Unknown {
            known = self.resolve(rollup, code);
        }
        known == Known::Holds
    }

    fn resolve(&mut self, rollup: &Rollup, code: u32) -> Known {
        let value = self.lookup.value(rollup, code);
        let known = if self.values.iter().any(|literal| value.matches(literal)) {
            Known::Holds
        } else {
            Known::Fails
        };
        self.memo[code as usize] = known;
        known
    }
}

/// A by key of an ask, its values numbered without hashing any: a key's by
/// its codes, a time key's in time order.
enum Grouping<'s> {
    Lookup {
        lookup: Lookup<'s>,
        bound: usize,
    },
    Time {
        width: Width,
        /// The start of each time-key bucket met so far, the one at hand last.
        bucket_starts: Vec<i64>,
        /// The buckets the ask covers, the most it can meet.
        bound: usize,
    },
}

impl<'s> Grouping<'s> {
    fn lookup(lookup: Lookup<'s>, rollup: &Rollup) -> Grouping<'s> {
        Grouping::Lookup {
            bound: lookup.bound(rollup),
            lookup,
        }
    }

    fn time(width: Width, buckets: usize) -> Grouping<'s> {
        Grouping::Time {
            width,
            bucket_starts: Vec::new(),
            bound: buckets,
        }
    }

    /// Numbers the key's values have.
    fn bound(&self) -> usize {
        match self {
            Grouping::Lookup { bound, .. } | Grouping::Time { bound, .. } => *bound,
        }
    }

    fn column_type(&self, rollup: &Rollup) -> ColumnType {
        match self {
            Grouping::Lookup { lookup, .. } => lookup.column_type(rollup),
            Grouping::Time { .. } => rollup.time_type(),
        }
    }

    /// Buckets come in time order, so those of one time-key bucket come
    /// together.
    fn enter(&mut self, bucket: i64) {
        if let Grouping::Time {
            width,
            bucket_starts,
            ..
        } = self
        {
            let start = width.bucket_start(bucket);
            if bucket_starts.last() != Some(&start) {
                bucket_starts.push(start);
            }
        }
    }

    fn offsets(&mut self, rollup: &Rollup, cells: &Cells, stride: usize, at: &mut [usize]) {
        match self {
            Grouping::Time { bucket_starts, .. } => {
                let offset = (bucket_starts.len() - 1) * stride;
                for place in at.iter_mut() {
                    *place += offset;
                }
            }
            // Cells passed over too, as a branch costs more than their
            // entries, which nothing reads.
            Grouping::Lookup { lookup, .. } => {
                for (cell, place) in at.iter_mut().enumerate() {
                    *place += lookup.code(rollup, cells, cell) as usize * stride;
                }
            }
        }
    }

    fn number(&mut self, rollup: &Rollup, cells: &Cells, cell: usize) -> u32 {
        match self {
            Grouping::Lookup { lookup, .. } => lookup.code(rollup, cells, cell),
            Grouping::Time { bucket_starts, .. } => current(bucket_starts),
        }
    }

    fn number_in(&self, cells: &Cells, cell: usize, row: &JoinRow<'_>) -> u32 {
        match self {
            Grouping::Lookup { lookup, .. } => lookup.code_in(cells, cell, row),
            Grouping::Time { bucket_starts, .. } => current(bucket_starts),
        }
    }

    fn value<'r>(&self, rollup: &'r Rollup, number: u32) -> Cow<'r, Datum>
    where
        's: 'r,
    {
        match self {
            Grouping::Lookup { lookup, .. } => Cow::Borrowed(lookup.value(rollup, number)),
            Grouping::Time { bucket_starts, .. } => {
                Cow::Owned(Datum::Int(bucket_starts[number as usize]))
            }
        }
    }
}

/// The number of the time-key bucket at hand.
fn current(bucket_starts: &[i64]) -> u32 {
    u32::try_from(bucket_starts.len() - 1).expect("codes are u32")
}

/// A cell's group while [`Groups::assign`] has yet to look it up. Groups
/// number fewer than the cells, which fit in memory.
const NEW: u32 = u32::MAX;

/// Most entries a dense group table may have: 4 MiB of `u32`, of which an
/// ask touches only the pages its groups fall on.
const DENSE: usize = 1 << 20;

/// The groups an ask has met, numbered from 1 in the order it met them.
struct Groups<'a> {
    index: GroupIndex,
    strides: Vec<usize>,
    /// Each group's values, `by.len()` a group, group 1's first.
    keys: Vec<Cow<'a, Datum>>,
    /// Groups so far, not counting group 0.
    made: usize,
    numbers: Vec<u32>,
    at: Vec<usize>,
}

enum GroupIndex {
    /// Group per combination of by-key numbers, 0 for none yet.
    Dense(Vec<u32>),
    Sparse(FastMap<Box<[u32]>, u32>),
}

impl<'a> Groups<'a> {
    fn new(by: &[Grouping<'_>]) -> Groups<'a> {
        let mut strides = Vec::with_capacity(by.len());
        let size = by.iter().try_fold(1usize, |size, grouping| {
            strides.push(size);
            size.checked_mul(grouping.bound())
        });
        let index = match size {
            Some(size) if size <= DENSE => GroupIndex::Dense(vec![0; size]),
            _ => GroupIndex::Sparse(FastMap::default()),
        };
        Groups {
            index,
            strides,
            keys: Vec::new(),
            made: 0,
            numbers: Vec::with_capacity(by.len()),
            at: Vec::new(),
        }
    }

    /// Groups so far, counting group 0.
    fn len(&self) -> usize {
        self.made + 1
    }

    /// Without `by`, every nonzero `groups` entry is group 1 already.
    fn assign(
        &mut self,
        by: &mut [Grouping<'a>],
        rollup: &'a Rollup,
        cells: &'a Cells,
        groups: &mut [u32],
    ) {
        if by.is_empty() {
            return;
        }
        // Known groups in one tight pass, the rest after: few cells start
        // a group.
        let mut new = false;
        match &self.index {
            GroupIndex::Dense(table) => {
                self.at.clear();
                self.at.resize(groups.len(), 0);
                for (grouping, &stride) in by.iter_mut().zip(&self.strides) {
                    grouping.offsets(rollup, cells, stride, &mut self.at);
                }
                // Selects, not branches: under a filter, whether a cell
                // passed is as likely as not. `at` is in the table for
                // every cell.
                for (group, &at) in groups.iter_mut().zip(&self.at) {
                    let known = table[at];
                    new |= known == 0 && *group != 0;
                    let found = if known == 0 { NEW } else { known };
                    *group = if *group != 0 { found } else { 0 };
                }
            }
            GroupIndex::Sparse(_) => {
                new = true;
                groups
                    .iter_mut()
                    .filter(|group| **group != 0)
                    .for_each(|group| *group = NEW);
            }
        }
        if new {
            for (cell, group) in groups.iter_mut().enumerate() {
                if *group != NEW {
                    continue;
                }
                // A cell before it in the bucket may have made its group.
                *group = match &self.index {
                    GroupIndex::Dense(table) if table[self.at[cell]] != 0 => table[self.at[cell]],
                    _ => self.make(by, rollup, cells, cell),
                };
            }
        }
    }

    /// The group of `cell`, made if new.
    fn make(
        &mut self,
        by: &mut [Grouping<'a>],
        rollup: &'a Rollup,
        cells: &'a Cells,
        cell: usize,
    ) -> u32 {
        self.numbers.clear();
        self.numbers.extend(
            by.iter_mut()
                .map(|grouping| grouping.number(rollup, cells, cell)),
        );
        self.insert(by, rollup)
    }

    /// The group `numbers` names, made if new.
    fn insert(&mut self, by: &[Grouping<'a>], rollup: &'a Rollup) -> u32 {
        let next = u32::try_from(self.len()).expect("fewer groups than cells");
        let group = match &mut self.index {
            GroupIndex::Dense(table) => {
                let at: usize = (self.numbers.iter().zip(&self.strides))
                    .map(|(&number, stride)| number as usize * stride)
                    .sum();
                if table[at] == 0 {
                    table[at] = next;
                }
                table[at]
            }
            // Looked up before inserting, so only a new group allocates.
            GroupIndex::Sparse(map) => {
                if let Some(&group) = map.get(self.numbers.as_slice()) {
                    group
                } else {
                    map.insert(self.numbers.as_slice().into(), next);
                    next
                }
            }
        };
        if group == next {
            let values = (self.numbers.iter().zip(by))
                .map(|(&number, grouping)| grouping.value(rollup, number));

            self.keys.extend(values);
            self.made += 1;
        }
        group
    }
}

struct Asked<'s, 'a> {
    partials: Partials,
    state: Option<InputIndex>,
    filter: Vec<ConditionCheck<'s, 'a>>,
}
