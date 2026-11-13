use std::borrow::Cow;
use std::ops::Range;

use crate::aggregate::Partials;
use crate::ask::{Ask, AskError, AskKey, TimeKey};
use crate::hash::FastMap;
use crate::ident::Identifier;
use crate::projection::{NULL, Projection};
use crate::rollup::{Bucket, Cells, Rollup, RowError};
use crate::sealed::{Sealed, SealedError, SealedId, Thawed};
use crate::spec::{
    Condition, Fact, Key, KeyPart, Literal, Measure, Ref, RefKind, Spec, Via, Width,
};
use crate::value::{ColumnType, Datum, Value};

const CHECKED: &str = "checked by Spec::new or Spec::check";
const IN_SPEC: &str = "the table is in the spec";
const HOT: &str = "the bucket is hot";
const POURED: &str = "a bound cell takes any cell of its fact";

/// Thawed cells an ask reads, by width and start.
type ThawedCells<'t> = FastMap<(i64, i64), &'t Cells>;

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
    dim: usize,
    kind: RefKind,
    /// Positions in a fact row, in the dim's key order.
    via: Vec<usize>,
    /// Into the dim's attributes, in the order the ref reads them.
    attrs: Vec<usize>,
}

impl Bound {
    fn new(spec: &Spec, fact: &Fact) -> Option<Bound> {
        if fact.refs.is_empty() || !fact.is_direct() || !fact.vias_add_parts() {
            return None;
        }
        let columns = fact.columns();
        let position = |column: &Identifier| columns.iter().position(|c| c == column);
        let refs = (fact.refs.iter())
            .map(|r| {
                let dim = (spec.dims().iter())
                    .position(|d| d.name == r.dim)
                    .expect(CHECKED);
                let attributes = &spec.dims()[dim].attributes;
                BoundRef {
                    dim,
                    kind: r.kind,
                    via: (r.via.iter())
                        .map(|v| match v {
                            Via::Fact(column) => position(column).expect(CHECKED),
                            Via::Attribute { .. } => unreachable!("every ref is direct"),
                        })
                        .collect(),
                    attrs: (r.reads.iter())
                        .map(|a| {
                            attributes
                                .iter()
                                .position(|c| *c == a.column)
                                .expect(CHECKED)
                        })
                        .collect(),
                }
            })
            .collect();
        Some(Bound {
            rollup: Rollup::bound(fact),
            refs,
            width: columns.len() + fact.attributes().count(),
            by_len: fact.by_parts().count(),
        })
    }

    /// Appends to `out` one bound row per way the refs resolve `row`
    /// through the projections as they are now: the row, then the
    /// attributes read, null where a left join finds none.
    fn rows(&self, projections: &[Projection], row: &[Datum], out: &mut Vec<Datum>) {
        let via_keys: Vec<Vec<Datum>> = (self.refs.iter())
            .map(|r| r.via.iter().map(|&c| row[c].clone()).collect())
            .collect();
        for way in self.ways(projections, &via_keys, None) {
            out.extend_from_slice(row);
            for (r, found) in self.refs.iter().zip(&way) {
                let projection = &projections[r.dim];
                out.extend(r.attrs.iter().map(|&a| match found {
                    Some(codes) => projection.attribute_value(a, codes[a]).clone(),
                    None => Datum::Null,
                }));
            }
        }
    }

    /// One owned join per way the refs resolve `via_keys`. `overlay`
    /// stands in for one dim key, so a change can expand the rows it
    /// held before the projection moved.
    fn ways(
        &self,
        projections: &[Projection],
        via_keys: &[Vec<Datum>],
        overlay: Option<Overlay<'_>>,
    ) -> Vec<Way> {
        let mut joined: Vec<Way> = vec![Vec::new()];
        for (r, via) in self.refs.iter().zip(via_keys) {
            let matches = matched(projections, r, via, overlay);
            let mut next = Vec::new();
            for way in &joined {
                keep_owned(r.kind, matches, way, &mut next);
            }
            joined = next;
        }
        joined
    }

    fn reads(&self, dim: usize) -> bool {
        self.refs.iter().any(|r| r.dim == dim)
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
    /// A time key, whose values are the second its bucket bucket_starts.
    pub is_time: bool,
}

impl Store {
    pub fn new(spec: Spec) -> Store {
        Store {
            rollups: spec.facts().iter().map(Rollup::new).collect(),
            projections: spec.dims().iter().map(Projection::new).collect(),
            bound: (spec.facts().iter())
                .map(|fact| Bound::new(&spec, fact))
                .collect(),
            spec,
        }
    }

    /// Sets the types of a row of `table`, a fact's [`Fact::columns`] or
    /// a dim's [`Dim::columns`]. Only the source knows them, so a load
    /// sets them.
    pub fn set_types(&mut self, table: &str, types: &[ColumnType]) -> Result<(), RowError> {
        match self.member_mut(table) {
            Member::Rollup(rollup) => rollup.set_types(types),
            Member::Projection(projection) => projection.set_types(types),
        }?;
        // A bound row's types are the fact's and its dims', whichever
        // came last.
        let Store {
            spec,
            rollups,
            projections,
            bound,
        } = self;
        for ((fact, rollup), bound) in (spec.facts().iter().zip(rollups)).zip(bound) {
            let Some(bound) = bound else { continue };
            let mut types = rollup.types().to_vec();
            for (r, attr) in (fact.attributes()).map(|(r, _)| r).zip(bound_attrs(bound)) {
                let dim = (spec.dims().iter())
                    .position(|d| d.name == r.dim)
                    .expect(CHECKED);
                types.push(projections[dim].attribute_type(attr));
            }
            (bound.rollup.set_types(&types)).expect("a bound row is as wide as its types");
        }
        Ok(())
    }

