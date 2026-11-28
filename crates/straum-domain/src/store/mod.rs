//! The store: where rows land and asks read. It holds each fact's rollup
//! (its cells, per bucket and width) and each dim's projection (its rows
//! by dim key), and nothing else. Rows come in through `change.rs`, asks
//! are answered in `ask.rs`. A fact whose joins read only fact columns
//! also keeps bound cells, keyed by the dim columns themselves
//! (`bound.rs`).

mod ask;
mod bound;
mod cell_loop;
mod change;
mod groups;
mod lookup;

use std::ops::Range;

use crate::identifier::Identifier;
use crate::projection::{AttributeIndex, Projection};
use crate::rollup::{KeptFrom, PartIndex, Rollup};
use crate::spec::{Fact, Key, KeyPart, Ref, Spec};
use crate::value::{ColumnType, Datum, Value};

use bound::Bound;

const CHECKED: &str = "checked by Spec::new or Spec::check";
const IN_SPEC: &str = "the table is in the spec";
const POURED: &str = "a bound cell takes any cell of its fact";

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
/// fewer and answer its asks (ADR-0006). A dim change moves
/// the bound cells whose join columns name the changed key.
#[derive(Debug)]
pub struct Store {
    spec: Spec,
    /// One per fact, in the spec's order: its keyed cells.
    rollups: Vec<Rollup>,
    /// One per dim, in the spec's order.
    projections: Vec<Projection>,
    /// Per fact. None for a fact without refs, whose keyed cells are
    /// bound already, for one whose refs all join on `by` keys, whose
    /// bound cells would be as many as its keyed cells, and for one with
    /// a ref through another dim.
    bound: Vec<Option<Bound>>,
}

/// One row per group, sorted by key: the values of the ask's `by`, then its
/// measures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// One per value of a row, the `by` keys then the measures.
    pub columns: Vec<AnswerColumn>,
    pub rows: Vec<AnswerRow>,
}

/// Which rows of an answer to build: at most `count` from `start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start: Start,
    pub count: usize,
}

/// Where a window starts, as a Python slice does: a row counted from the
/// first, which is 0, or back from the end, the last row being 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    FromFirst(usize),
    FromLast(usize),
}

/// Which of an answer's rows were built: `count` of them from row
/// `first`, of `total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cut {
    pub first: usize,
    pub count: usize,
    pub total: usize,
}

impl Cut {
    /// Every row of `total`.
    pub fn whole(total: usize) -> Cut {
        Cut {
            first: 0,
            count: total,
            total,
        }
    }

    /// The rows of `total` that `window` shows, every one without it. A
    /// start past either end is held at it, as a slice's is, so no
    /// window fails.
    pub fn of(window: Option<Window>, total: usize) -> Cut {
        let Some(Window { start, count }) = window else {
            return Cut::whole(total);
        };
        let first = match start {
            Start::FromFirst(row) => row.min(total),
            Start::FromLast(rows) => total.saturating_sub(rows),
        };
        let end = first.saturating_add(count).min(total);
        Cut {
            first,
            count: end - first,
            total,
        }
    }

    /// The row after the last one built.
    pub fn end(&self) -> usize {
        self.first + self.count
    }
}

/// An answer cut to a window, and where its rows lie among all of the
/// answer's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowAnswer {
    pub answer: Answer,
    pub cut: Cut,
}

/// One group of an answer: its key values and its measures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerRow {
    /// The values of the ask's `by`.
    pub keys: Vec<Datum>,
    /// One per measure the ask names.
    pub values: Vec<Value>,
}

