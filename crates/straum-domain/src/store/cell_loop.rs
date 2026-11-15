//! The cell loop: the pass an ask makes over the cells it reads, bucket by
//! bucket, dropping the cells its filter and joins fail and merging the
//! rest into their groups. A view whose joins find one dim row a key at
//! most walks each cell once; one whose joins can repeat a cell walks it
//! once per join row, as PostgreSQL repeats the fact row.

use std::borrow::Cow;
use std::cmp::Ordering;

use crate::aggregate::Partials;
use crate::ask::Ask;
use crate::hash::FastMap;
use crate::projection::{AttributeIndex, Projection};
use crate::rollup::{Cells, InputIndex, PartIndex, Rollup, State};
use crate::spec::RefKind;
use crate::value::{Datum, Value};

use super::AnswerRow;
use super::ask::{HavingCheck, ThawedCells, buckets, cells_of, meets};
use super::bound::{JoinRow, keep};
use super::groups::{Grouping, Groups};
use super::lookup::{ConditionCheck, JoinCheck, all_hold};

/// What the cell loop fills in: per by key its grouping, the groups met,
/// the ask's and the view's filter, and per measure its partial answers.
// The `'s` in this file borrows the store, and the thawed buckets an ask
// was handed, for the length of the ask.
pub(super) struct CellLoop<'s> {
    pub(super) rollup: &'s Rollup,
    pub(super) by: Vec<Grouping<'s>>,
    pub(super) groups: Groups<'s>,
    pub(super) filter: Vec<ConditionCheck<'s>>,
    pub(super) measures: Vec<Asked<'s>>,
}

/// A measure an ask names, as the cell loop merges it.
pub(super) struct Asked<'s> {
    pub(super) partials: Partials,
    /// The cell state the measure reads; none for a count of rows.
    pub(super) state: Option<InputIndex>,
    /// The measure's own `FILTER`.
    pub(super) filter: Vec<ConditionCheck<'s>>,
}

fn state_in(cells: &Cells, input: Option<InputIndex>) -> Option<&State> {
    match input {
        Some(input) => Some(cells.state(input)),
        None => None,
    }
}

impl<'s> CellLoop<'s> {
    /// The loop for a view that counts each cell once or not at all:
    /// one row a key at most, none joined through another dim. `joins` drop
    /// the cells an inner, semi or anti join does.
    pub(super) fn ask_once(
        &mut self,
        joins: &mut [JoinCheck<'s>],
        ask: &Ask,
        loaded: &ThawedCells<'s>,
    ) {
        let rollup = self.rollup;
        // Per cell of a bucket: 1 while it may pass, then its group; 0 for
        // cells passed over. Again per measure with a filter of its own.
        let mut cell_groups = Vec::new();
        let mut measure_groups = Vec::new();
        for covered in buckets(rollup, ask) {
            let cells = cells_of(covered, loaded);
            for grouping in &mut self.by {
                grouping.enter(covered.id.start);
            }
            cell_groups.clear();
            cell_groups.resize(cells.len(), 1);
            for join in joins.iter_mut() {
                join.pass(rollup, cells, &mut cell_groups);
            }
            for check in &mut self.filter {
                check.pass(rollup, cells, &mut cell_groups);
            }
            (self.groups).assign(&mut self.by, rollup, cells, &mut cell_groups);
            for measure in &mut self.measures {
                measure.partials.grow(self.groups.len());
                let read = if measure.filter.is_empty() {
                    &cell_groups
                } else {
                    measure_groups.clone_from(&cell_groups);
                    for check in &mut measure.filter {
                        check.pass(rollup, cells, &mut measure_groups);
                    }
                    &measure_groups
                };
                let state = state_in(cells, measure.state);
                measure.partials.merge(read, cells.rows(), state);
            }
        }
    }

    /// The loop for a view whose joins repeat a cell: each cell once per
    /// join row its joins resolve it to.
    pub(super) fn ask_expanded(
        &mut self,
        ask: &Ask,
        loaded: &ThawedCells<'s>,
        expander: &mut Expander<'s>,
    ) {
        let rollup = self.rollup;
        for covered in buckets(rollup, ask) {
            let cells = cells_of(covered, loaded);
            for grouping in &mut self.by {
                grouping.enter(covered.id.start);
            }
            for cell in 0..cells.len() {
                for row in expander.expand(rollup, cells.key(cell)) {
                    if all_hold(&mut self.filter, rollup, cells, cell, row) {
                        self.merge_row(cells, cell, row);
                    }
                }
            }
        }
    }

    /// Merges `cell` under one join row into its group.
    fn merge_row(&mut self, cells: &Cells, cell: usize, row: &JoinRow<'_>) {
        let rollup = self.rollup;
        let groups = &mut self.groups;
        groups.numbers.clear();
        for grouping in &self.by {
            groups.numbers.push(grouping.number_in(cells, cell, row));
        }
        let group = groups.insert(&self.by, rollup);
        for measure in &mut self.measures {
            measure.partials.grow(groups.len());
            if all_hold(&mut measure.filter, rollup, cells, cell, row) {
                let state = state_in(cells, measure.state);
                (measure.partials).merge_cell(group, cell, cells.rows(), state);
            }
        }
    }

    /// One row per group met that meets every HAVING check, sorted by its
    /// keys. A group's checked measures are finished first; a group they
    /// drop finishes no other measure, owns no keys and isn't sorted.
    // Benchmark: `bench/taxi.py`'s ask by hour and zone over six months,
    // 509,200 groups, of which HAVING keeps one in eight.
    pub(super) fn answer_rows(self, having: &[HavingCheck]) -> Vec<AnswerRow> {
        let CellLoop {
            by,
            groups,
            mut measures,
            ..
        } = self;
        let key_count = by.len();
        let group_count = groups.len();
        let mut checked = vec![false; measures.len()];
        for check in having {
            checked[check.position] = true;
        }
        for measure in &mut measures {
            measure.partials.grow(group_count);
        }
        let mut keys = groups.keys.into_iter();
        // Sized for every group, as without HAVING: growing by doubling
        // would hold two lists at once.
        let mut rows = Vec::with_capacity(groups.made);
        let mut values = vec![Value::Null; measures.len()];
        for group in 1..=groups.made {
            let group_keys = keys.by_ref().take(key_count);
            finish_where(&mut measures, &checked, Which::Checked, group, &mut values);
            if !meets(&values, having) {
                for _skipped in group_keys {}
                continue;
            }
            finish_where(
                &mut measures,
                &checked,
                Which::Unchecked,
                group,
                &mut values,
            );
            let mut owned = Vec::with_capacity(key_count);
            for key in group_keys {
                owned.push(Cow::into_owned(key));
            }
            rows.push(AnswerRow {
                keys: owned,
                values: values.clone(),
            });
        }
        rows.sort_by(by_keys);
        rows
    }
}

/// Which of a group's measures to finish: those a HAVING check reads, or
/// the rest.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Which {
    Checked,
    Unchecked,
}