    /// Adds rows laid end to end, as a load reads them, each a row of
    /// `table` wide.
    pub fn add_rows(&mut self, table: &str, rows: &[Datum]) -> Result<(), RowError> {
        if let Some(f) = self.fact_index(table) {
            self.rollups[f].add_rows(rows)?;
            let Some(bound) = &mut self.bound[f] else {
                return Ok(());
            };
            let row_len = self.rollups[f].row_len();
            let mut out = Vec::new();
            for row in rows.chunks_exact(row_len) {
                bound.rows(&self.projections, row, &mut out);
            }
            return bound.rollup.add_rows(&out);
        }
        let d = self.dim_index(table).expect(IN_SPEC);
        self.projections[d].add_rows(rows)?;
        self.rebuild_dim(d);
        Ok(())
    }

    /// Adds one row, as the stream brings it.
    pub fn add_row(&mut self, table: &str, row: &[Datum]) -> Result<(), RowError> {
        if let Some(f) = self.fact_index(table) {
            self.rollups[f].add(row)?;
            return self.bind(f, row, true);
        }
        let d = self.dim_index(table).expect(IN_SPEC);
        let key_len = self.projections[d].key_len();
        let key = row.get(..key_len).unwrap_or(&[]);
        let old = self.projections[d].row_codes(key);
        self.projections[d].add(row)?;
        let new_rows = self.projections[d].row_codes(key);
        self.retarget(d, key, &old, &new_rows)
    }

    /// Removes one row equal to the one given. A row never added is not
    /// found, so the caller starts over.
    pub fn remove_row(&mut self, table: &str, row: &[Datum]) -> Result<(), RowError> {
        if let Some(f) = self.fact_index(table) {
            self.rollups[f].remove(row)?;
            return self.bind(f, row, false);
        }
        let d = self.dim_index(table).expect(IN_SPEC);
        let key_len = self.projections[d].key_len();
        let key = row.get(..key_len).unwrap_or(&[]);
        let old = self.projections[d].row_codes(key);
        self.projections[d].remove(row)?;
        let new_rows = self.projections[d].row_codes(key);
        self.retarget(d, key, &old, &new_rows)
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
        if let Some(d) = self.dim_index(table)
            && old == new
        {
            return self.projections[d].check(old);
        }
        self.remove_row(table, old)?;
        self.add_row(table, new)
    }

    /// Drops every row of a table after a truncate.
    pub fn clear(&mut self, table: &str) {
        if let Some(f) = self.fact_index(table) {
            self.rollups[f].clear();
            if let Some(bound) = &mut self.bound[f] {
                bound.rollup.clear();
            }
            return;
        }
        let d = self.dim_index(table).expect(IN_SPEC);
        self.projections[d].clear();
        self.rebuild_dim(d);
    }

    /// Adds or removes a fact row's bound rows, as the refs resolve it
    /// now.
    fn bind(&mut self, f: usize, row: &[Datum], add: bool) -> Result<(), RowError> {
        let Some(bound) = &mut self.bound[f] else {
            return Ok(());
        };
        let mut out = Vec::new();
        bound.rows(&self.projections, row, &mut out);
        for row in out.chunks_exact(bound.width) {
            if add {
                bound.rollup.add(row)?;
            } else {
                bound.rollup.remove(row)?;
            }
        }
        Ok(())
    }

    /// Dim `d`'s `key` changed from `old` attribute rows to the ones it
    /// holds now. Bound cells whose join columns name `key` move; the
    /// rest stay.
    fn retarget(
        &mut self,
        d: usize,
        key: &[Datum],
        old: &[Box<[u32]>],
        new_rows: &[Box<[u32]>],
    ) -> Result<(), RowError> {
        if old == new_rows {
            return Ok(());
        }
        let facts: Vec<usize> = (self.bound.iter().enumerate())
            .filter(|(_, bound)| bound.as_ref().is_some_and(|bound| bound.reads(d)))
            .map(|(f, _)| f)
            .collect();
        for f in facts {
            self.retarget_fact(f, d, key, old)?;
        }
        Ok(())
    }

    fn retarget_fact(
        &mut self,
        f: usize,
        d: usize,
        key: &[Datum],
        old: &[Box<[u32]>],
    ) -> Result<(), RowError> {
        let Store {
            rollups,
            projections,
            bound,
            ..
        } = self;
        let Some(bound) = bound[f].as_mut() else {
            return Ok(());
        };
        let keyed = &rollups[f];
        let overlay = Overlay {
            dim: d,
            key,
            rows: old,
        };
        for start in keyed.hot_date_bin_starts() {
            let Some(cells) = keyed.hot_date_bin(start) else {
                continue;
            };
            for i in 0..cells.len() {
                if !cell_hits(bound, keyed, cells, i, d, key) {
                    continue;
                }
                let old_keys = bound_keys(bound, keyed, cells, i, projections, Some(overlay));
                let new_keys = bound_keys(bound, keyed, cells, i, projections, None);
                if old_keys == new_keys {
                    continue;
                }
                let count = cells.rows().get(i);
                for key in &old_keys {
                    bound
                        .rollup
                        .adjust(start, key, count, cells.states(), i, false)?;
                }
                for key in &new_keys {
                    bound
                        .rollup
                        .adjust(start, key, count, cells.states(), i, true)?;
                }
            }
        }
        Ok(())
    }

    /// The dim was replaced wholesale: pour the hot keyed cells into a
    /// fresh bound rollup.
    fn rebuild_dim(&mut self, d: usize) {
        let facts: Vec<usize> = (self.bound.iter().enumerate())
            .filter(|(_, bound)| bound.as_ref().is_some_and(|bound| bound.reads(d)))
            .map(|(f, _)| f)
            .collect();
        for f in facts {
            if self.rollups[f].cells() == 0 {
                continue;
            }
            self.rebuild_fact(f);
        }
    }

