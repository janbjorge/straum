//! Answering an ask. The spec checks it and names its view; the store
//! picks the cells that answer it (bound or keyed), works out how each
//! key, filter and join reads a cell, runs the cell loop over the buckets
//! of the ask's range (an open end reaching to the buckets held), and
//! turns the groups met into answer rows.

use std::ops::Range;

use crate::aggregate::Partials;
use crate::ask::{Ask, AskError, BoundZone, Comparison};
use crate::hash::FastMap;
use crate::identifier::Identifier;
use crate::rollup::{Cover, CoveredBucket, Rollup};
use crate::spec::{Condition, Fact, Key, KeyPart, Measure, Ref, RefIndex, RefKind, Via};

use super::cell_loop::{Asked, CellLoop, Expander, RefPlan, Rows, ViaFrom};
use super::groups::{Grouping, Groups};
use super::lookup::{
    AttributeLookup, ConditionCheck, JoinCheck, JoinKeeps, Joined, KnownMemo, Lookup, Memo,
    UNKNOWN, Wanted,
};
use crate::value::{ColumnType, Threshold, Value};

use super::{
    Answer, AnswerColumn, CHECKED, Store, Window, WindowAnswer, bound_part_of, key_part_of,
    position_of,
};

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
        Ok(self.ask_window(ask, None)?.answer)
    }

    /// As [`Store::ask`], building only the rows `window` shows, every row
    /// without one, and saying where they lie among all of the answer's.
    pub fn ask_window(
        &self,
        ask: &Ask,
        window: Option<Window>,
    ) -> Result<WindowAnswer, Vec<AskError>> {
        let Reading { fact, read } = self.reading(ask)?;
        check_horizons(read.rollup(), ask, cover_of(fact, ask))?;
        self.check_types(fact, read, ask)?;
        let rollup = read.rollup();
        let mut asked = Vec::with_capacity(ask.measures.len());
        for name in &ask.measures {
            asked.push(fact.measure(name.as_str()).expect(CHECKED));
        }
        let mut expander = match read {
            Read::Bound(_) => None,
            Read::Keyed(_) => self.expander(fact),
        };
        let mut cell_loop = self.cell_loop(fact, read, ask, &asked, expander.is_some());
        let columns = answer_columns(ask, rollup, &cell_loop.by, &asked);
        match read {
            // The bound cells hold only what the joins keep.
            Read::Bound(_) => cell_loop.ask_once(&mut Vec::new(), ask),
            Read::Keyed(_) => {
                if let Some(expander) = &mut expander {
                    cell_loop.ask_expanded(ask, expander);
                } else {
                    let cells = cells_in(rollup, ask, cell_loop.cover);
                    cell_loop.ask_once(&mut self.join_checks(fact, rollup, cells), ask);
                }
            }
        }
        let checks = having_checks(ask, &columns);
        let Rows { rows, cut } = cell_loop.answer_rows(&checks, window);
        Ok(WindowAnswer {
            answer: Answer { columns, rows },
            cut,
        })
    }

    /// Cells the ask reads.
    pub fn cells_read(&self, ask: &Ask) -> Result<usize, Vec<AskError>> {
        let Reading { fact, read } = self.reading(ask)?;
        let cover = cover_of(fact, ask);
        check_horizons(read.rollup(), ask, cover)?;
        Ok(cells_in(read.rollup(), ask, cover))
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
    /// filters and measures. `expanded` tells that the joins repeat a cell.
    fn cell_loop<'s>(
        &'s self,
        fact: &Fact,
        read: Read<'s>,
        ask: &Ask,
        asked: &[&Measure],
        expanded: bool,
    ) -> CellLoop<'s> {
        let rollup = read.rollup();
        let cover = cover_of(fact, ask);
        let buckets = buckets(rollup, ask, cover).count();
        let cells = cells_in(rollup, ask, cover);
        let mut by = Vec::with_capacity(ask.keys.len());
        for key in &ask.keys {
            by.push(match fact.time_width(key) {
                Some(width) => Grouping::time(width, buckets),
                None => {
                    Grouping::lookup(self.lookup(fact, read, key.name(), cells, expanded), rollup)
                }
            });
        }
        let groups = Groups::new(&by);
        let mut filter = self.condition_checks(fact, read, &fact.filter, cells, expanded);
        filter.extend(self.condition_checks(fact, read, &ask.filter, cells, expanded));
        let mut measures = Vec::with_capacity(asked.len());
        for (position, measure) in asked.iter().enumerate() {
            let mut next = Asked {
                partials: Partials::new(&measure.aggregate),
                state: rollup.state_of(&measure.aggregate),
                filter: self.condition_checks(fact, read, &measure.filter, cells, expanded),
                shares: None,
            };
            next.share_tally(&mut measures, asked, position);
            measures.push(next);
        }
        CellLoop {
            rollup,
            cover,
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
        if ask.bound_zone == BoundZone::Offset
            && read.rollup().time_type() == (ColumnType::Time { zoned: false })
        {
            wrong.push(AskError::OffsetOnWallClock);
        }
        for condition in &ask.filter {
            let column = self.condition_type(fact, read, &condition.key);
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

    /// The type of the key a condition names, read without the memo a
    /// [`Lookup`] of a dim value keeps: one entry per code of the via
    /// column, which may be millions.
    fn condition_type(&self, fact: &Fact, read: Read<'_>, name: &Identifier) -> ColumnType {
        let rollup = match read {
            Read::Bound(rollup) => return rollup.key_type(bound_part_of(fact, name)),
            Read::Keyed(rollup) => rollup,
        };
        self.keyed_key_type(fact, rollup, fact.resolve(name).expect(CHECKED))
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
            Via::Attribute { through, column } => ViaFrom::Through {
                reference: *through,
                attribute: self.attribute_index(fact.reference(*through), column),
            },
        }
    }

    /// The joins that keep or drop a whole cell, for a view that counts
    /// each cell once: every ref but the left ones, which keep them all.
    /// The ask reads `cells` cells.
    fn join_checks<'s>(&'s self, fact: &Fact, rollup: &Rollup, cells: usize) -> Vec<JoinCheck<'s>> {
        let mut checks = Vec::new();
        for reference in &fact.refs {
            if reference.kind == RefKind::Left {
                continue;
            }
            checks.push(JoinCheck {
                joined: self.joined(fact, reference, rollup, cells, false),
                keeps: JoinKeeps::of(reference.kind),
            });
        }
        checks
    }

    /// How a cell's key finds a ref's dim rows, through the rollup-key
    /// parts of its fact-side via columns, for an ask of `cells` cells.
    /// When `expanded`, the cell loop reads the join rows and not the memo.
    fn joined<'s>(
        &'s self,
        fact: &Fact,
        reference: &Ref,
        rollup: &Rollup,
        cells: usize,
        expanded: bool,
    ) -> Joined<'s> {
        let mut via = Vec::with_capacity(reference.via.len());
        for column in &reference.via {
            if let Via::Fact(column) = column {
                via.push(key_part_of(fact, KeyPart::Column(column)));
            }
        }
        // A memo of every code costs per id ever seen: only an ask that
        // reads as many cells as there are codes pays for one.
        let memo = match via[..] {
            [part] if !expanded && rollup.codes(part) <= cells => {
                Memo::Dense(vec![UNKNOWN; rollup.codes(part)])
            }
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
    /// dim's projection reached through a ref, for an ask of `cells`
    /// cells, `expanded` as [`Store::joined`] says.
    fn lookup<'s>(
        &'s self,
        fact: &Fact,
        read: Read<'s>,
        name: &Identifier,
        cells: usize,
        expanded: bool,
    ) -> Lookup<'s> {
        let rollup = match read {
            Read::Bound(_) => return Lookup::KeyPart(bound_part_of(fact, name)),
            Read::Keyed(rollup) => rollup,
        };
        match fact.resolve(name).expect(CHECKED) {
            Key::ByKey(by) => Lookup::KeyPart(key_part_of(fact, by.key_part())),
            Key::Attribute(reference, attribute) => Lookup::Attribute(AttributeLookup {
                reference: ref_position(fact, reference),
                attribute: self.attribute_index(reference, &attribute.column),
                joined: self.joined(fact, reference, rollup, cells, expanded),
            }),
        }
    }

    fn condition_checks<'s>(
        &'s self,
        fact: &Fact,
        read: Read<'s>,
        filter: &[Condition],
        cells: usize,
        expanded: bool,
    ) -> Vec<ConditionCheck<'s>> {
        // Conditions on one key fold into one check, of the values they
        // share: each check keeps a memo of its key's codes, and
        // an ask may repeat a condition a thousand times.
        let mut checks: Vec<ConditionCheck<'s>> = Vec::new();
        let mut keys: Vec<&Identifier> = Vec::new();
        for condition in filter {
            let wanted = Wanted::new(&condition.values);
            if let Some(position) = position_of_key(&keys, &condition.key) {
                checks[position].wanted.keep_shared(&wanted);
                continue;
            }
            keys.push(&condition.key);
            let lookup = self.lookup(fact, read, &condition.key, cells, expanded);
            checks.push(ConditionCheck {
                memo: KnownMemo::new(lookup.bound(read.rollup()), cells),
                lookup,
                wanted,
            });
        }
        checks
    }
}

