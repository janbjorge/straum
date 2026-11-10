use std::borrow::Cow;
use std::collections::HashMap;
use std::iter;

use crate::agg::Partials;
use crate::ask::{Ask, AskError, AskKey, TimeKey};
use crate::rollup::{Cells, DimTable, Rollup};
use crate::spec::{Cond, Fact, Grain, Key, KeyPath, Literal, Slot, Spec};
use crate::value::{ColumnType, Datum, Value};

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
    /// One per value of a row, the `by` keys then the measures.
    pub columns: Vec<AnswerColumn>,
    pub rows: Vec<(Vec<Datum>, Vec<Value>)>,
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
            dims: spec.dims().iter().map(DimTable::new).collect(),
            spec,
        }
    }

    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    pub fn rollup(&mut self, fact: &str) -> Option<&mut Rollup> {
        let i = self.fact_index(fact)?;
        Some(&mut self.rollups[i])
    }

    pub fn dim(&mut self, dim: &str) -> Option<&mut DimTable> {
        let i = self.dim_index(dim)?;
        Some(&mut self.dims[i])
    }

    pub fn cells(&self, fact: &str) -> Option<usize> {
        Some(self.rollups[self.fact_index(fact)?].cells())
    }

    pub fn rows(&self, dim: &str) -> Option<usize> {
        Some(self.dims[self.dim_index(dim)?].rows())
    }

    pub fn cells_read(&self, ask: &Ask) -> Result<usize, Vec<AskError>> {
        let fact = self.spec.check(ask)?;
        let rollup = &self.rollups[self.fact_index(fact.name.as_str()).expect(CHECKED)];
        Ok(cover(rollup, ask).map(|(_, cells)| cells.len()).sum())
    }

    /// Dim attributes are read per code, after the rollup: the cell loop only
    /// looks up what each code of a key column maps to.
    pub fn ask(&self, ask: &Ask) -> Result<Answer, Vec<AskError>> {
        let fact = self.spec.check(ask)?;
        let rollup = &self.rollups[self.fact_index(fact.name.as_str()).expect(CHECKED)];
        // Only the load knows the types, so this check comes after Spec::check.
        let wrong: Vec<AskError> = (ask.filter.iter())
            .flat_map(|cond| {
                let column = self.source(fact, &cond.path).column_type(rollup);
                (cond.values.iter())
                    .filter(move |value| !column.fits(value))
                    .map(move |value| AskError::WrongType {
                        path: cond.path.clone(),
                        column,
                        value: value.clone(),
                    })
            })
            .collect();
        if !wrong.is_empty() {
            return Err(wrong);
        }
        let buckets = cover(rollup, ask).count();

        let mut by: Vec<Part> = ask
            .by
            .iter()
            .map(|key| {
                let source = match key {
                    AskKey::Key(path) => self.source(fact, path),
                    AskKey::Time(t) => Source::Time(*t, fact.time.grain),
                };
                Part::new(source, rollup, buckets)
            })
            .collect();
        let mut groups = Groups::new(&by);
        let mut filter = self.tests(fact, rollup, &ask.filter);
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
                AskKey::Key(path) => path.to_string(),
            },
            scale: p.source.column_type(rollup).scale(),
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
            groups.make(&mut by, rollup, &[]);
        }
        // Per cell of a bucket: 1 while it may pass, then its group; 0 for
        // cells passed over. Again per measure with a filter of its own.
        let mut cell_groups = Vec::new();
        let mut measure_groups = Vec::new();
        for (bucket, cells) in cover(rollup, ask) {
            by.iter_mut().for_each(|p| p.enter(bucket));
            cell_groups.clear();
            cell_groups.resize(cells.len(), 1);
            for t in &mut filter {
                t.pass(rollup, cells, &mut cell_groups);
            }
            groups.assign(&mut by, rollup, cells, &mut cell_groups);
            for m in &mut measures {
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

        measures
            .iter_mut()
            .for_each(|m| m.partials.grow(groups.len()));
        let mut keys = groups.keys.into_iter().map(Cow::into_owned);
        let mut rows: Vec<(Vec<Datum>, Vec<Value>)> = (1..=groups.made)
            .map(|g| {
                (
                    keys.by_ref().take(by.len()).collect(),
                    measures.iter().map(|m| m.partials.finish(g)).collect(),
                )
            })
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Answer { columns, rows })
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
            Key::Ref(r) => Source::Slot(slot(Slot::Column(&r.via))),
            Key::Attr(r, attr) => {
                let d = self.dim_index(r.dim.as_str()).expect(CHECKED);
                let dim = &self.spec.dims()[d];
                Source::Dim {
                    slot: slot(Slot::Column(&r.via)),
                    table: &self.dims[d],
                    column: iter::once(&dim.key)
                        .chain(&dim.attrs)
                        .position(|a| a == attr)
                        .expect(CHECKED),
                }
            }
        }
    }

    fn tests<'a>(&'a self, fact: &Fact, rollup: &Rollup, conds: &'a [Cond]) -> Vec<Test<'a>> {
        conds
            .iter()
            .map(|c| {
                let source = self.source(fact, &c.path);
                let (Source::Slot(slot) | Source::Dim { slot, .. }) = source else {
                    unreachable!("conditions name rollup-key parts");
                };
                Test {
                    memo: vec![Memo::Unknown; rollup.codes(slot)],
                    slot,
                    source,
                    values: &c.values,
                }
            })
            .collect()
    }
}