    fn rebuild_fact(&mut self, f: usize) {
        let Store {
            rollups,
            projections,
            bound,
            ..
        } = self;
        let Some(bound) = bound[f].as_mut() else {
            return;
        };
        bound.rollup.clear();
        let keyed = &rollups[f];
        for start in keyed.hot_date_bin_starts() {
            let cells = keyed.hot_date_bin(start).expect(HOT);
            pour(bound, keyed, projections, start, cells, true).expect(POURED);
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
        let f = self.fact_index(fact)?;
        Some(self.bound[f].as_ref()?.rollup.cells())
    }

    pub fn dim_rows(&self, dim: &str) -> Option<usize> {
        Some(self.projections[self.dim_index(dim)?].len())
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
            Key::Attribute(r, attribute) => {
                let dim = self.spec.dim(r.dim.as_str()).expect(CHECKED);
                let read = (dim.attributes.iter())
                    .position(|a| *a == attribute.column)
                    .expect(CHECKED);
                self.projection(r.dim.as_str()).attribute_type(read)
            }
        })
    }

    pub fn cells_read(&self, ask: &Ask) -> Result<usize, Vec<AskError>> {
        let (_, read) = self.reading(ask)?;
        Ok(buckets(read.rollup(), ask).map(|(_, _, b)| b.len()).sum())
    }

    /// Each `date_bin` bucket not sealed that ends by `before`, Unix
    /// seconds, every fact.
    pub fn sealable(&self, before: i64) -> Vec<SealedId> {
        (self.spec.facts().iter().zip(&self.rollups))
            .flat_map(|(fact, rollup)| sealed_ids(fact, rollup.sealable(before)))
            .collect()
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
        let error = |reason| SealedError {
            id: id.clone(),
            reason,
        };
        let f = (self.fact_index(id.fact.as_str())).ok_or_else(|| error("unknown fact"))?;
        self.pour_bucket(f, id.width, id.bucket_start, false)
            .map_err(|_| error("bound cells don't hold the bucket"))?;
        (self.rollups[f].seal(id.width, id.bucket_start)).map_err(error)
    }

    /// Binds a hot `date_bin` bucket's cells into the bound cells, or
    /// takes them back out, as the dims are now. A wider bucket has no
    /// rows of its own to bind.
    fn pour_bucket(&mut self, f: usize, width: i64, start: i64, add: bool) -> Result<(), RowError> {
        let Store {
            rollups,
            projections,
            bound,
            ..
        } = self;
        let (Some(bound), keyed) = (bound[f].as_mut(), &rollups[f]) else {
            return Ok(());
        };
        if width != keyed.date_bin_width() {
            return Ok(());
        }
        match keyed.hot_date_bin(Some(start)) {
            Some(cells) => pour(bound, keyed, projections, Some(start), cells, add),
            None => Ok(()),
        }
    }

    /// Every sealed bucket of a fact, which a truncate drops with the rest.
    pub fn sealed(&self, fact: &str) -> Vec<SealedId> {
        let (fact, rollup) = self.rollup(fact).expect(IN_SPEC);
        sealed_ids(fact, rollup.sealed())
    }

    /// The sealed buckets [`Store::ask_with`] needs for `ask`; none when
    /// the bound cells answer it, which are never sealed.
    pub fn sealed_read(&self, ask: &Ask) -> Result<Vec<SealedId>, Vec<AskError>> {
        let (fact, read) = self.reading(ask)?;
        let sealed = buckets(read.rollup(), ask)
            .filter(|(_, _, bucket)| bucket.cells().is_none())
            .map(|(width, start, _)| (width, start));
        Ok(sealed_ids(fact, sealed))
    }

