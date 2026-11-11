use std::borrow::Cow;

use crate::aggregate::Partials;
use crate::ask::{Ask, AskError, AskKey, TimeKey};
use crate::hash::FastMap;
use crate::ident::Identifier;
use crate::projection::{NULL, Projection};
use crate::rollup::{Bucket, Cells, Rollup, RowError};
use crate::sealed::{Sealed, SealedError, SealedId, Thawed};
use crate::spec::{Condition, Fact, Key, KeyPart, Literal, Measure, RefKind, Spec, Via, Width};
use crate::value::{ColumnType, Datum, Value};

const CHECKED: &str = "checked by Spec::new or Spec::check";
const IN_SPEC: &str = "the table is in the spec";

/// Thawed cells an ask reads, by width and start.
type ThawedCells<'t> = FastMap<(i64, i64), &'t Cells>;

/// The cells of every fact, and every dim projected to its key and the
/// columns the views read. Nothing else: a cell is keyed by the fact's
/// join columns, and an ask reads a dim's rows through them, as the dim
/// is now.
#[derive(Debug)]
pub struct Store {
    spec: Spec,
    rollups: Vec<Rollup>,
    projections: Vec<Projection>,
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
        }
    }

    /// Adds rows laid end to end, as a load reads them, each a row of
    /// `table` wide.
    pub fn add_rows(&mut self, table: &str, rows: &[Datum]) -> Result<(), RowError> {
        match self.member_mut(table) {
            Member::Rollup(rollup) => rollup.add_rows(rows),
            Member::Projection(projection) => projection.add_rows(rows),
        }
    }

    /// Adds one row, as the stream brings it.
    pub fn add_row(&mut self, table: &str, row: &[Datum]) -> Result<(), RowError> {
        match self.member_mut(table) {
            Member::Rollup(rollup) => rollup.add(row),
            Member::Projection(projection) => projection.add(row),
        }
    }

    /// Removes one row equal to the one given. A row never added is not
    /// found, so the caller bucket_starts over.
    pub fn remove_row(&mut self, table: &str, row: &[Datum]) -> Result<(), RowError> {
        match self.member_mut(table) {
            Member::Rollup(rollup) => rollup.remove(row),
            Member::Projection(projection) => projection.remove(row),
        }
    }

    /// Drops every row of a table after a truncate.
    pub fn clear(&mut self, table: &str) {
        match self.member_mut(table) {
            Member::Rollup(rollup) => rollup.clear(),
            Member::Projection(projection) => projection.clear(),
        }
    }

    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    pub fn cells(&self, fact: &str) -> Option<usize> {
        Some(self.rollup(fact)?.1.cells())
    }

    pub fn dim_rows(&self, dim: &str) -> Option<usize> {
        Some(self.projections[self.dim_index(dim)?].len())
    }

    pub fn cells_read(&self, ask: &Ask) -> Result<usize, Vec<AskError>> {
        let (_, rollup) = self.checked(ask)?;
        Ok(buckets(rollup, ask).map(|(_, _, b)| b.len()).sum())
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
    /// for it back.
    pub fn seal(&mut self, id: &SealedId) -> Result<(), SealedError> {
        let sealed = match self.fact_index(id.fact.as_str()) {
            Some(i) => self.rollups[i].seal(id.width, id.bucket_start),
            None => Err("unknown fact"),
        };
        sealed.map_err(|reason| SealedError {
            id: id.clone(),
            reason,
        })
    }

    /// Every sealed bucket of a fact, which a truncate drops with the rest.
    pub fn sealed(&self, fact: &str) -> Vec<SealedId> {
        let (fact, rollup) = self.rollup(fact).expect(IN_SPEC);
        sealed_ids(fact, rollup.sealed())
    }

    /// The sealed buckets [`Store::ask_with`] needs for `ask`.
    pub fn sealed_read(&self, ask: &Ask) -> Result<Vec<SealedId>, Vec<AskError>> {
        let (fact, rollup) = self.checked(ask)?;
        let sealed = buckets(rollup, ask)
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

    /// Puts a sealed bucket's cells back, so changes reach them again.
    pub fn unseal(&mut self, thawed: Thawed) -> Result<(), SealedError> {
        let Thawed { id, cells } = thawed;
        let rollup = self.rollup_mut(id.fact.as_str());
        (rollup.unseal(id.width, id.bucket_start, cells))
            .map_err(|reason| SealedError { id, reason })
    }

    /// Every key is a part of the rollup key or a dim value reached
    /// through the parts of a ref's via columns, so the cell loop only
    /// looks up what each code maps to, once per code. A view whose joins
    /// are all left and one row a key walks the cells once each; one with
    /// another kind of join, or a key with several rows, walks each cell
    /// once per row its joins find, as PostgreSQL repeats the fact row.
    pub fn ask(&self, ask: &Ask) -> Result<Answer, Vec<AskError>> {
        self.ask_with(ask, &[])
    }

    /// As [`Store::ask`], reading sealed buckets from `thawed`, which holds
    /// every one [`Store::sealed_read`] names.
    pub fn ask_with(&self, ask: &Ask, thawed: &[Thawed]) -> Result<Answer, Vec<AskError>> {
        let (fact, rollup) = self.checked(ask)?;
        self.check_types(fact, rollup, ask)?;
        let loaded: ThawedCells<'_> = (thawed.iter())
            .filter(|t| t.id.fact == fact.name)
            .map(|t| ((t.id.width, t.id.bucket_start), &t.cells))
            .collect();
        let buckets = buckets(rollup, ask).count();
        let mut by: Vec<Grouping> = (ask.keys.iter())
            .map(|key| match key {
                AskKey::Key(name) => Grouping::lookup(self.lookup(fact, rollup, name), rollup),
                AskKey::Time(t) => Grouping::time(*t, fact.time.width, buckets),
            })
            .collect();
        let mut groups = Groups::new(&by);
        let mut filter = self.tests(fact, rollup, &fact.filter);
        filter.extend(self.tests(fact, rollup, &ask.filter));
        let asked: Vec<&Measure> = (ask.measures.iter())
            .map(|name| fact.measure(name.as_str()).expect(CHECKED))
            .collect();
        let mut measures: Vec<Asked> = (asked.iter())
            .map(|m| Asked {
                partials: Partials::new(&m.aggregate),
                state: rollup.state_of(&m.aggregate),
                filter: self.tests(fact, rollup, &m.filter),
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
        match self.expander(fact) {
            None => ask_once(&mut each, cover(rollup, ask, &loaded)),
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
    fn check_types(&self, fact: &Fact, rollup: &Rollup, ask: &Ask) -> Result<(), Vec<AskError>> {
        let wrong: Vec<AskError> = (ask.filter.iter())
            .flat_map(|cond| {
                let column = self.lookup(fact, rollup, &cond.key).column_type(rollup);
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

    /// How a cell's joins repeat it, when any can: a join not left, a key
    /// with several rows, or a join through another dim. `None` when every
    /// cell counts once.
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
            p.kind == RefKind::Left
                && p.projection.is_unique()
                && p.via.iter().all(|v| matches!(v, ViaFrom::KeyPart(_)))
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

    fn rollup_mut(&mut self, fact: &str) -> &mut Rollup {
        let i = self.fact_index(fact).expect(IN_SPEC);
        &mut self.rollups[i]
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

    fn checked(&self, ask: &Ask) -> Result<(&Fact, &Rollup), Vec<AskError>> {
        let fact = self.spec.check_ask(ask)?;
        Ok(self.rollup(fact.name.as_str()).expect(CHECKED))
    }

    /// Where a key's values come from: a rollup-key part, or a dim's
    /// projection reached through a ref.
    fn lookup(&self, fact: &Fact, rollup: &Rollup, name: &Identifier) -> Lookup<'_> {
        match fact.resolve(name).expect(CHECKED) {
            Key::ByKey(by) => Lookup::KeyPart(key_part_of(fact, by.key_part())),
            Key::Attribute(r, attr) => {
                let dim = self.spec.dim(r.dim.as_str()).expect(CHECKED);
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
                    projection: self.projection(r.dim.as_str()),
                    via,
                    key: Vec::new(),
                    tuple: Vec::new(),
                    memo,
                })
            }
        }
    }

    fn tests<'a>(
        &self,
        fact: &Fact,
        rollup: &Rollup,
        conds: &'a [Condition],
    ) -> Vec<Match<'_, 'a>> {
        conds
            .iter()
            .map(|c| {
                let lookup = self.lookup(fact, rollup, &c.key);
                Match {
                    memo: vec![Known::Unknown; lookup.bound(rollup)],
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

/// What the cell loops fill in.
struct Each<'e, 's, 'a> {
    rollup: &'s Rollup,
    by: &'e mut [Grouping<'s>],
    groups: &'e mut Groups<'s>,
    filter: &'e mut [Match<'s, 'a>],
    measures: &'e mut [Asked<'s, 'a>],
}

/// The cell loop for a view that counts each cell once: every join left
/// and one row a key, none joined through another.
fn ask_once<'s>(each: &mut Each<'_, 's, '_>, cover: impl Iterator<Item = (i64, &'s Cells)>) {
    let rollup = each.rollup;
    // Per cell of a bucket: 1 while it may pass, then its group; 0 for
    // cells passed over. Again per measure with a filter of its own.
    let mut cell_groups = Vec::new();
    let mut measure_groups = Vec::new();
    for (bucket, cells) in cover {
        each.by.iter_mut().for_each(|p| p.enter(bucket));
        cell_groups.clear();
        cell_groups.resize(cells.len(), 1);
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
                .merge(read, cells.counts(), m.state.map(|i| cells.state(i)));
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
                        m.partials.merge_cell(g, i, cells.counts(), column);
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
                let matches = plan.projection.matches(&dim_key);
                let with = |found: Option<&'s [u32]>| {
                    let mut row = row.clone();
                    row.push(found);
                    row
                };
                match plan.kind {
                    RefKind::Left if matches.is_empty() => next.push(with(None)),
                    RefKind::Left | RefKind::Inner => {
                        next.extend(matches.iter().map(|m| with(Some(m))));
                    }
                    RefKind::Semi if !matches.is_empty() => next.push(with(None)),
                    RefKind::Anti if matches.is_empty() => next.push(with(None)),
                    RefKind::Semi | RefKind::Anti => {}
                }
            }
            rows = next;
        }
        rows
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
    projection: &'s Projection,
    /// The rollup-key parts of the ref's fact-side via columns, in the
    /// dim's key order, for a view that counts each cell once.
    via: Vec<usize>,
    /// The key of one lookup, kept to spare an allocation per lookup.
    key: Vec<Datum>,
    /// The via codes of one lookup, kept likewise.
    tuple: Vec<u32>,
    memo: Memo,
}

/// A via code not looked up yet.
const UNKNOWN: u32 = u32::MAX;

/// Attribute code per via code tuple: by the one code when the ref joins
/// on one column, else by the tuple.
enum Memo {
    Dense(Vec<u32>),
    Sparse(FastMap<Box<[u32]>, u32>),
}

impl AttributeLookup<'_> {
    /// The attribute's code for the cell with `key`: the one row under the
    /// key, or [`NULL`].
    fn code(&mut self, rollup: &Rollup, key: &[u32]) -> u32 {
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
            let found = self.projection.matches(&self.key).first();
            *known = found.map_or(NULL, |row| row[self.attr]);
        }
        *known
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
            Lookup::Attribute(attr) => attr.projection.attribute_value(attr.attr, code),
        }
    }

    /// Codes the key's values have.
    fn bound(&self, rollup: &Rollup) -> usize {
        match self {
            Lookup::KeyPart(part) => rollup.codes(*part),
            Lookup::Attribute(attr) => attr.projection.attribute_codes(attr.attr),
        }
    }

    fn column_type(&self, rollup: &Rollup) -> ColumnType {
        match self {
            Lookup::KeyPart(part) => rollup.key_type(*part),
            Lookup::Attribute(attr) => attr.projection.attribute_type(attr.attr),
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
