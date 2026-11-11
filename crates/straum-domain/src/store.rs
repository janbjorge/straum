use std::borrow::Cow;
use std::collections::HashMap;

use crate::agg::Partials;
use crate::ask::{Ask, AskError, AskKey, TimeKey};
use crate::hash::FastMap;
use crate::ident::Ident;
use crate::projection::{NULL, Projection};
use crate::rollup::{Bucket, Cells, Rollup, RowError};
use crate::sealed::{Sealed, SealedError, SealedId, Thawed};
use crate::spec::{Cond, Fact, Grain, Key, Literal, RefKind, Slot, Spec, Via};
use crate::value::{ColumnType, Datum, Value};

const CHECKED: &str = "checked by Spec::new or Spec::check";
const IN_SPEC: &str = "the fact is in the spec";
const IN_DIMS: &str = "the dim is in the spec";
const THAWED: &str = "ask_with is given every bucket sealed_read names";

/// Thawed cells an ask reads, by width and start.
type Loaded<'t> = FastMap<(i64, i64), &'t Cells>;

/// The cells of every fact, and every dim projected to its key and the
/// columns the views read. Nothing else: a cell is keyed by the fact's
/// join columns, and an ask reads a dim's rows through them, as the dim
/// is now.
#[derive(Debug)]
pub struct Store {
    spec: Spec,
    /// One per fact, in spec order.
    rollups: Vec<Rollup>,
    /// One per dim, in spec order.
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
    /// A time key, whose values are the second its period starts.
    pub time: bool,
}

impl Store {
    pub fn new(spec: Spec) -> Store {
        Store {
            rollups: spec.facts().iter().map(Rollup::new).collect(),
            projections: spec.dims().iter().map(Projection::new).collect(),
            spec,
        }
    }

    /// Sets the types of a row of `fact`, [`Fact::columns`]. Only the
    /// source knows them, so a load sets them.
    pub fn set_fact_types(&mut self, fact: &str, types: &[ColumnType]) -> Result<(), RowError> {
        self.rollup_mut(fact).set_types(types)
    }

    /// Adds rows laid end to end, as a load reads them, each
    /// [`Fact::columns`] wide.
    pub fn add_fact_rows(&mut self, fact: &str, rows: &[Datum]) -> Result<(), RowError> {
        self.rollup_mut(fact).add_rows(rows)
    }

    /// Adds one row, as the stream brings it.
    pub fn add_fact_row(&mut self, fact: &str, row: &[Datum]) -> Result<(), RowError> {
        self.rollup_mut(fact).add(row)
    }

    /// Removes one row. A row never added is not found, so the caller
    /// starts over.
    pub fn remove_fact_row(&mut self, fact: &str, row: &[Datum]) -> Result<(), RowError> {
        self.rollup_mut(fact).remove(row)
    }

    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    /// Drops every row of a fact after a truncate.
    pub fn clear_fact(&mut self, fact: &str) {
        self.rollup_mut(fact).clear();
    }

    pub fn cells(&self, fact: &str) -> Option<usize> {
        Some(self.rollups[self.fact_index(fact)?].cells())
    }

    /// Sets the types of a row of `dim`, [`Dim::columns`]: the key, then
    /// the attributes. Only the source knows them, so a load sets them.
    pub fn set_dim_types(&mut self, dim: &str, types: &[ColumnType]) -> Result<(), RowError> {
        self.projection_mut(dim).set_types(types)
    }

    /// Adds rows laid end to end, as a load reads them, each
    /// [`Dim::columns`] wide.
    pub fn add_dim_rows(&mut self, dim: &str, rows: &[Datum]) -> Result<(), RowError> {
        self.projection_mut(dim).add_rows(rows)
    }

    /// Adds one row, as the stream brings it.
    pub fn add_dim_row(&mut self, dim: &str, row: &[Datum]) -> Result<(), RowError> {
        self.projection_mut(dim).add(row)
    }