    /// The sealed buckets a fact row falls in, which must be unsealed
    /// before the row is added or removed.
    pub fn sealed_touched(&self, fact: &str, row: &[Datum]) -> Vec<SealedId> {
        let (fact, rollup) = self.rollup(fact).expect(IN_SPEC);
        match row.first() {
            Some(time) => sealed_ids(fact, rollup.touched(time)),
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
        let f = self.fact_index(id.fact.as_str()).expect(IN_SPEC);
        match self.rollups[f].unseal(id.width, id.bucket_start, cells) {
            Ok(()) => {
                (self.pour_bucket(f, id.width, id.bucket_start, true)).expect(POURED);
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
        let loaded: ThawedCells<'_> = (thawed.iter())
            .filter(|t| t.id.fact == fact.name)
            .map(|t| ((t.id.width, t.id.bucket_start), &t.cells))
            .collect();
        let buckets = buckets(rollup, ask).count();
        let mut by: Vec<Grouping> = (ask.keys.iter())
            .map(|key| match key {
                AskKey::Key(name) => Grouping::lookup(self.lookup(fact, &read, name), rollup),
                AskKey::Time(t) => Grouping::time(*t, fact.time.width, buckets),
            })
            .collect();
        let mut groups = Groups::new(&by);
        let mut filter = self.tests(fact, &read, &fact.filter);
        filter.extend(self.tests(fact, &read, &ask.filter));
        let asked: Vec<&Measure> = (ask.measures.iter())
            .map(|name| fact.measure(name.as_str()).expect(CHECKED))
            .collect();
        let mut measures: Vec<Asked> = (asked.iter())
            .map(|m| Asked {
                partials: Partials::new(&m.aggregate),
                state: rollup.state_of(&m.aggregate),
                filter: self.tests(fact, &read, &m.filter),
            })
            .collect();
        let columns = answer_columns(ask, rollup, &by, &asked);

        if by.is_empty() {
            // As in SQL without GROUP BY: one row even when nothing matches.
            groups.numbers.clear();
            groups.insert(&by, rollup);
        }
        let mut each = Each {
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
                    Read::Keyed(_) => self.presences(fact, rollup),
                };
                ask_once(&mut each, &mut joins, cover(rollup, ask, &loaded));
            }
            Some(mut expander) => {
                ask_expanded(&mut each, cover(rollup, ask, &loaded), &mut expander)
            }
        }

        measures
            .iter_mut()
            .for_each(|m| m.partials.grow(groups.len()));
        let mut keys = groups.keys.into_iter().map(Cow::into_owned);
        let mut rows: Vec<AnswerRow> = (1..=groups.made)
            .map(|g| AnswerRow {
                keys: keys.by_ref().take(by.len()).collect(),
                values: measures.iter().map(|m| m.partials.finish(g)).collect(),
            })
            .collect();
        rows.sort_by(|a, b| a.keys.cmp(&b.keys));
        Ok(Answer { columns, rows })
    }

    /// Only the load knows the types, so this check comes after
    /// [`Spec::check`].
    fn check_types(&self, fact: &Fact, read: &Read<'_>, ask: &Ask) -> Result<(), Vec<AskError>> {
        let wrong: Vec<AskError> = (ask.filter.iter())
            .flat_map(|cond| {
                let column = self
                    .lookup(fact, read, &cond.key)
                    .column_type(read.rollup());
                (cond.values.iter())
                    .filter(move |value| !column.fits(value))
                    .map(move |value| AskError::WrongType {
                        key: cond.key.clone(),
                        column,
                        value: value.clone(),
                    })
            })
            .collect();
        if wrong.is_empty() { Ok(()) } else { Err(wrong) }
    }

    /// How a cell's joins repeat it, when any can: a key with several rows
    /// under a left or inner join, or a join through another dim. `None`
    /// when every cell counts once or not at all, which
    /// [`Store::presences`] tells apart.
    fn expander(&self, fact: &Fact) -> Option<Expander<'_>> {
        let plans: Vec<RefPlan<'_>> = (fact.refs.iter())
            .map(|r| {
                let via = (r.via.iter())
                    .map(|v| match v {
                        Via::Fact(column) => {
                            ViaFrom::KeyPart(key_part_of(fact, KeyPart::Column(column)))
                        }
                        Via::Attribute { through, column } => {
                            let parent = self.spec.dim(fact.refs[*through].dim.as_str());
                            let attr = (parent.expect(CHECKED).attributes.iter())
                                .position(|a| a == column)
                                .expect(CHECKED);
                            ViaFrom::Through { r: *through, attr }
                        }
                    })
                    .collect();
                RefPlan {
                    kind: r.kind,
                    projection: self.projection(r.dim.as_str()),
                    via,
                }
            })
            .collect();
        let once = plans.iter().all(|p| {
            let repeats = match p.kind {
                RefKind::Left | RefKind::Inner => !p.projection.is_unique(),
                RefKind::Semi | RefKind::Anti => false,
            };
            !repeats && p.via.iter().all(|v| matches!(v, ViaFrom::KeyPart(_)))
        });
        if once {
            return None;
        }
        let mut parts = Vec::new();
        for v in plans.iter().flat_map(|p| &p.via) {
            if let ViaFrom::KeyPart(part) = v
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

    /// The joins that keep or drop a whole cell, for a view that counts
    /// each cell once: every ref but the left ones, which keep them all.
    fn presences<'s>(&'s self, fact: &Fact, rollup: &Rollup) -> Vec<Presence<'s>> {
        (fact.refs.iter())
            .filter(|r| r.kind != RefKind::Left)
            .map(|r| Presence {
                joined: self.joined(fact, r, rollup),
                keeps_found: r.kind != RefKind::Anti,
            })
            .collect()
    }

    /// How a cell's key finds a ref's dim rows, through the rollup-key
    /// parts of its fact-side via columns.
    fn joined<'s>(&'s self, fact: &Fact, r: &Ref, rollup: &Rollup) -> Joined<'s> {
        let via: Vec<usize> = (r.via.iter())
            .filter_map(|v| match v {
                Via::Fact(column) => Some(key_part_of(fact, KeyPart::Column(column))),
                Via::Attribute { .. } => None,
            })
            .collect();
        let memo = match via[..] {
            [part] => Memo::Dense(vec![UNKNOWN; rollup.codes(part)]),
            _ => Memo::Sparse(FastMap::default()),
        };
        Joined {
            projection: self.projection(r.dim.as_str()),
            via,
            key: Vec::new(),
            tuple: Vec::new(),
            memo,
        }
    }

    fn fact_index(&self, name: &str) -> Option<usize> {
        self.spec
            .facts()
            .iter()
            .position(|f| f.name.as_str() == name)
    }

    fn dim_index(&self, name: &str) -> Option<usize> {
        self.spec
            .dims()
            .iter()
            .position(|d| d.name.as_str() == name)
    }

    fn rollup(&self, fact: &str) -> Option<(&Fact, &Rollup)> {
        let i = self.fact_index(fact)?;
        Some((&self.spec.facts()[i], &self.rollups[i]))
    }

    fn projection(&self, dim: &str) -> &Projection {
        &self.projections[self.dim_index(dim).expect(IN_SPEC)]
    }

    /// A fact's rollup or a dim's projection: dims and facts share one
    /// namespace, so the name says which.
    fn member_mut(&mut self, table: &str) -> Member<'_> {
        if let Some(i) = self.fact_index(table) {
            return Member::Rollup(&mut self.rollups[i]);
        }
        let i = self.dim_index(table).expect(IN_SPEC);
        Member::Projection(&mut self.projections[i])
    }

    /// The ask's fact and the cells that answer it: the bound cells,
    /// unless a sealed `date_bin` bucket overlaps the range. The bound
    /// cells hold nothing of a sealed bucket at any width, so that ask
    /// reads the keyed cells for the whole range.
    fn reading(&self, ask: &Ask) -> Result<(&Fact, Read<'_>), Vec<AskError>> {
        let fact = self.spec.check_ask(ask)?;
        let f = self.fact_index(fact.name.as_str()).expect(CHECKED);
        let keyed = &self.rollups[f];
        let read = match &self.bound[f] {
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
            Key::Attribute(r, attr) => {
                let dim = self.spec.dim(r.dim.as_str()).expect(CHECKED);
                Lookup::Attribute(AttributeLookup {
                    r: fact
                        .refs
                        .iter()
                        .position(|q| q.alias == r.alias)
                        .expect(CHECKED),
                    attr: dim
                        .attributes
                        .iter()
                        .position(|a| *a == attr.column)
                        .expect(CHECKED),
                    joined: self.joined(fact, r, rollup),
                })
            }
        }
    }

    fn tests<'a>(
        &self,
        fact: &Fact,
        read: &Read<'_>,
        conds: &'a [Condition],
    ) -> Vec<Match<'_, 'a>> {
        conds
            .iter()
            .map(|c| {
                let lookup = self.lookup(fact, read, &c.key);
                Match {
                    memo: vec![Known::Unknown; lookup.bound(read.rollup())],
                    lookup,
                    values: &c.values,
                }
            })
            .collect()
    }
}

