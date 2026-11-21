//! How the cell loop reads a cell: where a key's values come from (a part
//! of the cell key, or a dim column reached through a join), whether a
//! cell passes a condition, and whether an inner, semi or anti join keeps
//! it. Each is worked out once per code or per tuple of join codes and
//! remembered, so a cell costs a lookup in a table, not in a dim.

use crate::date::parse_date;
use crate::hash::{FastMap, FastSet};
use crate::projection::{AttributeIndex, NULL, Projection};
use crate::rollup::{Cells, PartIndex, Rollup};
use crate::spec::{Literal, RefIndex, RefKind};
use crate::uuid::parse_uuid;
use crate::value::{ColumnType, Datum};

use super::bound::JoinRow;

/// A via code not looked up yet.
pub(super) const UNKNOWN: u32 = u32::MAX;

/// Where a key's values come from.
// The `'s` in this file borrows the store's projections for the length
// of an ask: copying a dim to answer one ask would cost more than the ask.
pub(super) enum Lookup<'s> {
    /// A part of the cell key: a by key, or a dim column in bound cells.
    KeyPart(PartIndex),
    /// A dim column, read through a join.
    Attribute(AttributeLookup<'s>),
}

/// A dim value read through a ref.
pub(super) struct AttributeLookup<'s> {
    pub(super) reference: RefIndex,
    pub(super) attribute: AttributeIndex,
    pub(super) joined: Joined<'s>,
}

/// A ref's dim rows as a cell's key finds them, in a view that counts
/// each cell once: what is read of them is worked out once per tuple of
/// the cell's via codes.
pub(super) struct Joined<'s> {
    pub(super) projection: &'s Projection,
    /// The rollup-key parts of the ref's fact-side via columns, in the
    /// dim's key order.
    pub(super) via: Vec<PartIndex>,
    /// The key of one lookup, kept to spare an allocation per lookup.
    pub(super) key: Vec<Datum>,
    /// The via codes of one lookup, kept likewise.
    pub(super) tuple: Vec<u32>,
    pub(super) memo: Memo,
}

/// What was read per via code tuple: by the one code when the ref joins
/// on one column, else by the tuple.
pub(super) enum Memo {
    Dense(Vec<u32>),
    Sparse(FastMap<Box<[u32]>, u32>),
}

/// Which cells a join keeps: those whose key finds a dim row (an inner
/// or semi join), or those whose key finds none (an anti join).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JoinKeeps {
    Found,
    NotFound,
}

impl JoinKeeps {
    /// What a join of `kind` keeps.
    pub(super) fn of(kind: RefKind) -> JoinKeeps {
        match kind {
            RefKind::Anti => JoinKeeps::NotFound,
            RefKind::Left | RefKind::Inner | RefKind::Semi => JoinKeeps::Found,
        }
    }
}

/// What [`Joined::read`] makes of the dim rows under a cell's key.
#[derive(Debug, Clone, Copy)]
enum JoinRead {
    /// 1 when the join keeps the cell, 0 when it drops it.
    Kept(JoinKeeps),
    /// The code of an attribute of the first row, or [`NULL`].
    Attribute(AttributeIndex),
}

fn read_rows(read: JoinRead, rows: &[Box<[u32]>]) -> u32 {
    match read {
        JoinRead::Kept(JoinKeeps::Found) => u32::from(!rows.is_empty()),
        JoinRead::Kept(JoinKeeps::NotFound) => u32::from(rows.is_empty()),
        JoinRead::Attribute(attribute) => match rows.first() {
            Some(row) => row[attribute.0],
            None => NULL,
        },
    }
}

impl Joined<'_> {
    /// What `read` makes of the rows under the key of the cell with
    /// `key`, below [`UNKNOWN`].
    // Benchmark: see `Lookup::code`.
    #[inline]
    fn read(&mut self, rollup: &Rollup, key: &[u32], read: JoinRead) -> u32 {
        let known = match &mut self.memo {
            Memo::Dense(codes) => &mut codes[key[self.via[0].0] as usize],
            Memo::Sparse(map) => {
                self.tuple.clear();
                for part in &self.via {
                    self.tuple.push(key[part.0]);
                }
                // Looked up before inserting, so only a new tuple allocates.
                if !map.contains_key(self.tuple.as_slice()) {
                    map.insert(self.tuple.as_slice().into(), UNKNOWN);
                }
                map.get_mut(self.tuple.as_slice()).expect("inserted above")
            }
        };
        if *known == UNKNOWN {
            self.key.clear();
            for &part in &self.via {
                self.key.push(rollup.value(part, key[part.0]).clone());
            }
            *known = read_rows(read, self.projection.matches(&self.key));
        }
        *known
    }
}