/// The name and form of one value of each answer row.
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
    /// An empty store for the views of `spec`.
    pub fn new(spec: Spec) -> Store {
        let mut rollups = Vec::with_capacity(spec.facts().len());
        let mut bound = Vec::with_capacity(spec.facts().len());
        for fact in spec.facts() {
            let keyed = Rollup::new(fact);
            bound.push(Bound::new(&spec, fact, &keyed));
            rollups.push(keyed);
        }
        let mut projections = Vec::with_capacity(spec.dims().len());
        for dim in spec.dims() {
            projections.push(Projection::new(dim));
        }
        Store {
            spec,
            rollups,
            projections,
            bound,
        }
    }

    /// Moves the horizon of every width kept for a time to `at`, in Unix
    /// seconds, dropping the buckets before it. An earlier `at` than
    /// before moves none back. The keyed and bound cells move together, so
    /// a dim change finds the same buckets in both.
    pub fn advance(&mut self, at: i64) {
        for rollup in &mut self.rollups {
            rollup.advance(at);
        }
        for bound in self.bound.iter_mut().flatten() {
            bound.rollup.advance(at);
        }
    }

    /// The spec the store was made for.
    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    /// Keyed cells at the `date_bin` width.
    pub fn cells(&self, fact: &str) -> Option<usize> {
        let fact = self.fact_index(fact)?;
        Some(self.keyed(fact).cells())
    }

    /// Keyed cells at each width, the `date_bin` width first, then the
    /// wider ones in the spec's order.
    pub fn width_cells(&self, fact: &str) -> Option<Vec<usize>> {
        let fact = self.fact_index(fact)?;
        Some(self.keyed(fact).level_cells().to_vec())
    }

    /// Bound cells at the `date_bin` width, while the fact has them.
    pub fn bound_cells(&self, fact: &str) -> Option<usize> {
        let fact = self.fact_index(fact)?;
        let bound = self.bound[fact.0].as_ref()?;
        Some(bound.rollup.cells())
    }

    /// Rows a dim holds, or none for a table that is no dim.
    pub fn dim_rows(&self, dim: &str) -> Option<usize> {
        let dim = self.dim_index(dim)?;
        Some(self.projections[dim.0].len())
    }

    /// Every bucket at any width of the cells an ask reads, in Unix
    /// seconds: the range an ask without bounds reads. These are the bound
    /// cells when the fact keeps them, else the keyed cells. The `date_bin`
    /// buckets while that width is kept for good; from the first widest
    /// bucket once it is kept for a time. None while the fact has no bucket.
    pub fn range(&self, fact: &str) -> Option<Range<i64>> {
        let fact = self.fact_index(fact)?;
        self.read(fact).rollup().open_range()
    }

    /// Each width the view keeps only for a time, narrowest first, with
    /// its horizon; empty for a view that keeps every width.
    pub fn horizons(&self, fact: &str) -> Option<Vec<KeptFrom>> {
        let fact = self.fact_index(fact)?;
        Some(self.keyed(fact).horizons())
    }

    /// The type of a key the view names, as the load set it.
    pub fn key_type(&self, fact: &str, key: &Identifier) -> Option<ColumnType> {
        let index = self.fact_index(fact)?;
        let fact = self.fact(index);
        let resolved = fact.resolve(key)?;
        Some(self.keyed_key_type(fact, self.keyed(index), resolved))
    }

    /// The type of a key read through the keyed cells: a `by` key's part,
    /// or the dim column a ref reads.
    fn keyed_key_type(&self, fact: &Fact, keyed: &Rollup, key: Key<'_>) -> ColumnType {
        match key {
            Key::ByKey(by) => keyed.key_type(key_part_of(fact, by.key_part())),
            Key::Attribute(reference, attribute) => {
                let read = self.attribute_index(reference, &attribute.column);
                self.projection(reference.dim.as_str()).attribute_type(read)
            }
        }
    }

    /// Where the dim `reference` joins keeps `column` among the
    /// attributes of its projection.
    fn attribute_index(&self, reference: &Ref, column: &Identifier) -> AttributeIndex {
        let dim = self.spec.dim(reference.dim.as_str()).expect(CHECKED);
        AttributeIndex(position_of(&dim.attributes, column))
    }

    /// The type of the view's time column, as the load found it.
    pub fn time_type(&self, fact: &str) -> Option<ColumnType> {
        let index = self.fact_index(fact)?;
        Some(self.keyed(index).time_type())
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

    fn fact(&self, fact: FactIndex) -> &Fact {
        &self.spec.facts()[fact.0]
    }

    /// The fact's keyed cells.
    fn keyed(&self, fact: FactIndex) -> &Rollup {
        &self.rollups[fact.0]
    }

    fn projection(&self, dim: &str) -> &Projection {
        let dim = self.dim_index(dim).expect(IN_SPEC);
        &self.projections[dim.0]
    }
}

/// Where `name` is among `names`, which the spec checked it is.
fn position_of(names: &[Identifier], name: &Identifier) -> usize {
    for (position, each) in names.iter().enumerate() {
        if each == name {
            return position;
        }
    }
    panic!("{CHECKED}")
}

fn dim_index(spec: &Spec, name: &str) -> Option<DimIndex> {
    for (position, dim) in spec.dims().iter().enumerate() {
        if dim.name.as_str() == name {
            return Some(DimIndex(position));
        }
    }
    None
}

/// The part of a fact's keyed rollup key that `part` is.
fn key_part_of(fact: &Fact, part: KeyPart<'_>) -> PartIndex {
    for (position, key) in fact.rollup_key().into_iter().enumerate() {
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
            for (position, part) in fact.by_parts().into_iter().enumerate() {
                if part == by.key_part() {
                    return PartIndex(position);
                }
            }
            panic!("{CHECKED}")
        }
        Key::Attribute(reference, attribute) => {
            let by_parts = fact.by_parts().len();
            for (position, (other, read)) in fact.attributes().into_iter().enumerate() {
                if other.alias == reference.alias && read.name == attribute.name {
                    return PartIndex(by_parts + position);
                }
            }
            panic!("{CHECKED}")
        }
    }
}