enum Member<'s> {
    Rollup(&'s mut Rollup),
    Projection(&'s mut Projection),
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

/// The dim attribute each of a fact's bound refs reads, in
/// [`Fact::attributes`] order.
fn bound_attrs(bound: &Bound) -> impl Iterator<Item = usize> + '_ {
    bound.refs.iter().flat_map(|r| r.attrs.iter().copied())
}

/// What the cell loops fill in.
struct Each<'e, 's, 'a> {
    rollup: &'s Rollup,
    by: &'e mut [Grouping<'s>],
    groups: &'e mut Groups<'s>,
    filter: &'e mut [Match<'s, 'a>],
    measures: &'e mut [Asked<'s, 'a>],
}

/// The cell loop for a view that counts each cell once or not at all:
/// one row a key at most, none joined through another dim. `joins` drop
/// the cells an inner, semi or anti join does.
fn ask_once<'s>(
    each: &mut Each<'_, 's, '_>,
    joins: &mut [Presence<'s>],
    cover: impl Iterator<Item = (i64, &'s Cells)>,
) {
    let rollup = each.rollup;
    // Per cell of a bucket: 1 while it may pass, then its group; 0 for
    // cells passed over. Again per measure with a filter of its own.
    let mut cell_groups = Vec::new();
    let mut measure_groups = Vec::new();
    for (bucket, cells) in cover {
        each.by.iter_mut().for_each(|p| p.enter(bucket));
        cell_groups.clear();
        cell_groups.resize(cells.len(), 1);
        for join in joins.iter_mut() {
            join.pass(rollup, cells, &mut cell_groups);
        }
        for t in each.filter.iter_mut() {
            t.pass(rollup, cells, &mut cell_groups);
        }
        each.groups.assign(each.by, rollup, cells, &mut cell_groups);
        for m in each.measures.iter_mut() {
            m.partials.grow(each.groups.len());
            let read = if m.filter.is_empty() {
                &cell_groups
            } else {
                measure_groups.clone_from(&cell_groups);
                for t in &mut m.filter {
                    t.pass(rollup, cells, &mut measure_groups);
                }
                &measure_groups
            };
            m.partials
                .merge(read, cells.rows(), m.state.map(|i| cells.state(i)));
        }
    }
}

/// The cell loop for a view whose joins repeat a cell: each cell once per
/// way its joins resolve.
fn ask_expanded<'s>(
    each: &mut Each<'_, 's, '_>,
    cover: impl Iterator<Item = (i64, &'s Cells)>,
    expander: &mut Expander<'s>,
) {
    let rollup = each.rollup;
    for (bucket, cells) in cover {
        each.by.iter_mut().for_each(|p| p.enter(bucket));
        for i in 0..cells.len() {
            for row in expander.expand(rollup, cells.key(i)) {
                if !each
                    .filter
                    .iter_mut()
                    .all(|t| t.holds(rollup, cells, i, row))
                {
                    continue;
                }
                let groups = &mut *each.groups;
                groups.numbers.clear();
                (groups.numbers).extend(each.by.iter().map(|p| p.number_in(cells, i, row)));
                let g = groups.insert(each.by, rollup);
                for m in each.measures.iter_mut() {
                    m.partials.grow(groups.len());
                    if m.filter.iter_mut().all(|t| t.holds(rollup, cells, i, row)) {
                        let column = m.state.map(|s| cells.state(s));
                        m.partials.merge_cell(g, i, cells.rows(), column);
                    }
                }
            }
        }
    }
}

/// The `by` keys, then the measures, as the ask names them.
fn answer_columns(
    ask: &Ask,
    rollup: &Rollup,
    by: &[Grouping<'_>],
    measures: &[&Measure],
) -> Vec<AnswerColumn> {
    let keys = (ask.keys.iter().zip(by)).map(|(key, p)| AnswerColumn {
        name: key.to_string(),
        scale: p.column_type(rollup).scale(),
        is_time: matches!(key, AskKey::Time(_)),
    });
    let values = (ask.measures.iter().zip(measures)).map(|(name, m)| AnswerColumn {
        name: name.to_string(),
        scale: rollup.scale_of(&m.aggregate),
        is_time: false,
    });
    keys.chain(values).collect()
}

fn sealed_ids(fact: &Fact, buckets: impl Iterator<Item = (i64, i64)>) -> Vec<SealedId> {
    buckets
        .map(|(width, start)| SealedId {
            fact: fact.name.clone(),
            width,
            bucket_start: start,
        })
        .collect()
}

fn key_part_of(fact: &Fact, part: KeyPart<'_>) -> usize {
    fact.rollup_key().position(|k| k == part).expect(CHECKED)
}

/// The part of the bound cell key the view names so: the `by` keys'
/// parts, then the attributes in [`Fact::attributes`] order, as
/// [`Rollup::bound`] lays them.
fn bound_part_of(fact: &Fact, name: &Identifier) -> usize {
    match fact.resolve(name).expect(CHECKED) {
        Key::ByKey(by) => (fact.by_parts())
            .position(|part| part == by.key_part())
            .expect(CHECKED),
        Key::Attribute(r, attr) => {
            let at = (fact.attributes())
                .position(|(q, a)| q.alias == r.alias && a.name == attr.name)
                .expect(CHECKED);
            fact.by_parts().count() + at
        }
    }
}

fn buckets<'r>(rollup: &'r Rollup, ask: &Ask) -> impl Iterator<Item = (i64, i64, &'r Bucket)> {
    let each_bucket = ask.keys.iter().any(|key| matches!(key, AskKey::Time(_)));
    rollup.cover(ask.from, ask.to, each_bucket)
}

/// Each bucket's start and cells, a sealed one's from `loaded`.
fn cover<'r>(
    rollup: &'r Rollup,
    ask: &Ask,
    loaded: &'r ThawedCells<'r>,
) -> impl Iterator<Item = (i64, &'r Cells)> {
    buckets(rollup, ask).map(|(width, start, bucket)| {
        let cells = bucket.cells().unwrap_or_else(|| {
            loaded
                .get(&(width, start))
                .expect("ask_with is given every bucket sealed_read names")
        });
        (start, cells)
    })
}

