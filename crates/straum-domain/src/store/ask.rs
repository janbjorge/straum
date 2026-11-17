//! Answering an ask. The spec checks it and names its view; the store
//! picks the cells that answer it (bound or keyed), works out how each
//! key, filter and join reads a cell, runs the cell loop over the buckets
//! of the ask's range (an open end reaching to the buckets held), and
//! turns the groups met into answer rows.

use std::ops::Range;

use crate::aggregate::Partials;
use crate::ask::{Ask, AskError, AskKey, Comparison};
use crate::hash::FastMap;
use crate::identifier::Identifier;
use crate::projection::AttributeIndex;
use crate::rollup::{Cover, CoveredBucket, Rollup};
use crate::spec::{Condition, Fact, Key, KeyPart, Measure, Ref, RefKind, Via};

use super::cell_loop::{Asked, CellLoop, Expander, RefPlan, ViaFrom};
use super::groups::{Grouping, Groups};
use super::lookup::{
    AttributeLookup, ConditionCheck, JoinCheck, JoinKeeps, Joined, Known, Lookup, Memo, UNKNOWN,
    Wanted,
};
use crate::value::{Threshold, Value};

use super::{Answer, AnswerColumn, CHECKED, Store, bound_part_of, key_part_of, position_of};

/// The cells an ask reads: a fact's bound cells, where every key the
/// view names is a part of the cell key, or its keyed cells, where a dim
/// column is read through the projection.
// The `'s` in this file borrows the store for the length of an ask: an
// ask reads the cells in place.
#[derive(Clone, Copy)]
pub(super) enum Read<'s> {
    Bound(&'s Rollup),
    Keyed(&'s Rollup),
}

impl<'s> Read<'s> {
    /// The rollup whose cells the ask reads.
    pub(super) fn rollup(self) -> &'s Rollup {
        match self {
            Read::Bound(rollup) | Read::Keyed(rollup) => rollup,
        }
    }
}

/// The view an ask names, and the cells that answer it.
pub(super) struct Reading<'s> {
    pub(super) fact: &'s Fact,
    pub(super) read: Read<'s>,
}

