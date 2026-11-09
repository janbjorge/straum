//! Every fact's rollup and every dim's rows for one spec, and asks answered
//! from them.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::iter;

use crate::ask::{Ask, AskError, AskKey, TimeKey};
use crate::rollup::{Cell, DimTable, Rollup, State};
use crate::spec::{Agg, Cond, Fact, Grain, Key, KeyPath, Literal, Slot, Spec};
use crate::value::{Datum, Value};

/// Panic message for names that `Spec::new` or `Spec::check` resolved.
const CHECKED: &str = "checked by Spec::new or Spec::check";

#[derive(Debug)]
pub struct Store {
    spec: Spec,
    /// One per fact, in spec order.
    rollups: Vec<Rollup>,
    /// One per dim, in spec order.
    dims: Vec<DimTable>,
}

/// One row per group, sorted by key: the values of the ask's `by`, then its
/// measures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub rows: Vec<(Vec<Datum>, Vec<Value>)>,
    /// The scale of each column of a row, the `by` keys then the measures:
    /// numeric values are integers in units of `10^-scale`.
    pub scales: Vec<u32>,
}

impl Store {
    pub fn new(spec: Spec) -> Store {
        Store {
            rollups: spec.facts().iter().map(Rollup::new).collect(),
            dims: spec.dims().iter().map(DimTable::new).collect(),
            spec,
        }
    }

    pub fn rollup(&mut self, fact: &str) -> Option<&mut Rollup> {
        let i = self.fact_index(fact)?;
        Some(&mut self.rollups[i])
    }

    pub fn dim(&mut self, dim: &str) -> Option<&mut DimTable> {
        let i = self.dim_index(dim)?;
        Some(&mut self.dims[i])
    }

    /// How many cells the fact's rollup holds.
    pub fn cells(&self, fact: &str) -> Option<usize> {
        Some(self.rollups[self.fact_index(fact)?].cells())
    }

    /// How many rows the dim holds.
    pub fn rows(&self, dim: &str) -> Option<usize> {
        Some(self.dims[self.dim_index(dim)?].rows())
    }

    /// Merges the cells in range that pass the ask's filter, one group per
    /// `by` value. Dim attributes are read per cell, after the rollup.
    pub fn ask(&self, ask: &Ask) -> Result<Answer, Vec<AskError>> {
        let fact = self.spec.check(ask)?;
        let rollup = &self.rollups[self.fact_index(fact.name.as_str()).expect(CHECKED)];

        let by: Vec<Source> = ask
            .by
            .iter()
            .map(|key| match key {
                AskKey::Key(path) => self.source(fact, path),
                AskKey::Time(t) => Source::Time(*t, fact.time.grain),
            })
            .collect();
        let filter = self.conditions(fact, &ask.filter);
        let measures: Vec<Asked> = ask
            .measures
            .iter()
            .map(|name| {
                let m = fact.measure(name.as_str()).expect(CHECKED);
                Asked {
                    empty: Partial::new(&m.agg),
                    state: rollup.state_of(&m.agg),
                    filter: self.conditions(fact, &m.filter),
                }
            })
            .collect();
        let empty = || measures.iter().map(|m| m.empty.clone()).collect::<Vec<_>>();
        let scales = (by.iter().map(|s| s.scale(rollup)))
            .chain(
                ask.measures
                    .iter()
                    .map(|name| rollup.scale_of(&fact.measure(name.as_str()).expect(CHECKED).agg)),
            )
            .collect();

        // Keyed on values borrowed from the cells and dims, filled into one
        // buffer: a cell costs no allocation, only a new group does.
        let mut groups: HashMap<Vec<Cow<Datum>>, Vec<Partial>> = HashMap::new();
        let mut group = Vec::with_capacity(by.len());
        let mut total = empty();
        let bucket_slot = rollup.bucket_slot();
        for (key, cell) in rollup.iter() {
            let Datum::Int(bucket) = key[bucket_slot] else {
                continue;
            };
            if bucket < ask.from || bucket >= ask.to || !all_hold(&filter, key, bucket) {
                continue;
            }
            let merge = |partials: &mut [Partial]| {
                for (partial, m) in partials.iter_mut().zip(&measures) {
                    if all_hold(&m.filter, key, bucket) {
                        partial.merge(cell, m.state);
                    }
                }
            };
            if by.is_empty() {
                merge(&mut total);
                continue;
            }
            group.clear();
            group.extend(by.iter().map(|s| s.value(key, bucket)));
            match groups.get_mut(group.as_slice()) {
                Some(partials) => merge(partials),
                None => {
                    let mut partials = empty();
                    merge(&mut partials);
                    groups.insert(group.clone(), partials);
                }
            }
        }
        if by.is_empty() {
            // As in SQL without GROUP BY: one row even when nothing matches.
            groups.insert(Vec::new(), total);
        }

        let mut rows: Vec<(Vec<Datum>, Vec<Value>)> = groups
            .into_iter()
            .map(|(key, partials)| {
                (
                    key.into_iter().map(Cow::into_owned).collect(),
                    partials.into_iter().map(Partial::finish).collect(),
                )
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Answer { rows, scales })
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

    fn source(&self, fact: &Fact, path: &KeyPath) -> Source<'_> {
        let slot = |s: Slot| fact.rollup_key().position(|k| k == s).expect(CHECKED);
        match self.spec.resolve(fact, path).expect(CHECKED) {
            Key::Column(c) => Source::Slot(slot(Slot::Column(c))),
            Key::Ref(r) => Source::Slot(slot(Slot::Ref(r))),
            Key::Attr(r, attr) => {
                let d = self.dim_index(r.dim.as_str()).expect(CHECKED);
                let dim = &self.spec.dims()[d];
                Source::Dim {
                    slot: slot(Slot::Ref(r)),
                    table: &self.dims[d],
                    column: iter::once(&dim.key)
                        .chain(&dim.attrs)
                        .position(|a| a == attr)
                        .expect(CHECKED),
                }
            }
        }
    }

    fn conditions<'a>(&'a self, fact: &Fact, conds: &'a [Cond]) -> Vec<Condition<'a>> {
        conds
            .iter()
            .map(|c| Condition {
                source: self.source(fact, &c.path),
                values: &c.values,
            })
            .collect()
    }
}

/// Where the value of an ask key comes from, given a cell key.
enum Source<'a> {
    /// A by column or ref id.
    Slot(usize),
    /// A column of the dim row the ref in `slot` points at: 0 for the dim
    /// key, then the attributes. Null when the dim has no such row.
    Dim {
        slot: usize,
        table: &'a DimTable,
        column: usize,
    },
    Time(TimeKey, Grain),
}

