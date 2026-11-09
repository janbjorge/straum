//! Every fact's rollup and every dim's rows for one spec, and asks answered
//! from them.

use std::borrow::Cow;
use std::collections::HashMap;
use std::iter;

use crate::agg::Partial;
use crate::ask::{Ask, AskError, AskKey, TimeKey};
use crate::rollup::{Cells, DimTable, Rollup};
use crate::spec::{Cond, Fact, Grain, Key, KeyPath, Literal, Slot, Spec};
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

    /// How many cells answering `ask` reads, whatever they hold: its cost.
    /// Those are the cells of the buckets that cover its range.
    pub fn cells_read(&self, ask: &Ask) -> Result<usize, Vec<AskError>> {
        let fact = self.spec.check(ask)?;
        let rollup = &self.rollups[self.fact_index(fact.name.as_str()).expect(CHECKED)];
        Ok(cover(rollup, ask).map(|(_, cells)| cells.len()).sum())
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
        let cells = cover(rollup, ask)
            .flat_map(|(bucket, cells)| cells.iter().map(move |cell| (bucket, cell)));
        for (bucket, (key, rows, states)) in cells {
            let at = At {
                rollup,
                key,
                bucket,
            };
            if !all_hold(&filter, &at) {
                continue;
            }
            let merge = |partials: &mut [Partial]| {
                for (partial, m) in partials.iter_mut().zip(&measures) {
                    if all_hold(&m.filter, &at) {
                        partial.merge(rows, m.state.map(|i| &states[i]));
                    }
                }
            };
            if by.is_empty() {
                merge(&mut total);
                continue;
            }
            group.clear();
            group.extend(by.iter().map(|s| s.value(&at)));
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

/// The buckets `ask` reads: every `date_bin` bucket in its range when it
/// groups by the bucket, else the widest buckets that fit.
fn cover<'r>(rollup: &'r Rollup, ask: &Ask) -> impl Iterator<Item = (i64, &'r Cells)> {
    let each_bucket = ask.by.iter().any(|key| matches!(key, AskKey::Time(_)));
    rollup.cover(ask.from, ask.to, each_bucket)
}

/// A cell an ask reads: its key codes and bucket, and the rollup whose
/// codes they are.
struct At<'r> {
    rollup: &'r Rollup,
    key: &'r [u32],
    bucket: i64,
}

/// Where the value of an ask key comes from, given a cell.
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

    fn value<'k>(&self, at: &At<'k>) -> Cow<'k, Datum>
    where
        'a: 'k,
    {
        match *self {
            Source::Slot(slot) => Cow::Borrowed(at.rollup.value(slot, at.key[slot])),
            Source::Dim {
                slot,
                table,
                column,
            } => table
                .get(at.rollup.value(slot, at.key[slot]))
                .map_or(Cow::Owned(Datum::Null), |row| Cow::Borrowed(&row[column])),
            Source::Time(t, grain) => Cow::Owned(Datum::Int(t.start(at.bucket, grain))),
        }
    }
}

/// A [`Cond`] ready to test cells.
struct Condition<'a> {
    source: Source<'a>,
    values: &'a [Literal],
}

fn all_hold(conditions: &[Condition], at: &At) -> bool {
    conditions.iter().all(|c| {
        let v = c.source.value(at);
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