impl Store {
    /// Every key is a part of the rollup key or a dim value reached
    /// through the parts of a ref's via columns, so the cell loop only
    /// looks up what each code maps to, once per code. A view whose joins
    /// find one row a key at most walks the cells once each, passing over
    /// those an inner, semi or anti join drops; one with a key of several
    /// rows, or a join through another dim, walks each cell once per row
    /// its joins find, as PostgreSQL repeats the fact row.
    pub fn ask(&self, ask: &Ask) -> Result<Answer, Vec<AskError>> {
        let Reading { fact, read } = self.reading(ask)?;
        self.check_types(fact, read, ask)?;
        let rollup = read.rollup();
        let mut asked = Vec::with_capacity(ask.measures.len());
        for name in &ask.measures {
            asked.push(fact.measure(name.as_str()).expect(CHECKED));
        }
        let mut cell_loop = self.cell_loop(fact, read, ask, &asked);
        let columns = answer_columns(ask, rollup, &cell_loop.by, &asked);
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
                cell_loop.ask_once(&mut joins, ask);
            }
            Some(mut expander) => cell_loop.ask_expanded(ask, &mut expander),
        }
        let checks = having_checks(ask, &columns);
        let rows = cell_loop.answer_rows(&checks);
        Ok(Answer { columns, rows })
    }

    /// Cells the ask reads.
    pub fn cells_read(&self, ask: &Ask) -> Result<usize, Vec<AskError>> {
        let reading = self.reading(ask)?;
        let mut cells = 0;
        for covered in buckets(reading.read.rollup(), ask) {
            cells += covered.cells.len();
        }
        Ok(cells)
    }

    /// The ask's fact and the cells that answer it: the bound cells when
    /// the fact keeps them, else the keyed cells.
    pub(super) fn reading(&self, ask: &Ask) -> Result<Reading<'_>, Vec<AskError>> {
        let fact = self.spec.check_ask(ask)?;
        let index = self.fact_index(fact.name.as_str()).expect(CHECKED);
        let read = match &self.bound[index.0] {
            Some(bound) => Read::Bound(&bound.rollup),
            None => Read::Keyed(self.keyed(index)),
        };
        Ok(Reading { fact, read })
    }

    /// The cell loop of an ask before it meets a cell: its groupings,
    /// filters and measures.
    fn cell_loop<'s>(
        &'s self,
        fact: &Fact,
        read: Read<'s>,
        ask: &Ask,
        asked: &[&Measure],
    ) -> CellLoop<'s> {
        let rollup = read.rollup();
        let buckets = buckets(rollup, ask).count();
        let mut by = Vec::with_capacity(ask.keys.len());
        for key in &ask.keys {
            by.push(match key {
                AskKey::Key(name) => Grouping::lookup(self.lookup(fact, read, name), rollup),
                AskKey::Bucket => Grouping::time(fact.time.width, buckets),
            });
        }
        let mut groups = Groups::new(&by);
        let mut filter = self.condition_checks(fact, read, &fact.filter);
        filter.extend(self.condition_checks(fact, read, &ask.filter));
        let mut measures = Vec::with_capacity(asked.len());
        for measure in asked {
            measures.push(Asked {
                partials: Partials::new(&measure.aggregate),
                state: rollup.state_of(&measure.aggregate),
                filter: self.condition_checks(fact, read, &measure.filter),
            });
        }
        if by.is_empty() {
            // As in SQL without GROUP BY: one row even when nothing matches.
            groups.numbers.clear();
            groups.insert();
        }
        CellLoop {
            rollup,
            by,
            groups,
            filter,
            measures,
        }
    }

    /// Only the load knows the types, so this check comes after
    /// [`Spec::check`](crate::spec::Spec::check).
    fn check_types(&self, fact: &Fact, read: Read<'_>, ask: &Ask) -> Result<(), Vec<AskError>> {
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
        let mut plans = Vec::with_capacity(fact.refs.len());
        for reference in &fact.refs {
            plans.push(self.ref_plan(fact, reference));
        }
        Expander::new(plans)
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
                let attributes = &parent.expect(CHECKED).attributes;
                ViaFrom::Through {
                    reference: *through,
                    attribute: AttributeIndex(position_of(attributes, column)),
                }
            }
        }
    }

    /// The joins that keep or drop a whole cell, for a view that counts
    /// each cell once: every ref but the left ones, which keep them all.
    fn join_checks<'s>(&'s self, fact: &Fact, rollup: &Rollup) -> Vec<JoinCheck<'s>> {
        let mut checks = Vec::new();
        for reference in &fact.refs {
            if reference.kind == RefKind::Left {
                continue;
            }
            checks.push(JoinCheck {
                joined: self.joined(fact, reference, rollup),
                keeps: JoinKeeps::of(reference.kind),
            });
        }
        checks
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

    /// Where a key's values come from: a part of the cells read, or a
    /// dim's projection reached through a ref.
    fn lookup<'s>(&'s self, fact: &Fact, read: Read<'s>, name: &Identifier) -> Lookup<'s> {
        let rollup = match read {
            Read::Bound(_) => return Lookup::KeyPart(bound_part_of(fact, name)),
            Read::Keyed(rollup) => rollup,
        };
        match fact.resolve(name).expect(CHECKED) {
            Key::ByKey(by) => Lookup::KeyPart(key_part_of(fact, by.key_part())),
            Key::Attribute(reference, attribute) => {
                let dim = self.spec.dim(reference.dim.as_str()).expect(CHECKED);
                Lookup::Attribute(AttributeLookup {
                    reference: ref_position(fact, reference),
                    attribute: AttributeIndex(position_of(&dim.attributes, &attribute.column)),
                    joined: self.joined(fact, reference, rollup),
                })
            }
        }
    }

    fn condition_checks<'s>(
        &'s self,
        fact: &Fact,
        read: Read<'s>,
        filter: &[Condition],
    ) -> Vec<ConditionCheck<'s>> {
        let mut checks = Vec::with_capacity(filter.len());
        for condition in filter {
            let lookup = self.lookup(fact, read, &condition.key);
            checks.push(ConditionCheck {
                memo: vec![Known::Unknown; lookup.bound(read.rollup())],
                lookup,
                wanted: Wanted::new(&condition.values),
            });
        }
        checks
    }
}