impl<'a> Source<'a> {
    fn scale(&self, rollup: &Rollup) -> u32 {
        match *self {
            Source::Slot(slot) => rollup.key_scale(slot),
            Source::Dim { table, column, .. } => table.scale(column),
            Source::Time(..) => 0,
        }
    }

    fn value<'k>(&self, key: &'k [Datum], bucket: i64) -> Cow<'k, Datum>
    where
        'a: 'k,
    {
        match *self {
            Source::Slot(slot) => Cow::Borrowed(&key[slot]),
            Source::Dim {
                slot,
                table,
                column,
            } => table
                .get(&key[slot])
                .map_or(Cow::Owned(Datum::Null), |row| Cow::Borrowed(&row[column])),
            Source::Time(t, grain) => Cow::Owned(Datum::Int(t.start(bucket, grain))),
        }
    }
}

/// A [`Cond`] ready to test cells.
struct Condition<'a> {
    source: Source<'a>,
    values: &'a [Literal],
}

fn all_hold(conditions: &[Condition], key: &[Datum], bucket: i64) -> bool {
    conditions.iter().all(|c| {
        let v = c.source.value(key, bucket);
        c.values.iter().any(|lit| v.matches(lit))
    })
}

/// A measure of the ask, ready to merge cells.
struct Asked<'a> {
    /// The measure over no cells.
    empty: Partial,
    /// See [`Rollup::state_of`].
    state: Option<usize>,
    filter: Vec<Condition<'a>>,
}

/// One measure merged over the cells of one group.
#[derive(Clone)]
enum Partial {
    Count(u64),
    Sum { sum: i128, count: u64 },
    Avg { sum: i128, count: u64 },
    Min(Option<i64>),
    Max(Option<i64>),
    Distinct(HashSet<Datum>),
}

impl Partial {
    fn new(agg: &Agg) -> Partial {
        match agg {
            Agg::Count => Partial::Count(0),
            Agg::Sum(_) => Partial::Sum { sum: 0, count: 0 },
            Agg::Avg(_) => Partial::Avg { sum: 0, count: 0 },
            Agg::Min(_) => Partial::Min(None),
            Agg::Max(_) => Partial::Max(None),
            Agg::CountDistinct(_) => Partial::Distinct(HashSet::new()),
        }
    }

    fn merge(&mut self, cell: &Cell, state: Option<usize>) {
        match (self, state.map(|i| &cell.state[i])) {
            (Partial::Count(count), None) => *count += cell.rows,
            (
                Partial::Sum { sum, count } | Partial::Avg { sum, count },
                Some(State::Sum {
                    sum: cell_sum,
                    count: cell_count,
                }),
            ) => {
                *sum += cell_sum;
                *count += cell_count;
            }
            (Partial::Min(min), Some(State::Extent(counts))) => {
                if let Some(&v) = counts.keys().next() {
                    *min = Some(min.map_or(v, |m| m.min(v)));
                }
            }
            (Partial::Max(max), Some(State::Extent(counts))) => {
                if let Some(&v) = counts.keys().next_back() {
                    *max = Some(max.map_or(v, |m| m.max(v)));
                }
            }
            (Partial::Distinct(seen), Some(State::Distinct(counts))) => {
                seen.extend(counts.keys().cloned());
            }
            _ => unreachable!("Rollup::state_of gives the state the measure reads"),
        }
    }

    fn finish(self) -> Value {
        match self {
            Partial::Count(count) => Value::Int(count.into()),
            Partial::Sum { count: 0, .. }
            | Partial::Avg { count: 0, .. }
            | Partial::Min(None)
            | Partial::Max(None) => Value::Null,
            Partial::Sum { sum, .. } => Value::Int(sum),
            Partial::Avg { sum, count } => Value::Avg { sum, count },
            Partial::Min(Some(v)) | Partial::Max(Some(v)) => Value::Int(v.into()),
            Partial::Distinct(seen) => Value::Int(seen.len() as i128),
        }
    }
}
