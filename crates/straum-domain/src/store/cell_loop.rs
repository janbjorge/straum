//! The cell loop: the pass an ask makes over the cells it reads, bucket by
//! bucket, dropping the cells its filter and joins fail and merging the
//! rest into their groups. A view whose joins find one dim row a key at
//! most walks each cell once; one whose joins can repeat a cell walks it
//! once per join row, as PostgreSQL repeats the fact row.

use crate::aggregate::Partials;
use crate::ask::Ask;
use crate::hash::FastMap;
use crate::projection::{AttributeIndex, Projection};
use crate::rollup::{Cells, Cover, InputIndex, PartIndex, Rollup, State};
use crate::spec::{Measure, RefKind};
use crate::value::{Datum, Value};

use super::ask::{HavingCheck, buckets, meets};
use super::bound::{JoinRow, keep};
use super::groups::{Grouping, Groups};
use super::lookup::{ConditionCheck, JoinCheck, all_hold};
use super::{AnswerRow, Cut, Window};

/// What the cell loop fills in: per by key its grouping, the groups met,
/// the ask's and the view's filter, and per measure its partial answers.
// The `'s` in this file borrows the store for the length of the ask.
pub(super) struct CellLoop<'s> {
    pub(super) rollup: &'s Rollup,
    /// Which buckets the ask reads.
    pub(super) cover: Cover,
    pub(super) by: Vec<Grouping<'s>>,
    pub(super) groups: Groups,
    pub(super) filter: Vec<ConditionCheck<'s>>,
    pub(super) measures: Vec<Asked<'s>>,
}

/// A measure an ask names, as the cell loop merges it.
pub(super) struct Asked<'s> {
    pub(super) partials: Partials<'s>,
    /// The cell state the measure reads; none for a count of rows.
    pub(super) state: Option<InputIndex>,
    /// The measure's own `FILTER`.
    pub(super) filter: Vec<ConditionCheck<'s>>,
    /// An earlier measure whose distinct values this one reads, merging
    /// none of its own: `count`, `sum` and `avg` of one column's distinct
    /// values under one `FILTER` tally them once.
    pub(super) shares: Option<AskedIndex>,
}

/// A measure's place among those an ask names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct AskedIndex(pub(super) usize);

impl<'s> Asked<'s> {
    /// Points this measure, `asked[position]`, at the first of `measures`
    /// that tallies the same distinct values: the same column's state
    /// under the same `FILTER`.
    pub(super) fn share_tally(
        &mut self,
        measures: &mut [Asked<'s>],
        asked: &[&Measure],
        position: usize,
    ) {
        let Some(finish) = self.partials.distinct_finish() else {
            return;
        };
        for (earlier, measure) in measures.iter_mut().enumerate() {
            let alike = measure.shares.is_none()
                && measure.state == self.state
                && measure.partials.distinct_finish().is_some()
                && asked[earlier].filter == asked[position].filter;
            if alike {
                measure.partials.share(finish);
                self.shares = Some(AskedIndex(earlier));
                return;
            }
        }
    }
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
    pub(super) fn ask_once(&mut self, joins: &mut [JoinCheck<'s>], ask: &Ask) {
        let rollup = self.rollup;
        // Per cell of a bucket: 1 while it may pass, then its group; 0 for
        // cells passed over. Again per measure with a filter of its own.
        let mut cell_groups = Vec::new();
        let mut measure_groups = Vec::new();
        for covered in buckets(rollup, ask, self.cover) {
            let cells = covered.cells;
            self.groups.enter(&mut self.by, covered.id.start);
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
                if measure.shares.is_some() {
                    continue;
                }
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
    pub(super) fn ask_expanded(&mut self, ask: &Ask, expander: &mut Expander<'s>) {
        let rollup = self.rollup;
        for covered in buckets(rollup, ask, self.cover) {
            let cells = covered.cells;
            self.groups.enter(&mut self.by, covered.id.start);
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
    fn merge_row(&mut self, cells: &'s Cells, cell: usize, row: &JoinRow<'_>) {
        let rollup = self.rollup;
        let groups = &mut self.groups;
        groups.numbers.clear();
        for grouping in &self.by {
            groups.numbers.push(grouping.number_in(cells, cell, row));
        }
        let group = groups.insert();
        for measure in &mut self.measures {
            if measure.shares.is_some() {
                continue;
            }
            measure.partials.grow(groups.len());
            if all_hold(&mut measure.filter, rollup, cells, cell, row) {
                let state = state_in(cells, measure.state);
                (measure.partials).merge_cell(group, cell, cells.rows(), state);
            }
        }
    }

    /// One row per group met that meets every HAVING check, in the order
    /// of its keys; with a window, only the rows it shows. A group's
    /// checked measures are finished first; a group they drop finishes no
    /// other measure and reads no keys.
    // Benchmark: `bench/taxi.py`'s ask by hour and zone over six months,
    // 509,200 groups, of which HAVING keeps one in eight.
    pub(super) fn answer_rows(self, having: &[HavingCheck], window: Option<Window>) -> Rows {
        let CellLoop {
            rollup,
            by,
            groups,
            mut measures,
            ..
        } = self;
        let group_count = groups.len();
        let mut checked = vec![false; measures.len()];
        for check in having {
            checked[check.position] = true;
        }
        for measure in &mut measures {
            measure.partials.grow(group_count);
        }
        let mut values = vec![Value::Null; measures.len()];
        let mut kept = groups.in_key_order(&by, rollup);
        if !having.is_empty() {
            let mut meeting = Vec::new();
            for group in kept {
                let place = group as usize;
                finish_where(&mut measures, &checked, Which::Checked, place, &mut values);
                if meets(&values, having) {
                    meeting.push(group);
                }
            }
            kept = meeting;
        }
        // Only the rows the window shows: a view of millions of cells
        // asked for a few rows builds those few.
        let cut = Cut::of(window, kept.len());
        let mut rows = Vec::with_capacity(cut.end() - cut.first);
        for &group in &kept[cut.first..cut.end()] {
            let place = group as usize;
            for which in [Which::Checked, Which::Unchecked] {
                finish_where(&mut measures, &checked, which, place, &mut values);
            }
            rows.push(AnswerRow {
                keys: groups.key_values(&by, rollup, group),
                values: values.clone(),
            });
        }
        Rows { rows, cut }
    }
}

/// The rows of an answer the cell loop built, and where they lie among
/// all of its rows.
pub(super) struct Rows {
    pub(super) rows: Vec<AnswerRow>,
    pub(super) cut: Cut,
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
    for position in 0..measures.len() {
        let kind = if checked[position] {
            Which::Checked
        } else {
            Which::Unchecked
        };
        if kind != which {
            continue;
        }
        values[position] = match measures[position].shares {
            Some(AskedIndex(source)) => {
                let finish = measures[position].partials.distinct_finish();
                let finish = finish.expect("only a distinct measure shares");
                measures[source].partials.finish_as(group, finish)
            }
            None => measures[position].partials.finish(group),
        };
    }
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
            } => self.plans[reference]
                .projection
                .value_in(attribute, row[reference]),
        }
    }
}