    /// Removes one row equal to the one given.
    pub fn remove_dim_row(&mut self, dim: &str, row: &[Datum]) -> Result<(), RowError> {
        self.projection_mut(dim).remove(row)
    }

    /// Drops every row of a dim after a truncate.
    pub fn clear_dim(&mut self, dim: &str) {
        self.projection_mut(dim).clear();
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
            .flat_map(|(fact, rollup)| {
                (rollup.sealable(before)).map(|(width, start)| SealedId {
                    fact: fact.name.clone(),
                    width,
                    start,
                })
            })
            .collect()
    }

    /// A copy of a bucket's cells, for a spill to keep before
    /// [`Store::seal`] drops them. `None` if it is sealed or has no cells.
    pub fn sealed_copy(&self, id: &SealedId) -> Option<Sealed> {
        let rollup = &self.rollups[self.fact_index(id.fact.as_str())?];
        let (keys, rows, columns) = rollup.sealed_copy(id.width, id.start)?;
        Some(Sealed {
            id: id.clone(),
            keys,
            rows,
            columns,
        })
    }

    /// Drops a bucket's cells, keeping its place, so asks and changes ask
    /// for it back. Whether it was not sealed.
    pub fn seal(&mut self, id: &SealedId) -> bool {
        match self.fact_index(id.fact.as_str()) {
            Some(i) => self.rollups[i].seal(id.width, id.start),
            None => false,
        }
    }

    /// Every sealed bucket of a fact, which a truncate drops with the rest.
    pub fn sealed(&self, fact: &str) -> Vec<SealedId> {
        let i = self.fact_index(fact).expect(IN_SPEC);
        (self.rollups[i].sealed())
            .map(|(width, start)| SealedId {
                fact: self.spec.facts()[i].name.clone(),
                width,
                start,
            })
            .collect()
    }

    /// The sealed buckets [`Store::ask_with`] needs for `ask`.
    pub fn sealed_read(&self, ask: &Ask) -> Result<Vec<SealedId>, Vec<AskError>> {
        let (fact, rollup) = self.checked(ask)?;
        Ok(buckets(rollup, ask)
            .filter(|(_, _, b)| matches!(b, Bucket::Sealed { .. }))
            .map(|(width, start, _)| SealedId {
                fact: fact.name.clone(),
                width,
                start,
            })
            .collect())
    }

    /// The sealed buckets a fact row falls in, which must be unsealed
    /// before the row is added or removed.
    pub fn sealed_touched(&self, fact: &str, row: &[Datum]) -> Vec<SealedId> {
        let i = self.fact_index(fact).expect(IN_SPEC);
        let Some(time) = row.first() else {
            return Vec::new();
        };
        (self.rollups[i].touched(time))
            .map(|(width, start)| SealedId {
                fact: self.spec.facts()[i].name.clone(),
                width,
                start,
            })
            .collect()
    }

    /// Checks a sealed bucket against the store, so neither an ask nor
    /// [`Store::unseal`] can panic on it.
    pub fn thaw(&self, sealed: Sealed) -> Result<Thawed, SealedError> {
        let Sealed {
            id,
            keys,
            rows,
            columns,
        } = sealed;
        let rollup = match self.fact_index(id.fact.as_str()) {
            Some(i) if self.rollups[i].has_width(id.width) => &self.rollups[i],
            Some(_) => {
                return Err(SealedError {
                    id,
                    reason: "no such width",
                });
            }
            None => {
                return Err(SealedError {
                    id,
                    reason: "unknown fact",
                });
            }
        };
        match rollup.thaw(keys, rows, columns) {
            Ok(cells) => Ok(Thawed { id, cells }),
            Err(reason) => Err(SealedError { id, reason }),
        }
    }

    /// Puts a sealed bucket's cells back, so changes reach them again.
    pub fn unseal(&mut self, thawed: Thawed) -> Result<(), SealedError> {
        let Thawed { id, cells } = thawed;
        let i = self
            .fact_index(id.fact.as_str())
            .expect("thawed by this store");
        (self.rollups[i].unseal(id.width, id.start, cells))
            .map_err(|reason| SealedError { id, reason })
    }