/// One way a cell's joins resolve: per ref, the dim row matched, or none
/// for a left join with no match and for a semi or anti join, which keep
/// no row.
type JoinRow<'s> = Vec<Option<&'s [u32]>>;

/// As [`JoinRow`], owning its codes so a dim change can expand the rows
/// a key held before the projection moved.
type Way = Vec<Option<Box<[u32]>>>;

/// The attribute rows one dim key held, used in place of the projection.
#[derive(Clone, Copy)]
struct Overlay<'a> {
    dim: usize,
    key: &'a [Datum],
    rows: &'a [Box<[u32]>],
}

fn matched<'a>(
    projections: &'a [Projection],
    r: &BoundRef,
    via: &[Datum],
    overlay: Option<Overlay<'a>>,
) -> &'a [Box<[u32]>] {
    if let Some(overlay) = overlay
        && overlay.dim == r.dim
        && overlay.key == via
    {
        return overlay.rows;
    }
    projections[r.dim].matches(via)
}

fn keep_owned(kind: RefKind, matches: &[Box<[u32]>], way: &Way, next: &mut Vec<Way>) {
    let with = |found: Option<&[u32]>| {
        let mut way = way.clone();
        way.push(found.map(|codes| codes.to_vec().into_boxed_slice()));
        way
    };
    match kind {
        RefKind::Left if matches.is_empty() => next.push(with(None)),
        RefKind::Left | RefKind::Inner => {
            next.extend(matches.iter().map(|codes| with(Some(codes))));
        }
        RefKind::Semi if !matches.is_empty() => next.push(with(None)),
        RefKind::Anti if matches.is_empty() => next.push(with(None)),
        RefKind::Semi | RefKind::Anti => {}
    }
}