fn cover<'r>(rollup: &'r Rollup, ask: &Ask) -> impl Iterator<Item = (i64, &'r Cells)> {
    let each_bucket = ask.by.iter().any(|key| matches!(key, AskKey::Time(_)));
    rollup.cover(ask.from, ask.to, each_bucket)
}

enum Source<'a> {
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
    fn column_type(&self, rollup: &Rollup) -> ColumnType {
        match *self {
            Source::Slot(slot) => rollup.key_type(slot),
            Source::Dim { table, column, .. } => table.column_type(column),
            Source::Time(..) => ColumnType::Time,
        }
    }

    /// Time keys number their periods in [`Part`] instead.
    fn value<'k>(&self, rollup: &'k Rollup, key: &[u32]) -> Cow<'k, Datum>
    where
        'a: 'k,
    {
        match *self {
            Source::Slot(slot) => Cow::Borrowed(rollup.value(slot, key[slot])),
            Source::Dim {
                slot,
                table,
                column,
            } => table
                .get(rollup.value(slot, key[slot]))
                .map_or(Cow::Owned(Datum::Null), |row| {
                    Cow::Borrowed(table.value(column, row[column]))
                }),
            Source::Time(..) => unreachable!("time keys are numbered by period"),
        }
    }
}

/// A [`Cond`] of an ask, worked out once per code of its slot as the ask
/// meets the code.
struct Test<'a> {
    source: Source<'a>,
    values: &'a [Literal],
    slot: usize,
    memo: Vec<Memo>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Memo {
    Unknown,
    Holds,
    Fails,
}

impl<'a> Test<'a> {
    /// Whether a cell passes is as likely as not, so the loop selects rather
    /// than branches; only a code met first branches.
    fn pass(&mut self, rollup: &'a Rollup, cells: &'a Cells, groups: &mut [u32]) {
        for (i, g) in groups.iter_mut().enumerate() {
            let mut memo = self.memo[cells.code(i, self.slot) as usize];
            if memo == Memo::Unknown && *g != 0 {
                memo = self.resolve(rollup, cells.key(i));
            }
            *g = if memo == Memo::Holds { *g } else { 0 };
        }
    }

    fn resolve(&mut self, rollup: &'a Rollup, key: &'a [u32]) -> Memo {
        let v = self.source.value(rollup, key);
        let memo = if self.values.iter().any(|lit| v.matches(lit)) {
            Memo::Holds
        } else {
            Memo::Fails
        };
        self.memo[key[self.slot] as usize] = memo;
        memo
    }
}