    /// Every key is a slot of the rollup key or a dim value reached
    /// through the slots of a ref's via columns, so the cell loop only
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
        let loaded: Loaded<'_> = (thawed.iter())
            .filter(|t| t.id.fact == fact.name)
            .map(|t| ((t.id.width, t.id.start), &t.cells))
            .collect();
        // Only the load knows the types, so this check comes after Spec::check.
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
        if !wrong.is_empty() {
            return Err(wrong);
        }
        let buckets = buckets(rollup, ask).count();

        let mut by: Vec<Part> = ask
            .by
            .iter()
            .map(|key| {
                let of = match key {
                    AskKey::Key(name) => KeyOf::Lookup(self.lookup(fact, rollup, name)),
                    AskKey::Time(t) => KeyOf::Time(*t, fact.time.grain),
                };
                Part::new(of, rollup, buckets)
            })
            .collect();
        let mut groups = Groups::new(&by);
        let mut filter = self.tests(fact, rollup, &fact.filter);
        filter.extend(self.tests(fact, rollup, &ask.filter));
        let mut measures: Vec<Asked> = ask
            .measures
            .iter()
            .map(|name| {
                let m = fact.measure(name.as_str()).expect(CHECKED);
                Asked {
                    partials: Partials::new(&m.agg),
                    state: rollup.state_of(&m.agg),
                    filter: self.tests(fact, rollup, &m.filter),
                }
            })
            .collect();
        let keys = (ask.by.iter().zip(&by)).map(|(key, p)| AnswerColumn {
            name: match key {
                AskKey::Time(t) => t.to_string(),
                AskKey::Key(name) => name.to_string(),
            },
            scale: p.of.column_type(rollup).scale(),
            time: matches!(key, AskKey::Time(_)),
        });
        let columns = keys
            .chain(ask.measures.iter().map(|name| AnswerColumn {
                name: name.to_string(),
                scale: rollup.scale_of(&fact.measure(name.as_str()).expect(CHECKED).agg),
                time: false,
            }))
            .collect();