/// Where `key` is among `keys`.
fn position_of_key(keys: &[&Identifier], key: &Identifier) -> Option<usize> {
    for (position, candidate) in keys.iter().enumerate() {
        if *candidate == key {
            return Some(position);
        }
    }
    None
}

/// Where `reference` is among the fact's refs.
fn ref_position(fact: &Fact, reference: &Ref) -> RefIndex {
    for (position, other) in fact.refs.iter().enumerate() {
        if other.alias == reference.alias {
            return RefIndex(position);
        }
    }
    panic!("{CHECKED}")
}

/// The buckets an ask reads: the widest that fit, none wider than its row
/// width when it groups by the bucket.
// An iterator, not a `Vec`: an ask allocates per group, never per bucket
// (`an_ask_allocates_per_group_not_per_cell` in tests/cost/main.rs).
pub(super) fn buckets<'r>(
    rollup: &'r Rollup,
    ask: &Ask,
    cover: Cover,
) -> impl Iterator<Item = CoveredBucket<'r>> {
    let range = read_range(rollup, ask);
    rollup.cover(range.start, range.end, cover)
}

/// Which buckets an ask reads: none wider than its row width when it
/// groups by the view's time.
fn cover_of(fact: &Fact, ask: &Ask) -> Cover {
    for key in &ask.keys {
        if let Some(width) = fact.time_width(key) {
            return Cover::NoWiderThan(width);
        }
    }
    Cover::WidestFit
}