/// A by key of an ask, its values numbered without hashing any: a slot
/// key's by their codes, a dim key's by the codes of its dim column, and a
/// time key's in time order.
struct Part<'a> {
    source: Source<'a>,
    /// For a dim key, per code of its slot: 1 + the number of the code's
    /// value, or 0 until the ask meets the code.
    codes: Vec<u32>,
    /// For a time key, the start of each period met so far, the bucket at
    /// hand's last.
    starts: Vec<i64>,
    bound: usize,
}

impl<'a> Part<'a> {
    fn new(source: Source<'a>, rollup: &Rollup, buckets: usize) -> Part<'a> {
        let (bound, codes) = match source {
            Source::Slot(slot) => (rollup.codes(slot), 0),
            Source::Dim {
                slot,
                table,
                column,
            } => (table.codes(column) + 1, rollup.codes(slot)),
            Source::Time(..) => (buckets, 0),
        };
        Part {
            bound,
            codes: vec![0; codes],
            starts: Vec::new(),
            source,
        }
    }

    /// Buckets come in time order, so a period's buckets come together.
    fn enter(&mut self, bucket: i64) {
        if let Source::Time(t, grain) = self.source {
            let start = t.start(bucket, grain);
            if self.starts.last() != Some(&start) {
                self.starts.push(start);
            }
        }
    }

    fn offsets(
        &mut self,
        rollup: &'a Rollup,
        cells: &'a Cells,
        stride: usize,
        groups: &[u32],
        at: &mut [usize],
    ) {
        match self.source {
            Source::Time(..) => {
                let offset = (self.starts.len() - 1) * stride;
                at.iter_mut().for_each(|a| *a += offset);
            }
            // Cells passed over too, as a branch costs more than their
            // entries, which nothing reads.
            Source::Slot(slot) => {
                for (i, a) in at.iter_mut().enumerate() {
                    *a += cells.code(i, slot) as usize * stride;
                }
            }
            Source::Dim { slot, .. } => {
                for (i, (a, &g)) in at.iter_mut().zip(groups).enumerate() {
                    let n = match self.codes[cells.code(i, slot) as usize].checked_sub(1) {
                        Some(n) => n,
                        None if g == 0 => continue,
                        None => self.number(rollup, cells.key(i)),
                    };
                    *a += n as usize * stride;
                }
            }
        }
    }

    fn number(&mut self, rollup: &'a Rollup, key: &'a [u32]) -> u32 {
        let code = |n: usize| u32::try_from(n).expect("codes are u32");
        match self.source {
            Source::Slot(slot) => key[slot],
            Source::Time(..) => code(self.starts.len() - 1),
            Source::Dim {
                slot,
                table,
                column,
            } => {
                let c = key[slot] as usize;
                if let Some(n) = self.codes[c].checked_sub(1) {
                    return n;
                }
                let n = match table.get(rollup.value(slot, key[slot])) {
                    Some(row) => row[column],
                    None => table.null(column),
                };
                self.codes[c] = n + 1;
                n
            }
        }
    }

    fn value(&self, rollup: &'a Rollup, number: u32) -> Cow<'a, Datum> {
        match self.source {
            Source::Slot(slot) => Cow::Borrowed(rollup.value(slot, number)),
            Source::Dim { table, column, .. } if (number as usize) < table.codes(column) => {
                Cow::Borrowed(table.value(column, number))
            }
            Source::Dim { .. } => Cow::Owned(Datum::Null),
            Source::Time(..) => Cow::Owned(Datum::Int(self.starts[number as usize])),
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
    fn new(by: &[Part]) -> Groups<'a> {
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
                    p.offsets(rollup, cells, stride, groups, &mut self.at);
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
                    _ => self.make(by, rollup, cells.key(i)),
                };
            }
        }
    }

    fn make(&mut self, by: &mut [Part<'a>], rollup: &'a Rollup, key: &'a [u32]) -> u32 {
        self.numbers.clear();
        self.numbers
            .extend(by.iter_mut().map(|p| p.number(rollup, key)));
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
            let values = (self.numbers.iter().zip(&*by)).map(|(&n, p)| p.value(rollup, n));
            self.keys.extend(values);
            self.made += 1;
        }
        group
    }
}

struct Asked<'a> {
    partials: Partials,
    state: Option<usize>,
    filter: Vec<Test<'a>>,
}