        if by.is_empty() {
            // As in SQL without GROUP BY: one row even when nothing matches.
            groups.numbers.clear();
            groups.insert(&by, rollup);
        }
        match self.expander(fact) {
            None => ask_once(
                rollup,
                ask,
                &loaded,
                &mut by,
                &mut groups,
                &mut filter,
                &mut measures,
            ),
            Some(mut expander) => {
                for (bucket, cells) in cover(rollup, ask, &loaded) {
                    by.iter_mut().for_each(|p| p.enter(bucket));
                    for i in 0..cells.len() {
                        for row in expander.expand(rollup, cells.key(i)) {
                            let held = filter.iter_mut().all(|t| t.holds(rollup, cells, i, row));
                            if !held {
                                continue;
                            }
                            groups.numbers.clear();
                            groups
                                .numbers
                                .extend(by.iter_mut().map(|p| p.number_in(cells, i, row)));
                            let g = groups.insert(&by, rollup);
                            for m in &mut measures {
                                m.partials.grow(groups.len());
                                let held =
                                    m.filter.iter_mut().all(|t| t.holds(rollup, cells, i, row));
                                if held {
                                    let column = m.state.map(|s| cells.column(s));
                                    m.partials.merge_cell(g, i, cells.rows(), column);
                                }
                            }
                        }
                    }
                }
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

    /// How a cell's joins repeat it, when any can: a join not left, a key
    /// with several rows, or a join through another dim. `None` when every
    /// cell counts once.
    fn expander(&self, fact: &Fact) -> Option<Expander<'_>> {
        let plans: Vec<RefPlan<'_>> = (fact.refs.iter())
            .map(|r| {
                let dim = self.spec.dim(r.dim.as_str()).expect(CHECKED);
                let via = (r.via.iter())
                    .map(|v| match v {
                        Via::Fact(column) => ViaSource::Slot(slot_of(fact, Slot::Column(column))),
                        Via::Attr { through, column } => {
                            let parent = self.spec.dim(fact.refs[*through].dim.as_str());
                            let attr = (parent.expect(CHECKED).attrs.iter())
                                .position(|a| a == column)
                                .expect(CHECKED);
                            ViaSource::Through { r: *through, attr }
                        }
                    })
                    .collect();
                RefPlan {
                    kind: r.kind,
                    projection: self.projection(dim.name.as_str()),
                    via,
                }
            })
            .collect();
        let once = plans.iter().all(|p| {
            p.kind == RefKind::Left
                && p.projection.is_unique()
                && p.via.iter().all(|v| matches!(v, ViaSource::Slot(_)))
        });
        if once {
            return None;
        }
        let mut slots = Vec::new();
        for v in plans.iter().flat_map(|p| &p.via) {
            if let ViaSource::Slot(slot) = v
                && !slots.contains(slot)
            {
                slots.push(*slot);
            }
        }
        Some(Expander {
            plans,
            slots,
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

    fn rollup_mut(&mut self, fact: &str) -> &mut Rollup {
        let i = self.fact_index(fact).expect(IN_SPEC);
        &mut self.rollups[i]
    }

    fn projection(&self, dim: &str) -> &Projection {
        &self.projections[self.dim_index(dim).expect(IN_DIMS)]
    }

    fn projection_mut(&mut self, dim: &str) -> &mut Projection {
        let i = self.dim_index(dim).expect(IN_DIMS);
        &mut self.projections[i]
    }

    fn checked(&self, ask: &Ask) -> Result<(&Fact, &Rollup), Vec<AskError>> {
        let fact = self.spec.check(ask)?;
        let i = self.fact_index(fact.name.as_str()).expect(CHECKED);
        Ok((fact, &self.rollups[i]))
    }

    /// Where a key's values come from: a rollup-key slot, or a dim's
    /// projection reached through a ref.
    fn lookup(&self, fact: &Fact, rollup: &Rollup, name: &Ident) -> Lookup<'_> {
        match fact.resolve(name).expect(CHECKED) {
            Key::By(by) => Lookup::Slot(slot_of(fact, by.slot())),
            Key::Attr(r, attr) => {
                let dim = self.spec.dim(r.dim.as_str()).expect(CHECKED);
                let via: Vec<usize> = (r.via.iter())
                    .filter_map(|v| match v {
                        Via::Fact(column) => Some(slot_of(fact, Slot::Column(column))),
                        Via::Attr { .. } => None,
                    })
                    .collect();
                let memo = match via[..] {
                    [slot] => Memo::Dense(vec![UNKNOWN; rollup.codes(slot)]),
                    _ => Memo::Sparse(FastMap::default()),
                };
                Lookup::Attr(AttrLookup {
                    r: fact
                        .refs
                        .iter()
                        .position(|q| q.name == r.name)
                        .expect(CHECKED),
                    attr: dim
                        .attrs
                        .iter()
                        .position(|a| *a == attr.column)
                        .expect(CHECKED),
                    projection: self.projection(dim.name.as_str()),
                    via,
                    key: Vec::new(),
                    memo,
                })
            }
        }
    }

    fn tests<'a>(&self, fact: &Fact, rollup: &Rollup, conds: &'a [Cond]) -> Vec<Test<'_, 'a>> {
        conds
            .iter()
            .map(|c| {
                let lookup = self.lookup(fact, rollup, &c.key);
                Test {
                    memo: vec![Known::Unknown; lookup.bound(rollup)],
                    lookup,
                    values: &c.values,
                }
            })
            .collect()
    }
}

/// The cell loop for a view that counts each cell once: every join left
/// and one row a key, none joined through another.
fn ask_once<'s>(
    rollup: &'s Rollup,
    ask: &Ask,
    loaded: &'s Loaded<'s>,
    by: &mut [Part<'s>],
    groups: &mut Groups<'s>,
    filter: &mut [Test<'s, '_>],
    measures: &mut [Asked<'s, '_>],
) {
    // Per cell of a bucket: 1 while it may pass, then its group; 0 for
    // cells passed over. Again per measure with a filter of its own.
    let mut cell_groups = Vec::new();
    let mut measure_groups = Vec::new();
    for (bucket, cells) in cover(rollup, ask, loaded) {
        by.iter_mut().for_each(|p| p.enter(bucket));
        cell_groups.clear();
        cell_groups.resize(cells.len(), 1);
        for t in filter.iter_mut() {
            t.pass(rollup, cells, &mut cell_groups);
        }
        groups.assign(by, rollup, cells, &mut cell_groups);
        for m in measures.iter_mut() {
            m.partials.grow(groups.len());
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
                .merge(read, cells.rows(), m.state.map(|i| cells.column(i)));
        }
    }
}

fn slot_of(fact: &Fact, slot: Slot<'_>) -> usize {
    fact.rollup_key().position(|k| k == slot).expect(CHECKED)
}

fn buckets<'r>(rollup: &'r Rollup, ask: &Ask) -> impl Iterator<Item = (i64, i64, &'r Bucket)> {
    let each_bucket = ask.by.iter().any(|key| matches!(key, AskKey::Time(_)));
    rollup.cover(ask.from, ask.to, each_bucket)
}

/// Each bucket's start and cells, a sealed one's from `loaded`.
fn cover<'r>(
    rollup: &'r Rollup,
    ask: &Ask,
    loaded: &'r Loaded<'r>,
) -> impl Iterator<Item = (i64, &'r Cells)> {
    buckets(rollup, ask).map(|(width, start, bucket)| match bucket {
        Bucket::Hot(cells) => (start, cells),
        Bucket::Sealed { .. } => (start, *loaded.get(&(width, start)).expect(THAWED)),
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
    /// The rollup-key slots the refs read, each once.
    slots: Vec<usize>,
    memo: FastMap<Box<[u32]>, Vec<JoinRow<'s>>>,
}

struct RefPlan<'s> {
    kind: RefKind,
    projection: &'s Projection,
    via: Vec<ViaSource>,
}

/// Where a ref's key column comes from: a cell's slot, or an attribute
/// of a ref before it.
enum ViaSource {
    Slot(usize),
    Through { r: usize, attr: usize },
}

impl<'s> Expander<'s> {
    fn expand(&mut self, rollup: &Rollup, key: &[u32]) -> &[JoinRow<'s>] {
        let tuple: Box<[u32]> = self.slots.iter().map(|&slot| key[slot]).collect();
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
                        ViaSource::Slot(slot) => rollup.value(slot, key[slot]).clone(),
                        ViaSource::Through { r, attr } => match row[r] {
                            Some(found) => self.plans[r].projection.attr_value(attr, found[attr]),
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
    Slot(usize),
    Attr(AttrLookup<'s>),
}

/// A dim value read through a ref.
struct AttrLookup<'s> {
    /// Into [`Fact::refs`].
    r: usize,
    /// Into the dim's attributes.
    attr: usize,
    projection: &'s Projection,
    /// The rollup-key slots of the ref's fact-side via columns, in the
    /// dim's key order, for a view that counts each cell once.
    via: Vec<usize>,
    /// The key of one lookup, kept to spare an allocation per lookup.
    key: Vec<Datum>,
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

impl AttrLookup<'_> {
    /// The attribute's code for the cell with `key`: the one row under the
    /// key, or [`NULL`].
    fn code(&mut self, rollup: &Rollup, key: &[u32]) -> u32 {
        let known = match &self.memo {
            Memo::Dense(codes) => codes[key[self.via[0]] as usize],
            Memo::Sparse(map) => {
                let tuple: Vec<u32> = self.via.iter().map(|&slot| key[slot]).collect();
                map.get(tuple.as_slice()).copied().unwrap_or(UNKNOWN)
            }
        };
        if known != UNKNOWN {
            return known;
        }
        self.key.clear();
        self.key.extend(
            self.via
                .iter()
                .map(|&slot| rollup.value(slot, key[slot]).clone()),
        );
        let found = self.projection.matches(&self.key).first();
        let code = found.map_or(NULL, |row| row[self.attr]);
        match &mut self.memo {
            Memo::Dense(codes) => codes[key[self.via[0]] as usize] = code,
            Memo::Sparse(map) => {
                let tuple: Box<[u32]> = self.via.iter().map(|&slot| key[slot]).collect();
                map.insert(tuple, code);
            }
        }
        code
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
            Lookup::Slot(slot) => cells.code(i, *slot),
            Lookup::Attr(attr) => attr.code(rollup, cells.key(i)),
        }
    }

    /// The code of the key's value for cell `i` under one way its joins
    /// resolved.
    fn code_in(&self, cells: &Cells, i: usize, row: &JoinRow<'_>) -> u32 {
        match self {
            Lookup::Slot(slot) => cells.code(i, *slot),
            Lookup::Attr(attr) => attr.code_in(row),
        }
    }

    /// Borrowed from the rollup or the projection, which outlive the ask.
    fn value<'r>(&self, rollup: &'r Rollup, code: u32) -> &'r Datum
    where
        's: 'r,
    {
        match self {
            Lookup::Slot(slot) => rollup.value(*slot, code),
            Lookup::Attr(attr) => attr.projection.attr_value(attr.attr, code),
        }
    }

    /// Codes the key's values have.
    fn bound(&self, rollup: &Rollup) -> usize {
        match self {
            Lookup::Slot(slot) => rollup.codes(*slot),
            Lookup::Attr(attr) => attr.projection.attr_codes(attr.attr),
        }
    }

    fn column_type(&self, rollup: &Rollup) -> ColumnType {
        match self {
            Lookup::Slot(slot) => rollup.key_type(*slot),
            Lookup::Attr(attr) => attr.projection.attr_type(attr.attr),
        }
    }
}

/// What a by key's values are: a lookup, or the period of a time key.
enum KeyOf<'s> {
    Lookup(Lookup<'s>),
    Time(TimeKey, Grain),
}

impl KeyOf<'_> {
    fn column_type(&self, rollup: &Rollup) -> ColumnType {
        match self {
            KeyOf::Lookup(lookup) => lookup.column_type(rollup),
            KeyOf::Time(..) => rollup.time_type(),
        }
    }
}