/// A join that keeps or drops a whole cell, in a view that counts each
/// cell once.
pub(super) struct JoinCheck<'s> {
    pub(super) joined: Joined<'s>,
    pub(super) keeps: JoinKeeps,
}

impl JoinCheck<'_> {
    /// Zeroes the group of each cell the join drops. Selects, not
    /// branches, as [`ConditionCheck::pass`] does.
    pub(super) fn pass(&mut self, rollup: &Rollup, cells: &Cells, groups: &mut [u32]) {
        let read = JoinRead::Kept(self.keeps);
        for (cell, group) in groups.iter_mut().enumerate() {
            let kept = self.joined.read(rollup, cells.key(cell), read);
            *group = if kept == 1 { *group } else { 0 };
        }
    }
}

impl AttributeLookup<'_> {
    /// The attribute's code for the cell with `key`: the one row under the
    /// key, or [`NULL`].
    // Benchmark: see `Lookup::code`.
    #[inline]
    fn code(&mut self, rollup: &Rollup, key: &[u32]) -> u32 {
        let read = JoinRead::Attribute(self.attribute);
        self.joined.read(rollup, key, read)
    }

    /// The attribute's code under one join row.
    fn code_in(&self, row: &JoinRow<'_>) -> u32 {
        match row[self.reference.0] {
            Some(found) => found[self.attribute.0],
            None => NULL,
        }
    }
}

impl<'s> Lookup<'s> {
    /// The code of the key's value for `cell`, in a view that counts each
    /// cell once.
    // Benchmark: tests/cost/main.rs an_ask_stays_within_its_time_per_cell, by
    // borough, least of 6 runs: 10.0 ns a cell without `#[inline]` on
    // this, `AttributeLookup::code`, `Joined::read` and
    // `Grouping::offsets`, 9.0 ns with it, as before the store was split
    // into files. Each runs once per cell.
    #[inline]
    pub(super) fn code(&mut self, rollup: &Rollup, cells: &Cells, cell: usize) -> u32 {
        match self {
            Lookup::KeyPart(part) => cells.code(cell, *part),
            Lookup::Attribute(lookup) => lookup.code(rollup, cells.key(cell)),
        }
    }

    /// The code of the key's value for `cell` under one join row.
    pub(super) fn code_in(&self, cells: &Cells, cell: usize, row: &JoinRow<'_>) -> u32 {
        match self {
            Lookup::KeyPart(part) => cells.code(cell, *part),
            Lookup::Attribute(lookup) => lookup.code_in(row),
        }
    }

    /// Borrowed from the rollup or the projection, which outlive the ask.
    // `'r` is the shorter of the two borrows, the rollup's and the
    // projection's, so the value may come from either.
    pub(super) fn value<'r>(&self, rollup: &'r Rollup, code: u32) -> &'r Datum
    where
        's: 'r,
    {
        match self {
            Lookup::KeyPart(part) => rollup.value(*part, code),
            Lookup::Attribute(lookup) => {
                (lookup.joined.projection).attribute_value(lookup.attribute, code)
            }
        }
    }

    /// Codes the key's values have.
    pub(super) fn bound(&self, rollup: &Rollup) -> usize {
        match self {
            Lookup::KeyPart(part) => rollup.codes(*part),
            Lookup::Attribute(lookup) => lookup.joined.projection.attribute_codes(lookup.attribute),
        }
    }

    /// The type of the key's values, as the load set it.
    pub(super) fn column_type(&self, rollup: &Rollup) -> ColumnType {
        match self {
            Lookup::KeyPart(part) => rollup.key_type(*part),
            Lookup::Attribute(lookup) => lookup.joined.projection.attribute_type(lookup.attribute),
        }
    }
}