/// Where `reference` is among the fact's refs.
fn ref_position(fact: &Fact, reference: &Ref) -> usize {
    for (position, other) in fact.refs.iter().enumerate() {
        if other.alias == reference.alias {
            return position;
        }
    }
    panic!("{CHECKED}")
}

/// The buckets an ask reads: `date_bin` buckets when it groups by the
/// bucket, else the widest that fit.
// An iterator, not a `Vec`: an ask allocates per group, never per bucket
// (`an_ask_allocates_per_group_not_per_cell` in tests/cost.rs).
pub(super) fn buckets<'r>(
    rollup: &'r Rollup,
    ask: &Ask,
) -> impl Iterator<Item = CoveredBucket<'r>> {
    let mut cover = Cover::WidestFit;
    for key in &ask.keys {
        if *key == AskKey::Bucket {
            cover = Cover::EachDateBin;
        }
    }
    let range = read_range(rollup, ask);
    rollup.cover(range.start, range.end, cover)
}

/// The range an ask reads: its bounds, an open end at the edge of the
/// buckets held. With no bucket held there is nothing to read, so an open
/// end stands at 0.
fn read_range(rollup: &Rollup, ask: &Ask) -> Range<i64> {
    let held = rollup.range().unwrap_or(0..0);
    let start = ask.from.unwrap_or(held.start);
    let end = ask.to.unwrap_or(held.end);
    start..end
}

/// The `by` keys, then the measures, as the ask names them.
/// A HAVING condition made ready for the groups: where its measure sits
/// among a row's values, and the value scaled to the measure's scale.
pub(super) struct HavingCheck {
    pub(super) position: usize,
    comparison: Comparison,
    threshold: Threshold,
}

/// The ask's HAVING conditions, each scaled once to its measure's scale.
fn having_checks(ask: &Ask, columns: &[AnswerColumn]) -> Vec<HavingCheck> {
    let mut checks = Vec::with_capacity(ask.having.len());
    for condition in &ask.having {
        let position = position_of(&ask.measures, &condition.measure);
        let scale = columns[ask.keys.len() + position].scale;
        let threshold = Threshold::new(scale, &condition.value).expect(CHECKED);
        checks.push(HavingCheck {
            position,
            comparison: condition.comparison,
            threshold,
        });
    }
    checks
}

/// Whether a group's measure `values` meet every check. The values a
/// check doesn't read may not be finished yet.
pub(super) fn meets(values: &[Value], checks: &[HavingCheck]) -> bool {
    for check in checks {
        let Some(order) = values[check.position].order_against(check.threshold) else {
            return false;
        };
        if !check.comparison.holds(order) {
            return false;
        }
    }
    true
}

fn answer_columns(
    ask: &Ask,
    rollup: &Rollup,
    by: &[Grouping<'_>],
    measures: &[&Measure],
) -> Vec<AnswerColumn> {
    let mut columns = Vec::with_capacity(by.len() + measures.len());
    for (key, grouping) in ask.keys.iter().zip(by) {
        columns.push(AnswerColumn {
            name: key.to_string(),
            scale: grouping.column_type(rollup).scale(),
            is_time: *key == AskKey::Bucket,
        });
    }
    for (name, measure) in ask.measures.iter().zip(measures) {
        columns.push(AnswerColumn {
            name: name.to_string(),
            scale: rollup.scale_of(&measure.aggregate),
            is_time: false,
        });
    }
    columns
}