/// A [`Cond`] of an ask, worked out once per code of its key as the ask
/// meets the code.
struct Test<'s, 'a> {
    lookup: Lookup<'s>,
    values: &'a [Literal],
    /// Per code of the key.
    memo: Vec<Known>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Known {
    #[default]
    Unknown,
    Holds,
    Fails,
}

impl Test<'_, '_> {
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
struct Part<'s> {
    of: KeyOf<'s>,
    /// For a time key, the start of each period met so far, the bucket at
    /// hand's last.
    starts: Vec<i64>,
    bound: usize,
}

impl<'s> Part<'s> {
    fn new(of: KeyOf<'s>, rollup: &Rollup, buckets: usize) -> Part<'s> {
        let bound = match &of {
            KeyOf::Lookup(lookup) => lookup.bound(rollup),
            KeyOf::Time(..) => buckets,
        };
        Part {
            of,
            starts: Vec::new(),
            bound,
        }
    }

    /// Buckets come in time order, so a period's buckets come together.
    fn enter(&mut self, bucket: i64) {
        if let KeyOf::Time(t, grain) = self.of {
            let start = t.start(bucket, grain);
            if self.starts.last() != Some(&start) {
                self.starts.push(start);
            }
        }
    }

    fn offsets(&mut self, rollup: &Rollup, cells: &Cells, stride: usize, at: &mut [usize]) {
        match &mut self.of {
            KeyOf::Time(..) => {
                let offset = (self.starts.len() - 1) * stride;
                at.iter_mut().for_each(|a| *a += offset);
            }
            // Cells passed over too, as a branch costs more than their
            // entries, which nothing reads.
            KeyOf::Lookup(lookup) => {
                for (i, a) in at.iter_mut().enumerate() {
                    *a += lookup.code(rollup, cells, i) as usize * stride;
                }
            }
        }
    }

    fn number(&mut self, rollup: &Rollup, cells: &Cells, i: usize) -> u32 {
        match &mut self.of {
            KeyOf::Lookup(lookup) => lookup.code(rollup, cells, i),
            KeyOf::Time(..) => self.period(),
        }
    }

    fn number_in(&self, cells: &Cells, i: usize, row: &JoinRow<'_>) -> u32 {
        match &self.of {
            KeyOf::Lookup(lookup) => lookup.code_in(cells, i, row),
            KeyOf::Time(..) => self.period(),
        }
    }

    fn period(&self) -> u32 {
        u32::try_from(self.starts.len() - 1).expect("codes are u32")
    }

    fn value<'r>(&self, rollup: &'r Rollup, number: u32) -> Cow<'r, Datum>
    where
        's: 'r,
    {
        match &self.of {
            KeyOf::Lookup(lookup) => Cow::Borrowed(lookup.value(rollup, number)),
            KeyOf::Time(..) => Cow::Owned(Datum::Int(self.starts[number as usize])),
        }
    }
}

/// A cell's group while [`Groups::assign`] has yet to look it up. Groups
/// number fewer than the cells, which fit in memory.
const NEW: u32 = u32::MAX;

/// Most entries a dense group table may have: 4 MiB of `u32`, of which an
/// ask touches only the pages its groups fall on.
const DENSE: usize = 1 << 20;

/// The groups an ask has met, numbered from 1 in the order it met them.
struct Groups<'a> {
    index: Index,
    strides: Vec<usize>,
    /// Each group's values, `by.len()` a group, group 1's first.
    keys: Vec<Cow<'a, Datum>>,
    /// Groups so far, not counting group 0.
    made: usize,
    numbers: Vec<u32>,
    at: Vec<usize>,
}