/// The cells an ask reads.
fn cells_in(rollup: &Rollup, ask: &Ask, cover: Cover) -> usize {
    let mut cells = 0;
    for covered in buckets(rollup, ask, cover) {
        cells += covered.cells.len();
    }
    cells
}

/// Refuses an ask that would read buckets a width kept for a time has
/// dropped, naming each such width once.
fn check_horizons(rollup: &Rollup, ask: &Ask, cover: Cover) -> Result<(), Vec<AskError>> {
    let range = read_range(rollup, ask);
    let past = rollup.past_horizons(range.start, range.end, cover);
    if past.is_empty() { Ok(()) } else { Err(past) }
}

/// The range an ask reads: its bounds, an open end at the edge of the
/// buckets held, as [`Rollup::open_range`] sets it. With no bucket held
/// there is nothing to read, so an open end stands at 0.
fn read_range(rollup: &Rollup, ask: &Ask) -> Range<i64> {
    let held = rollup.open_range().unwrap_or(0..0);
    let start = ask.from.unwrap_or(held.start);
    let end = ask.to.unwrap_or(held.end);
    start..end
}

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

/// Whether group `group`, with its measure `values`, meets every check.
/// The values a check doesn't read may not be finished yet. A variance
/// or deviation compares exactly from its `measures`' partials, not as
/// its value rounds it.
pub(super) fn meets(
    measures: &[Asked<'_>],
    group: usize,
    values: &[Value],
    checks: &[HavingCheck],
) -> bool {
    for check in checks {
        let partials = &measures[check.position].partials;
        let order = match partials.spread() {
            Some(spread) => spread.order_against(partials.moments(group), check.threshold),
            None => values[check.position].order_against(check.threshold),
        };
        let Some(order) = order else {
            return false;
        };
        if !check.comparison.holds(order) {
            return false;
        }
    }
    true
}

/// The `by` keys, then the measures, as the ask names them.
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
            is_time: matches!(grouping, Grouping::Time { .. }),
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