/// A condition of a filter, worked out once per code of its key as the
/// ask meets the code.
pub(super) struct ConditionCheck<'s> {
    pub(super) lookup: Lookup<'s>,
    /// The values the key may have; copied from the condition, so the
    /// check borrows nothing but the store.
    pub(super) wanted: Wanted,
    pub(super) memo: KnownMemo,
}

/// Whether the condition holds, per code of its key: in a list of every
/// code when the ask reads as many cells as there are codes, else in a
/// map of the codes met.
pub(super) enum KnownMemo {
    Dense(Vec<Known>),
    Sparse(FastMap<u32, Known>),
}

impl KnownMemo {
    /// A memo of `codes` codes for an ask of `cells` cells.
    pub(super) fn new(codes: usize, cells: usize) -> KnownMemo {
        // A list of every code costs per value ever seen: only an ask
        // that reads as many cells as there are codes pays for one.
        if codes <= cells {
            KnownMemo::Dense(vec![Known::Unknown; codes])
        } else {
            KnownMemo::Sparse(FastMap::default())
        }
    }

    fn get(&self, code: u32) -> Known {
        match self {
            KnownMemo::Dense(codes) => codes[code as usize],
            KnownMemo::Sparse(map) => match map.get(&code) {
                Some(known) => *known,
                None => Known::Unknown,
            },
        }
    }

    fn set(&mut self, code: u32, known: Known) {
        match self {
            KnownMemo::Dense(codes) => codes[code as usize] = known,
            KnownMemo::Sparse(map) => {
                map.insert(code, known);
            }
        }
    }
}

/// Whether a condition holds for a code, once worked out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Known {
    Unknown,
    Holds,
    Fails,
}

impl ConditionCheck<'_> {
    /// Zeroes the group of each cell that fails. Whether a cell passes is
    /// as likely as not, so the loop selects rather than branches; only a
    /// code met first branches.
    pub(super) fn pass(&mut self, rollup: &Rollup, cells: &Cells, groups: &mut [u32]) {
        for (cell, group) in groups.iter_mut().enumerate() {
            let code = self.lookup.code(rollup, cells, cell);
            let mut known = self.memo.get(code);
            if known == Known::Unknown && *group != 0 {
                known = self.resolve(rollup, code);
            }
            *group = if known == Known::Holds { *group } else { 0 };
        }
    }

    /// Whether `cell` passes under one join row.
    pub(super) fn holds(
        &mut self,
        rollup: &Rollup,
        cells: &Cells,
        cell: usize,
        row: &JoinRow<'_>,
    ) -> bool {
        let code = self.lookup.code_in(cells, cell, row);
        let mut known = self.memo.get(code);
        if known == Known::Unknown {
            known = self.resolve(rollup, code);
        }
        known == Known::Holds
    }

    fn resolve(&mut self, rollup: &Rollup, code: u32) -> Known {
        let value = self.lookup.value(rollup, code);
        let known = if self.wanted.holds(value) {
            Known::Holds
        } else {
            Known::Fails
        };
        self.memo.set(code, known);
        known
    }
}

/// A condition's values, by kind, each found in one hash: matching a key
/// value against a list of 340,000 one by one took 45 s under the read
/// lock. As SQL's `=`: a value matches a literal of its own kind, and a
/// null matches nothing.
/// A text literal is wanted as text, and as the date or uuid it spells,
/// if any: the column's type, checked before, says which a value is.
#[derive(Default)]
pub(super) struct Wanted {
    ints: FastSet<i64>,
    texts: FastSet<String>,
    dates: FastSet<i32>,
    uuids: FastSet<[u8; 16]>,
    bools: [bool; 2],
}

impl Wanted {
    /// The values `values` lists, each once.
    pub(super) fn new(values: &[Literal]) -> Wanted {
        let mut wanted = Wanted::default();
        for value in values {
            match value {
                Literal::Int(number) => {
                    wanted.ints.insert(*number);
                }
                Literal::Text(text) => {
                    if let Some(days) = parse_date(text) {
                        wanted.dates.insert(days);
                    }
                    if let Some(uuid) = parse_uuid(text) {
                        wanted.uuids.insert(uuid);
                    }
                    wanted.texts.insert(text.clone());
                }
                Literal::Bool(flag) => wanted.bools[usize::from(*flag)] = true,
                // No key Straum filters on holds one; the ask was refused.
                Literal::Decimal { .. } => {}
            }
        }
        wanted
    }