enum Index {
    /// Group per combination of by-key numbers, 0 for none yet.
    Dense(Vec<u32>),
    Sparse(HashMap<Box<[u32]>, u32>),
}

impl<'a> Groups<'a> {
    fn new(by: &[Part<'_>]) -> Groups<'a> {
        let mut strides = Vec::with_capacity(by.len());
        let size = by.iter().try_fold(1usize, |size, p| {
            strides.push(size);
            size.checked_mul(p.bound)
        });
        let index = match size {
            Some(size) if size <= DENSE => Index::Dense(vec![0; size]),
            _ => Index::Sparse(HashMap::new()),
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
        by: &mut [Part<'a>],
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
            Index::Dense(table) => {
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
            Index::Sparse(_) => {
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
                    Index::Dense(table) if table[self.at[i]] != 0 => table[self.at[i]],
                    _ => self.make(by, rollup, cells, i),
                };
            }
        }
    }

    /// The group of cell `i`, made if new.
    fn make(&mut self, by: &mut [Part<'a>], rollup: &'a Rollup, cells: &'a Cells, i: usize) -> u32 {
        self.numbers.clear();
        self.numbers
            .extend(by.iter_mut().map(|p| p.number(rollup, cells, i)));
        self.insert(by, rollup)
    }

    /// The group `numbers` names, made if new.
    fn insert(&mut self, by: &[Part<'a>], rollup: &'a Rollup) -> u32 {
        let next = u32::try_from(self.len()).expect("fewer groups than cells");
        let group = match &mut self.index {
            Index::Dense(table) => {
                let at: usize = (self.numbers.iter().zip(&self.strides))
                    .map(|(&n, stride)| n as usize * stride)
                    .sum();
                if table[at] == 0 {
                    table[at] = next;
                }
                table[at]
            }
            // Looked up before inserting, so only a new group allocates.
            Index::Sparse(map) => match map.get(self.numbers.as_slice()) {
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
    filter: Vec<Test<'s, 'a>>,
}