/// Whether cell `i` joins `key` of dim `d` through any ref.
fn cell_hits(
    bound: &Bound,
    keyed: &Rollup,
    cells: &Cells,
    i: usize,
    d: usize,
    key: &[Datum],
) -> bool {
    bound.refs.iter().any(|r| {
        r.dim == d
            && r.via.len() == key.len()
            && (r.via.iter().zip(key)).all(|(&column, datum)| {
                let part = keyed.part_of_column(column).expect(CHECKED);
                keyed.value(part, cells.code(i, part)) == datum
            })
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
    add: bool,
) -> Result<(), RowError> {
    for i in 0..cells.len() {
        let count = cells.rows().get(i);
        for key in &bound_keys(bound, keyed, cells, i, projections, None) {
            (bound.rollup).adjust(start, key, count, cells.states(), i, add)?;
        }
    }
    Ok(())
}

/// The bound-key codes of each way cell `i` resolves. `overlay` supplies
/// one key's rows; `None` reads the projections as they are.
fn bound_keys(
    bound: &mut Bound,
    keyed: &Rollup,
    cells: &Cells,
    i: usize,
    projections: &[Projection],
    overlay: Option<Overlay<'_>>,
) -> Vec<Vec<u32>> {
    let via_keys: Vec<Vec<Datum>> = (bound.refs.iter())
        .map(|r| {
            (r.via.iter())
                .map(|&column| {
                    let part = keyed.part_of_column(column).expect(CHECKED);
                    keyed.value(part, cells.code(i, part)).clone()
                })
                .collect()
        })
        .collect();
    let ways = bound.ways(projections, &via_keys, overlay);
    let mut keys = Vec::with_capacity(ways.len());
    for way in &ways {
        let mut key = Vec::with_capacity(bound.by_len + way.len());
        for part in 0..bound.by_len {
            let datum = keyed.value(part, cells.code(i, part));
            key.push(bound.rollup.code_part(part, datum));
        }
        let mut part = bound.by_len;
        for (r, found) in bound.refs.iter().zip(way) {
            for &attr in &r.attrs {
                let datum = match found {
                    Some(codes) => projections[r.dim].attribute_value(attr, codes[attr]),
                    None => &Datum::Null,
                };
                key.push(bound.rollup.code_part(part, datum));
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
    parts: Vec<usize>,
    memo: FastMap<Box<[u32]>, Vec<JoinRow<'s>>>,
}

struct RefPlan<'s> {
    kind: RefKind,
    projection: &'s Projection,
    via: Vec<ViaFrom>,
}

/// Where a ref's key column comes from: a cell's part, or an attribute
/// of a ref before it.
enum ViaFrom {
    KeyPart(usize),
    Through { r: usize, attr: usize },
}

impl<'s> Expander<'s> {
    fn expand(&mut self, rollup: &Rollup, key: &[u32]) -> &[JoinRow<'s>] {
        let tuple: Box<[u32]> = self.parts.iter().map(|&part| key[part]).collect();
        if !self.memo.contains_key(&tuple) {
            let rows = self.resolve(rollup, key);
            self.memo.insert(tuple.clone(), rows);
        }
        &self.memo[&tuple]
    }

    /// Every way the refs resolve for a cell, in ref order: a ref's key
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
                        ViaFrom::KeyPart(part) => rollup.value(part, key[part]).clone(),
                        ViaFrom::Through { r, attr } => match row[r] {
                            Some(found) => {
                                self.plans[r].projection.attribute_value(attr, found[attr])
                            }
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

/// What a join of `kind` keeps of `row`, one way the refs before it
/// resolved, given the dim rows `matches` under its key: pushed onto
/// `next` once per row kept, as SQL's join kinds repeat or drop a row.
fn keep<'s>(
    kind: RefKind,
    matches: &'s [Box<[u32]>],
    row: &JoinRow<'s>,
    next: &mut Vec<JoinRow<'s>>,
) {
    let with = |found: Option<&'s [u32]>| {
        let mut row = row.clone();
        row.push(found);
        row
    };
    match kind {
        RefKind::Left if matches.is_empty() => next.push(with(None)),
        RefKind::Left | RefKind::Inner => {
            next.extend(matches.iter().map(|m| with(Some(m))));
        }
        RefKind::Semi if !matches.is_empty() => next.push(with(None)),
        RefKind::Anti if matches.is_empty() => next.push(with(None)),
        RefKind::Semi | RefKind::Anti => {}
    }
}

/// Where a key's values come from.
enum Lookup<'s> {
    KeyPart(usize),
    Attribute(AttributeLookup<'s>),
}

/// A dim value read through a ref.
struct AttributeLookup<'s> {
    /// Into [`Fact::refs`].
    r: usize,
    /// Into the dim's attributes.
    attr: usize,
    joined: Joined<'s>,
}

/// A ref's dim rows as a cell's key finds them, in a view that counts
/// each cell once: what is read of them is worked out once per tuple of
/// the cell's via codes.
struct Joined<'s> {
    projection: &'s Projection,
    /// The rollup-key parts of the ref's fact-side via columns, in the
    /// dim's key order.
    via: Vec<usize>,
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
            Memo::Dense(codes) => &mut codes[key[self.via[0]] as usize],
            Memo::Sparse(map) => {
                self.tuple.clear();
                self.tuple.extend(self.via.iter().map(|&part| key[part]));
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
                    .map(|&part| rollup.value(part, key[part]).clone()),
            );
            *known = read(self.projection.matches(&self.key));
        }
        *known
    }
}

/// A join that keeps or drops a whole cell, in a view that counts each
/// cell once: an inner or semi join keeps the cells whose key finds a
/// row, an anti join the ones whose key finds none.
struct Presence<'s> {
    joined: Joined<'s>,
    keeps_found: bool,
}

impl Presence<'_> {
    /// Selects, not branches, as [`Match::pass`] does.
    fn pass(&mut self, rollup: &Rollup, cells: &Cells, groups: &mut [u32]) {
        let keeps_found = self.keeps_found;
        for (i, g) in groups.iter_mut().enumerate() {
            let kept = (self.joined).read(rollup, cells.key(i), |rows| {
                u32::from(rows.is_empty() != keeps_found)
            });
            *g = if kept == 1 { *g } else { 0 };
        }
    }
}

impl AttributeLookup<'_> {
    /// The attribute's code for the cell with `key`: the one row under the
    /// key, or [`NULL`].
    fn code(&mut self, rollup: &Rollup, key: &[u32]) -> u32 {
        let attr = self.attr;
        (self.joined).read(rollup, key, |rows| {
            rows.first().map_or(NULL, |row| row[attr])
        })
    }

    /// The attribute's code under one way the joins resolved.
    fn code_in(&self, row: &JoinRow<'_>) -> u32 {
        row[self.r].map_or(NULL, |found| found[self.attr])
    }
}

impl<'s> Lookup<'s> {
    /// The code of the key's value for cell `i`, in a view that counts each
    /// cell once.
    fn code(&mut self, rollup: &Rollup, cells: &Cells, i: usize) -> u32 {
        match self {
            Lookup::KeyPart(part) => cells.code(i, *part),
            Lookup::Attribute(attr) => attr.code(rollup, cells.key(i)),
        }
    }

    /// The code of the key's value for cell `i` under one way its joins
    /// resolved.
    fn code_in(&self, cells: &Cells, i: usize, row: &JoinRow<'_>) -> u32 {
        match self {
            Lookup::KeyPart(part) => cells.code(i, *part),
            Lookup::Attribute(attr) => attr.code_in(row),
        }
    }

    /// Borrowed from the rollup or the projection, which outlive the ask.
    fn value<'r>(&self, rollup: &'r Rollup, code: u32) -> &'r Datum
    where
        's: 'r,
    {
        match self {
            Lookup::KeyPart(part) => rollup.value(*part, code),
            Lookup::Attribute(attr) => attr.joined.projection.attribute_value(attr.attr, code),
        }
    }

    /// Codes the key's values have.
    fn bound(&self, rollup: &Rollup) -> usize {
        match self {
            Lookup::KeyPart(part) => rollup.codes(*part),
            Lookup::Attribute(attr) => attr.joined.projection.attribute_codes(attr.attr),
        }
    }

    fn column_type(&self, rollup: &Rollup) -> ColumnType {
        match self {
            Lookup::KeyPart(part) => rollup.key_type(*part),
            Lookup::Attribute(attr) => attr.joined.projection.attribute_type(attr.attr),
        }
    }
}