    /// Keeps only the values `other` wants too: two conditions on one key
    /// both hold for those alone.
    pub(super) fn keep_shared(&mut self, other: &Wanted) {
        self.ints.retain(|number| other.ints.contains(number));
        self.texts.retain(|text| other.texts.contains(text));
        self.dates.retain(|days| other.dates.contains(days));
        self.uuids.retain(|uuid| other.uuids.contains(uuid));
        for (kept, also) in self.bools.iter_mut().zip(other.bools) {
            *kept &= also;
        }
    }

    /// Whether `value` is one of the values wanted.
    fn holds(&self, value: &Datum) -> bool {
        match value {
            Datum::Null => false,
            Datum::Bool(flag) => self.bools[usize::from(*flag)],
            Datum::Int(number) => self.ints.contains(number),
            Datum::Text(text) => self.texts.contains(&**text),
            Datum::Date(days) => self.dates.contains(days),
            Datum::Uuid(uuid) => self.uuids.contains(uuid),
        }
    }
}

/// Whether `cell` passes every check under one join row.
pub(super) fn all_hold(
    checks: &mut [ConditionCheck<'_>],
    rollup: &Rollup,
    cells: &Cells,
    cell: usize,
    row: &JoinRow<'_>,
) -> bool {
    for check in checks {
        if !check.holds(rollup, cells, cell, row) {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    /// Each value matches a literal of its own kind, a null nothing, and a
    /// list of any length is one hash away.
    #[test]
    fn a_value_is_wanted_as_sql_equals_says() {
        let mut values = Vec::new();
        for number in 0..340_000 {
            values.push(Literal::Int(number * 2));
        }
        values.extend([
            Literal::Text("cab".into()),
            Literal::Text("cab".into()),
            Literal::Bool(false),
            Literal::Decimal { units: 7, scale: 0 },
        ]);
        let wanted = Wanted::new(&values);
        for (value, holds) in [
            (Datum::Int(0), true),
            (Datum::Int(679_998), true),
            (Datum::Int(7), false),
            (Datum::Int(680_000), false),
            (Datum::Text(Arc::from("cab")), true),
            (Datum::Text(Arc::from("Cab")), false),
            (Datum::Bool(false), true),
            (Datum::Bool(true), false),
            (Datum::Null, false),
        ] {
            assert_eq!(wanted.holds(&value), holds, "{value:?}");
        }
        let none = Wanted::new(&[]);
        assert!(!none.holds(&Datum::Int(0)) && !none.holds(&Datum::Bool(false)));
    }

    /// A text literal finds the date or uuid it spells, and only that.
    #[test]
    fn a_date_or_uuid_is_wanted_by_its_text() {
        let uuid = "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11";
        let mut wanted = Wanted::new(&[
            Literal::Text("2000-01-02".into()),
            Literal::Text(uuid.into()),
            Literal::Text("infinity".into()),
        ]);
        let mut bytes = [0; 16];
        bytes[0] = 0xa0;
        let found = parse_uuid(uuid).unwrap();
        for (value, holds) in [
            (Datum::Date(1), true),
            (Datum::Date(i32::MAX), true),
            (Datum::Date(0), false),
            (Datum::Date(i32::MIN), false),
            (Datum::Uuid(found), true),
            (Datum::Uuid(bytes), false),
            (Datum::Text(Arc::from("2000-01-02")), true),
        ] {
            assert_eq!(wanted.holds(&value), holds, "{value:?}");
        }
        wanted.keep_shared(&Wanted::new(&[Literal::Text("2000-01-02".into())]));
        assert!(wanted.holds(&Datum::Date(1)));
        assert!(!wanted.holds(&Datum::Date(i32::MAX)));
        assert!(!wanted.holds(&Datum::Uuid(found)));
    }
}
