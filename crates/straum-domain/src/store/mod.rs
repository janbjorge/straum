//! The store: where rows land and asks read. It holds each fact's rollup
//! (its cells, per bucket and width) and each dim's projection (its rows
//! by dim key), and nothing else. Rows come in through `change.rs`, asks
//! are answered in `ask.rs`, and `seal.rs` moves a bucket's cells out to
//! a spill and back. A fact whose joins read only fact columns also keeps
//! bound cells, keyed by the dim columns themselves (`bound.rs`).

mod ask;
mod bound;
mod cell_loop;
mod change;
mod groups;
mod lookup;
mod seal;

use std::ops::Range;

use crate::identifier::Identifier;
use crate::projection::{AttributeIndex, Projection};
use crate::rollup::{PartIndex, Rollup};
use crate::spec::{Fact, Key, KeyPart, Spec};
use crate::value::{ColumnType, Datum, Value};

use bound::Bound;

const CHECKED: &str = "checked by Spec::new or Spec::check";
const IN_SPEC: &str = "the table is in the spec";
const HOT: &str = "the bucket is hot";
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
/// fewer and answer its asks (ADR-0038, ADR-0042). A dim change moves
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

    /// The spec the store was made for.
    pub fn spec(&self) -> &Spec {
        &self.spec
    }

    /// Keyed cells at the `date_bin` width.
    pub fn cells(&self, fact: &str) -> Option<usize> {
        let fact = self.fact_index(fact)?;
        Some(self.keyed(fact).cells())
    }

    /// Bound cells at the `date_bin` width, while the fact has them.
    pub fn bound_cells(&self, fact: &str) -> Option<usize> {
        let fact = self.fact_index(fact)?;
        let bound = self.bound[fact.0].as_ref()?;
        Some(bound.rollup.cells())
    }

    /// Sealed buckets, every fact and width.
    pub fn sealed_count(&self) -> usize {
        let mut sealed = 0;
        for rollup in &self.rollups {
            sealed += rollup.sealed_count();
        }
        sealed
    }

    /// Rows a dim holds, or none for a table that is no dim.
    pub fn dim_rows(&self, dim: &str) -> Option<usize> {
        let dim = self.dim_index(dim)?;
        Some(self.projections[dim.0].len())
    }

    /// From the start of the fact's first `date_bin` bucket to the end of
    /// its last, hot or sealed, in Unix seconds; none while it has no
    /// bucket.
    pub fn range(&self, fact: &str) -> Option<Range<i64>> {
        let fact = self.fact_index(fact)?;
        self.keyed(fact).range()
    }

    /// The type of a key the view names, as the load set it.
    pub fn key_type(&self, fact: &str, key: &Identifier) -> Option<ColumnType> {
        let index = self.fact_index(fact)?;
        let fact = self.fact(index);
        match fact.resolve(key)? {
            Key::ByKey(by) => Some(self.keyed(index).key_type(key_part_of(fact, by.key_part()))),
            Key::Attribute(reference, attribute) => {
                let dim = self.spec.dim(reference.dim.as_str()).expect(CHECKED);
                let read = AttributeIndex(position_of(&dim.attributes, &attribute.column));
                Some(self.projection(reference.dim.as_str()).attribute_type(read))
            }
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
    let mut found = None;
    for (position, each) in names.iter().enumerate() {
        if each == name {
            found = Some(position);
            break;
        }
    }
    found.expect(CHECKED)
}

fn dim_index(spec: &Spec, name: &str) -> Option<DimIndex> {
    let mut found = None;
    for (position, dim) in spec.dims().iter().enumerate() {
        if dim.name.as_str() == name {
            found = Some(DimIndex(position));
            break;
        }
    }
    found
}

/// The part of a fact's keyed rollup key that `part` is.
fn key_part_of(fact: &Fact, part: KeyPart<'_>) -> PartIndex {
    let mut found = None;
    for (position, key) in fact.rollup_key().into_iter().enumerate() {
        if key == part {
            found = Some(PartIndex(position));
            break;
        }
    }
    found.expect(CHECKED)
}

/// The part of the bound cell key the view names so: the `by` keys'
/// parts, then the attributes in [`Fact::attributes`] order, as
/// [`Rollup::bound`] lays them.
fn bound_part_of(fact: &Fact, name: &Identifier) -> PartIndex {
    match fact.resolve(name).expect(CHECKED) {
        Key::ByKey(by) => {
            let mut found = None;
            for (position, part) in fact.by_parts().into_iter().enumerate() {
                if part == by.key_part() {
                    found = Some(PartIndex(position));
                    break;
                }
            }
            found.expect(CHECKED)
        }
        Key::Attribute(reference, attribute) => {
            let by_parts = fact.by_parts().len();
            let mut found = None;
            for (position, (other, read)) in fact.attributes().into_iter().enumerate() {
                if other.alias == reference.alias && read.name == attribute.name {
                    found = Some(PartIndex(by_parts + position));
                    break;
                }
            }
            found.expect(CHECKED)
        }
    }
}