/// A [`Condition`] of an ask, worked out once per code of its key as the ask
/// meets the code.
struct Match<'s, 'a> {
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

impl Match<'_, '_> {
    /// Whether a cell passes is as likely as not, so the loop selects rather
    /// than branches; only a code met first branches.
    fn pass(&mut self, rollup: &Rollup, cells: &Cells, groups: &mut [u32]) {
        for (i, g) in groups.iter_mut().enumerate() {
            let code = self.lookup.code(rollup, cells, i);
            let mut known = self.memo[code as usize];
            if known == Known::Unknown && *g != 0 {
                known = self.resolve(rollup, code);
            }
            *g = if known == Known::Holds { *g } else { 0 };
        }
    }

    /// Whether cell `i` passes under one way its joins resolved.
    fn holds(&mut self, rollup: &Rollup, cells: &Cells, i: usize, row: &JoinRow<'_>) -> bool {
        let code = self.lookup.code_in(cells, i, row);
        let mut known = self.memo[code as usize];
        if known == Known::Unknown {
            known = self.resolve(rollup, code);
        }
        known == Known::Holds
    }

    fn resolve(&mut self, rollup: &Rollup, code: u32) -> Known {
        let v = self.lookup.value(rollup, code);
        let known = if self.values.iter().any(|lit| v.matches(lit)) {
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
        key: TimeKey,
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

    fn time(key: TimeKey, width: Width, buckets: usize) -> Grouping<'s> {
        Grouping::Time {
            key,
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
            key,
            width,
            bucket_starts,
            ..
        } = self
        {
            let start = key.bucket_start(bucket, *width);
            if bucket_starts.last() != Some(&start) {
                bucket_starts.push(start);
            }
        }
    }

    fn offsets(&mut self, rollup: &Rollup, cells: &Cells, stride: usize, at: &mut [usize]) {
        match self {
            Grouping::Time { bucket_starts, .. } => {
                let offset = (bucket_starts.len() - 1) * stride;
                at.iter_mut().for_each(|a| *a += offset);
            }
            // Cells passed over too, as a branch costs more than their
            // entries, which nothing reads.
            Grouping::Lookup { lookup, .. } => {
                for (i, a) in at.iter_mut().enumerate() {
                    *a += lookup.code(rollup, cells, i) as usize * stride;
                }
            }
        }
    }

    fn number(&mut self, rollup: &Rollup, cells: &Cells, i: usize) -> u32 {
        match self {
            Grouping::Lookup { lookup, .. } => lookup.code(rollup, cells, i),
            Grouping::Time { bucket_starts, .. } => current(bucket_starts),
        }
    }

    fn number_in(&self, cells: &Cells, i: usize, row: &JoinRow<'_>) -> u32 {
        match self {
            Grouping::Lookup { lookup, .. } => lookup.code_in(cells, i, row),
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
        let size = by.iter().try_fold(1usize, |size, p| {
            strides.push(size);
            size.checked_mul(p.bound())
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
                for (p, &stride) in by.iter_mut().zip(&self.strides) {
                    p.offsets(rollup, cells, stride, &mut self.at);
                }
                // Selects, not branches: under a filter, whether a cell
                // passed is as likely as not. `at` is in the table for
                // every cell.
                for (g, &at) in groups.iter_mut().zip(&self.at) {
                    let known = table[at];
                    new |= known == 0 && *g != 0;
                    let group = if known == 0 { NEW } else { known };
                    *g = if *g != 0 { group } else { 0 };
                }
            }
            GroupIndex::Sparse(_) => {
                new = true;
                groups
                    .iter_mut()
                    .filter(|g| **g != 0)
                    .for_each(|g| *g = NEW);
            }
        }
        if new {
            for (i, g) in groups.iter_mut().enumerate() {
                if *g != NEW {
                    continue;
                }
                // A cell before it in the bucket may have made its group.
                *g = match &self.index {
                    GroupIndex::Dense(table) if table[self.at[i]] != 0 => table[self.at[i]],
                    _ => self.make(by, rollup, cells, i),
                };
            }
        }
    }

    /// The group of cell `i`, made if new.
    fn make(
        &mut self,
        by: &mut [Grouping<'a>],
        rollup: &'a Rollup,
        cells: &'a Cells,
        i: usize,
    ) -> u32 {
        self.numbers.clear();
        self.numbers
            .extend(by.iter_mut().map(|p| p.number(rollup, cells, i)));
        self.insert(by, rollup)
    }

    /// The group `numbers` names, made if new.
    fn insert(&mut self, by: &[Grouping<'a>], rollup: &'a Rollup) -> u32 {
        let next = u32::try_from(self.len()).expect("fewer groups than cells");
        let group = match &mut self.index {
            GroupIndex::Dense(table) => {
                let at: usize = (self.numbers.iter().zip(&self.strides))
                    .map(|(&n, stride)| n as usize * stride)
                    .sum();
                if table[at] == 0 {
                    table[at] = next;
                }
                table[at]
            }
            // Looked up before inserting, so only a new group allocates.
            GroupIndex::Sparse(map) => match map.get(self.numbers.as_slice()) {
                Some(&g) => g,
                None => {
                    map.insert(self.numbers.as_slice().into(), next);
                    next
                }
            },
        };
        if group == next {
            let values = (self.numbers.iter().zip(by)).map(|(&n, p)| p.value(rollup, n));
            self.keys.extend(values);
            self.made += 1;
        }
        group
    }
}

struct Asked<'s, 'a> {
    partials: Partials,
    state: Option<usize>,
    filter: Vec<Match<'s, 'a>>,
}