/// Finishes `group`'s measures of the kind `which` into their place among
/// `values`.
fn finish_where(
    measures: &mut [Asked<'_>],
    checked: &[bool],
    which: Which,
    group: usize,
    values: &mut [Value],
) {
    for (position, measure) in measures.iter_mut().enumerate() {
        let kind = if checked[position] {
            Which::Checked
        } else {
            Which::Unchecked
        };
        if kind == which {
            values[position] = measure.partials.finish(group);
        }
    }
}

fn by_keys(left: &AnswerRow, right: &AnswerRow) -> Ordering {
    left.keys.cmp(&right.keys)
}

/// Enumerates how each cell's joins resolve, once per distinct tuple of
/// its join codes.
pub(super) struct Expander<'s> {
    plans: Vec<RefPlan<'s>>,
    /// The rollup-key parts the refs read, each once.
    parts: Vec<PartIndex>,
    /// The codes of `parts` of the cell at hand, kept to spare an
    /// allocation per cell.
    tuple: Vec<u32>,
    memo: FastMap<Box<[u32]>, Vec<JoinRow<'s>>>,
}

/// One ref, as the expander resolves it.
pub(super) struct RefPlan<'s> {
    pub(super) kind: RefKind,
    pub(super) projection: &'s Projection,
    /// Where each column of the ref's key comes from.
    pub(super) via: Vec<ViaFrom>,
}

impl RefPlan<'_> {
    /// Whether the ref keeps or drops a cell without repeating it: its key
    /// finds one row at most, or its join keeps no row, and its key comes
    /// from the cell alone.
    pub(super) fn counts_once(&self) -> bool {
        let repeats = match self.kind {
            RefKind::Left | RefKind::Inner => !self.projection.is_unique(),
            RefKind::Semi | RefKind::Anti => false,
        };
        if repeats {
            return false;
        }
        for via in &self.via {
            if let ViaFrom::Through { .. } = via {
                return false;
            }
        }
        true
    }
}

/// Where a ref's key column comes from: a cell's part, or an attribute
/// of a ref before it.
pub(super) enum ViaFrom {
    KeyPart(PartIndex),
    Through {
        /// Into [`Fact::refs`](crate::spec::Fact::refs).
        reference: usize,
        attribute: AttributeIndex,
    },
}

impl<'s> Expander<'s> {
    /// The expander for `plans`, one per ref; none when each counts a
    /// cell once.
    pub(super) fn new(plans: Vec<RefPlan<'s>>) -> Option<Expander<'s>> {
        let mut parts = Vec::new();
        let mut every_once = true;
        for plan in &plans {
            every_once &= plan.counts_once();
            for via in &plan.via {
                if let ViaFrom::KeyPart(part) = via
                    && !parts.contains(part)
                {
                    parts.push(*part);
                }
            }
        }
        if every_once {
            return None;
        }
        Some(Expander {
            plans,
            parts,
            tuple: Vec::new(),
            memo: FastMap::default(),
        })
    }

    fn expand(&mut self, rollup: &Rollup, key: &[u32]) -> &[JoinRow<'s>] {
        self.tuple.clear();
        for part in &self.parts {
            self.tuple.push(key[part.0]);
        }
        if !self.memo.contains_key(self.tuple.as_slice()) {
            let rows = self.resolve(rollup, key);
            self.memo.insert(self.tuple.as_slice().into(), rows);
        }
        &self.memo[self.tuple.as_slice()]
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
                    dim_key.push(self.via_value(rollup, key, row, via).clone());
                }
                keep(plan.kind, plan.projection.matches(&dim_key), row, &mut next);
            }
            rows = next;
        }
        rows
    }

    /// The value of one column of a ref's key, for the cell with `key`
    /// under the join row `row` of the refs before it.
    // The value borrows the rollup or a projection, so both share `'r`.
    fn via_value<'r>(
        &'r self,
        rollup: &'r Rollup,
        key: &[u32],
        row: &JoinRow<'_>,
        via: &ViaFrom,
    ) -> &'r Datum {
        match *via {
            ViaFrom::KeyPart(part) => rollup.value(part, key[part.0]),
            ViaFrom::Through {
                reference,
                attribute,
            } => match row[reference] {
                Some(found) => (self.plans[reference].projection)
                    .attribute_value(attribute, found[attribute.0]),
                None => &Datum::Null,
            },
        }
    }
}
