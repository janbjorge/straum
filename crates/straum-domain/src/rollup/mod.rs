//! A rollup: every cell of one fact, at each bucket width the view keeps.
//! A row comes in, its rollup-key values are coded to small integers, and
//! it adds to the cell of those codes in the bucket its time falls in, at
//! every width; a removed row takes back out what it added. An ask reads
//! the cells of the buckets that cover its range. The store keeps one
//! rollup per fact, and a second, bound one for a fact whose joins allow.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::{mem, panic, thread};

use crate::aggregate::{Aggregate, Reads};
use crate::codes::{Codes, worth_recoding};
use crate::identifier::Identifier;
use crate::spec::{Fact, KeyPart, Retention};
use crate::value::{ColumnType, Datum};
use crate::width::Width;

mod cells;
mod distinct;
mod level;
mod row_error;
mod state;
#[cfg(test)]
mod tests;

pub(crate) use cells::Cells;
pub(crate) use distinct::DistinctCells;
pub use level::KeptFrom;
use level::{CodedRows, Horizon, Level, MoveCell, Shift};
pub use row_error::RowError;
pub(crate) use row_error::{arity, split_rows};
pub(crate) use state::{State, StateKind};

/// The time column's place in a row: always first.
const TIME: usize = 0;

/// The level of the `date_bin` width: `Rollup::levels` holds it first,
/// since every wider width is made of its buckets.
const DATE_BIN: LevelIndex = LevelIndex(0);

const COLUMNS: &str = "Fact::columns lists every measure input";

/// A position in a row of the rollup's table, as [`Fact::columns`] lays
/// it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ColumnIndex(pub(crate) usize);

/// A part of the rollup key: a by key, a join column or a dim column,
/// the bucket aside.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct PartIndex(pub(crate) usize);

/// A cell state's place among the rollup's inputs, one per input column
/// and form of state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InputIndex(pub(crate) usize);

/// A level's place in [`Rollup`]'s levels: the `date_bin` width first,
/// then each wider one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LevelIndex(pub(crate) usize);

/// One bucket of one level: the level and the bucket's start, `None`
/// for the null-time bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LevelBucket {
    pub(crate) level: LevelIndex,
    pub(crate) start: Option<i64>,
}

/// One bucket of a rollup: its width and its start, both in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct BucketId {
    pub(crate) width: i64,
    pub(crate) start: i64,
}

/// A bucket an ask reads.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CoveredBucket<'r> {
    pub(crate) id: BucketId,
    // Borrowed: a bucket's cells are the bulk of the store.
    pub(crate) cells: &'r Cells,
}

/// Which buckets [`Rollup::cover`] picks for a range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cover {
    /// The widest buckets that fit, narrower ones at the ends.
    WidestFit,
    /// The widest buckets that fit, none wider than a kept width, so each
    /// lies inside one bucket of it: an ask grouped by the bucket at that
    /// width needs no more.
    NoWiderThan(Width),
}

/// Whether rows go into the cells or come out of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Direction {
    Add,
    Remove,
}

/// One cell per rollup key, holding what the fact's measures need. Cells
/// are kept per bucket, so an ask reads only the buckets in its range, and
/// once more per wider bucket width the fact declares, so a long range
/// reads few cells.
#[derive(Debug)]
pub(crate) struct Rollup {
    /// The columns a row is read from, as [`Fact::columns`].
    columns: Vec<Identifier>,
    /// What each rollup-key part codes of a row, in [`Fact::rollup_key`]
    /// order without the bucket.
    key_columns: Vec<Coded>,
    inputs: Vec<Input>,
    /// What each measure reads of its column, which a row's value must be.
    column_reads: Vec<ColumnRead>,
    /// One per rollup-key column: its values under small codes, so a cell
    /// key is a few integers.
    codes: Vec<Codes>,
    /// The `date_bin` width first, then the wider ones.
    levels: Vec<Level>,
    /// Cells at each level, in `levels` order.
    level_cells: Vec<usize>,
    /// The codes of the row [`Rollup::add`] is adding, kept to spare it an
    /// allocation per row.
    key: Vec<u32>,
    types: Vec<ColumnType>,
}

/// What a rollup-key part codes of a row.
#[derive(Debug, Clone, Copy)]
enum Coded {
    /// The column's value.
    Value(ColumnIndex),
    /// Whether the column is null, or not null when `negated`.
    IsNull { column: ColumnIndex, negated: bool },
}

impl Coded {
    // Borrowed for a column's value, owned for a null-ness key, so a
    // row's text is never copied to be coded.
    fn datum(self, row: &[Datum]) -> Cow<'_, Datum> {
        match self {
            Coded::Value(column) => Cow::Borrowed(&row[column.0]),
            Coded::IsNull { column, negated } => {
                Cow::Owned(Datum::Bool((row[column.0] == Datum::Null) != negated))
            }
        }
    }

    fn column_type(self, types: &[ColumnType]) -> ColumnType {
        match self {
            Coded::Value(column) => types[column.0],
            Coded::IsNull { .. } => ColumnType::Bool,
        }
    }
}

/// A column whose values a cell keeps, and in what form. Measures share
/// inputs: sum and avg read one `Sum`, min and max one `Extent`. Filtered
/// measures keep nothing of their own, since their conditions name rollup-key
/// parts only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Input {
    kind: StateKind,
    column: ColumnIndex,
}

/// A column a measure reads, and what of it. Read from the aggregate, not
/// the input: `sum(DISTINCT c)` keeps the state `count(DISTINCT c)` does,
/// which tells any values apart, yet adds its values, so reads numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ColumnRead {
    reads: Reads,
    column: ColumnIndex,
}

/// What each part of a key codes of a row of `columns`, the bucket aside.
fn coded(columns: &[Identifier], parts: &[KeyPart<'_>]) -> Vec<Coded> {
    let mut coded = Vec::with_capacity(parts.len());
    for part in parts {
        match *part {
            KeyPart::Column(column) => coded.push(Coded::Value(position_of(columns, column))),
            KeyPart::IsNull { column, negated } => coded.push(Coded::IsNull {
                column: position_of(columns, column),
                negated,
            }),
            KeyPart::Bucket => {}
        }
    }
    coded
}

/// `inputs` without one another input on its column serves: a `Sum`
/// beside a `Spread`, which keeps the sum and count too, and a `Distinct`
/// beside an `Extent`, which keeps the same count per number.
fn shared(inputs: &[Input]) -> Vec<Input> {
    let mut kept = Vec::with_capacity(inputs.len());
    for &input in inputs {
        let serving = serving(input);
        if serving != input && inputs.contains(&serving) {
            continue;
        }
        kept.push(input);
    }
    kept
}

/// The input that serves `input`'s column as well: a `Spread` for a
/// `Sum`, an `Extent` for a `Distinct`; for any other kind, the input
/// itself.
// An `Extent` reads numbers only, so beside one a `Distinct` never needs
// the map that holds values of other kinds.
fn serving(input: Input) -> Input {
    let kind = match input.kind {
        StateKind::Sum => StateKind::Spread,
        StateKind::Distinct => StateKind::Extent,
        _ => return input,
    };
    Input {
        kind,
        column: input.column,
    }
}

fn position_of(columns: &[Identifier], name: &Identifier) -> ColumnIndex {
    find_column(columns, name).expect(COLUMNS)
}

/// The first column of `columns` with the name `name`. A bound rollup
/// keeps a fact column before a ref column of the same name, so the first
/// is the fact column.
fn find_column(columns: &[Identifier], name: &Identifier) -> Option<ColumnIndex> {
    for (position, column) in columns.iter().enumerate() {
        if column == name {
            return Some(ColumnIndex(position));
        }
    }
    None
}

impl Rollup {
    /// Keyed as [`Fact::rollup_key`]: the cells an ask reads through the
    /// projections.
    pub(crate) fn new(fact: &Fact) -> Rollup {
        let columns = fact.columns();
        let parts = fact.rollup_key();
        let key_columns = coded(&columns, &parts);
        Rollup::build(fact, columns, key_columns)
    }

    /// Keyed by the `by` keys and every column the refs read, which a row
    /// carries after the fact's columns, so an ask reads no projection
    /// (ADR-0006). A column read is named as the view names it; a fact
    /// column of that name comes first, so the measure inputs find theirs.
    pub(crate) fn bound(fact: &Fact) -> Rollup {
        let mut columns = fact.columns();
        let parts = fact.by_parts();
        let mut key_columns = coded(&columns, &parts);
        for (_, attribute) in fact.attributes() {
            key_columns.push(Coded::Value(ColumnIndex(columns.len())));
            columns.push(attribute.name.clone());
        }
        Rollup::build(fact, columns, key_columns)
    }

    fn build(fact: &Fact, columns: Vec<Identifier>, key_columns: Vec<Coded>) -> Rollup {
        let mut inputs = Vec::new();
        let mut column_reads = Vec::new();
        for measure in &fact.measures {
            let aggregate = &measure.aggregate;
            let (Some(kind), Some(reads), Some(name)) =
                (aggregate.state_kind(), aggregate.reads(), aggregate.input())
            else {
                continue;
            };
            let column = position_of(&columns, name);
            let input = Input { kind, column };
            if !inputs.contains(&input) {
                inputs.push(input);
            }
            let read = ColumnRead { reads, column };
            if !column_reads.contains(&read) {
                column_reads.push(read);
            }
        }
        let inputs = shared(&inputs);
        let mut widths = Vec::with_capacity(1 + fact.time.wider.len());
        widths.push(fact.time.width);
        widths.extend_from_slice(&fact.time.wider);
        let mut levels = Vec::with_capacity(widths.len());
        for (index, width) in widths.iter().enumerate() {
            levels.push(Level {
                width: *width,
                buckets: BTreeMap::new(),
                horizon: horizon(&fact.time.retained, &widths, index),
            });
        }
        let mut codes = Vec::with_capacity(key_columns.len());
        codes.resize_with(key_columns.len(), Codes::default);
        Rollup {
            level_cells: vec![0; levels.len()],
            levels,
            codes,
            key_columns,
            inputs,
            column_reads,
            key: Vec::new(),
            types: vec![ColumnType::Int; columns.len()],
            columns,
        }
    }

    /// Columns a row holds, as [`Fact::columns`].
    pub(crate) fn column_count(&self) -> usize {
        self.columns.len()
    }

    /// Cells at the `date_bin` width; a cell goes when its last row is
    /// removed.
    pub(crate) fn cells(&self) -> usize {
        self.level_cells[DATE_BIN.0]
    }

    /// Cells at each level, the `date_bin` width first.
    pub(crate) fn level_cells(&self) -> &[usize] {
        &self.level_cells
    }

    /// Whether any level holds a cell. The widest level answers for all:
    /// every row goes into every level, so it holds a cell while any does.
    pub(crate) fn holds_cells(&self) -> bool {
        self.level_cells[self.widest().0] != 0
    }

    /// The widest level: every row goes into it, so it holds every key
    /// any level holds, in the fewest cells.
    fn widest(&self) -> LevelIndex {
        LevelIndex(self.levels.len() - 1)
    }

    /// How many levels the rollup keeps: the `date_bin` width and each
    /// wider one.
    pub(crate) fn level_count(&self) -> usize {
        self.levels.len()
    }

    /// Moves the horizon of every level kept for a time to `at`, in Unix
    /// seconds, dropping the buckets before it.
    pub(crate) fn advance(&mut self, at: i64) {
        for (level, cells) in self.levels.iter_mut().zip(&mut self.level_cells) {
            *cells -= level.advance(at);
        }
    }

    /// Drops every cell and code; the types and horizons stay.
    pub(crate) fn clear(&mut self) {
        for level in &mut self.levels {
            level.buckets.clear();
        }
        for codes in &mut self.codes {
            *codes = Codes::default();
        }
        self.level_cells.fill(0);
    }

    /// Only the source knows the row's types, so a load sets them; they are
    /// integers until then.
    pub(crate) fn set_types(&mut self, types: &[ColumnType]) -> Result<(), RowError> {
        arity(self.types.len(), types.len())?;
        self.types.copy_from_slice(types);
        Ok(())
    }

    /// The type of a rollup-key part's values.
    pub(crate) fn key_type(&self, part: PartIndex) -> ColumnType {
        self.key_columns[part.0].column_type(&self.types)
    }

    /// The type of each column of a row.
    pub(crate) fn types(&self) -> &[ColumnType] {
        &self.types
    }

    /// The time column's type.
    pub(crate) fn time_type(&self) -> ColumnType {
        self.types[TIME]
    }

    /// A measure over numbers keeps its column's scale; a count has none.
    pub(crate) fn scale_of(&self, aggregate: &Aggregate) -> u32 {
        match (aggregate.reads(), aggregate.input()) {
            (Some(Reads::Numbers), Some(column)) => {
                aggregate.answer_scale(self.types[self.column(column).expect(COLUMNS).0].scale())
            }
            _ => 0,
        }
    }

    /// Adds one row to its cell at every width.
    pub(crate) fn add(&mut self, row: &[Datum]) -> Result<(), RowError> {
        self.check(row)?;
        let mut key = mem::take(&mut self.key);
        key.clear();
        self.code(row, &mut key);
        for (level, cells) in self.levels.iter_mut().zip(&mut self.level_cells) {
            count_shift(cells, level.add(row, &key, &self.inputs));
        }
        self.key = key;
        Ok(())
    }

    /// Adds whole rows laid end to end, as a load reads them: each width
    /// of cells on its own thread, since they share nothing but the rows.
    /// If a row fails [`Rollup::add`]'s checks, none is added.
    pub(crate) fn add_rows(&mut self, rows: &[Datum]) -> Result<(), RowError> {
        let rows: Vec<&[Datum]> = split_rows(rows, self.column_count())?.collect();
        for row in &rows {
            self.check(row)?;
        }
        let mut keys = mem::take(&mut self.key);
        keys.clear();
        for row in &rows {
            self.code(row, &mut keys);
        }
        let coded = CodedRows {
            rows: &rows,
            keys: &keys,
            key_parts: self.key_columns.len(),
        };
        add_on_threads(&mut self.levels, coded, &self.inputs, &mut self.level_cells);
        self.key = keys;
        Ok(())
    }

    /// Takes one row back out of its cell at every width. After
    /// [`RowError::Missing`] the rollup no longer matches the table; start
    /// over.
    pub(crate) fn remove(&mut self, row: &[Datum]) -> Result<(), RowError> {
        self.check(row)?;
        let mut key = mem::take(&mut self.key);
        key.clear();
        let removed = match self.code_known(row, &mut key) {
            Ok(()) => self.remove_coded(row, &key),
            Err(error) => Err(error),
        };
        self.key = key;
        removed
    }

    /// The codes of a row whose every key value was coded before; a value
    /// never coded was never added.
    fn code_known(&self, row: &[Datum], key: &mut Vec<u32>) -> Result<(), RowError> {
        for (coded, codes) in self.key_columns.iter().zip(&self.codes) {
            let Some(code) = codes.get(&coded.datum(row)) else {
                return Err(RowError::Missing);
            };
            key.push(code);
        }
        Ok(())
    }

    fn remove_coded(&mut self, row: &[Datum], key: &[u32]) -> Result<(), RowError> {
        for (level, cells) in self.levels.iter_mut().zip(&mut self.level_cells) {
            count_shift(cells, level.remove(row, key, &self.inputs)?);
        }
        Ok(())
    }

    /// Moves one keyed cell's aggregates into or out of the bound cell
    /// `key` names, in `bucket` at its level only: `count` rows and entry `source_cell`
    /// of `source`, the keyed cell's states at that same level. A keyed
    /// cell at a wider level already merges its rows over the wider
    /// bucket, so each level moves its own cells.
    pub(crate) fn adjust(
        &mut self,
        bucket: LevelBucket,
        key: &[u32],
        count: u64,
        source: &[State],
        source_cell: usize,
        direction: Direction,
    ) -> Result<(), RowError> {
        let LevelBucket { level, start } = bucket;
        let time = match start {
            Some(start) => Datum::Int(start),
            None => Datum::Null,
        };
        let move_cell = MoveCell {
            key,
            count,
            source,
            source_cell,
            direction,
        };
        let shift = self.levels[level.0].adjust(&self.inputs, &time, move_cell)?;
        count_shift(&mut self.level_cells[level.0], shift);
        Ok(())
    }

    /// Drops the codes no cell holds any more from each part where they
    /// may be most of its codes, and gives the cells the new ones. Every
    /// key some level holds the widest level holds too, so a part holds at
    /// most as many live codes as there are cells at the widest level.
    pub(crate) fn reclaim_codes(&mut self) {
        let widest_cells = self.level_cells[self.widest().0];
        let mut maps = Vec::with_capacity(self.codes.len());
        let mut any = false;
        for part in 0..self.codes.len() {
            if !worth_recoding(self.codes[part].len(), widest_cells) {
                maps.push(None);
                continue;
            }
            let live = self.live_codes(PartIndex(part));
            maps.push(Some(self.codes[part].keep(&live)));
            any = true;
        }
        if !any {
            return;
        }
        for level in &mut self.levels {
            for cells in level.buckets.values_mut() {
                cells.recode(&maps);
            }
        }
    }

    /// Which of a part's codes some cell holds: the widest level holds
    /// every key a narrower one does.
    fn live_codes(&self, part: PartIndex) -> Vec<bool> {
        let mut live = vec![false; self.codes[part.0].len()];
        for cells in self.levels[self.widest().0].buckets.values() {
            for cell in 0..cells.len() {
                live[cells.code(cell, part) as usize] = true;
            }
        }
        live
    }

    /// The code of `datum` among a rollup-key part's values, given now if
    /// new.
    pub(crate) fn code_part(&mut self, part: PartIndex, datum: &Datum) -> u32 {
        self.codes[part.0].code(datum)
    }

    /// The code of `datum` among a rollup-key part's values, if the part
    /// ever met it.
    pub(crate) fn code_of(&self, part: PartIndex, datum: &Datum) -> Option<u32> {
        self.codes[part.0].get(datum)
    }

    /// The rollup-key part that codes the fact column, when one does.
    pub(crate) fn part_of_column(&self, column: ColumnIndex) -> Option<PartIndex> {
        for (part, coded) in self.key_columns.iter().enumerate() {
            if let Coded::Value(value_column) = coded
                && *value_column == column
            {
                return Some(PartIndex(part));
            }
        }
        None
    }

    /// The buckets of `level` by start, oldest first. `None` is the
    /// null-time bucket: no ask reads it, and a dim change still has to
    /// move it.
    pub(crate) fn buckets(&self, level: LevelIndex) -> &BTreeMap<Option<i64>, Cells> {
        &self.levels[level.0].buckets
    }

    /// Where among the cells' states an aggregate finds its own; none for
    /// a count of rows, which reads the row counts.
    pub(crate) fn state_of(&self, aggregate: &Aggregate) -> Option<InputIndex> {
        let kind = aggregate.state_kind()?;
        let column = self.column(aggregate.input()?)?;
        let wanted = Input { kind, column };
        for (position, input) in self.inputs.iter().enumerate() {
            if *input == wanted || *input == serving(wanted) {
                return Some(InputIndex(position));
            }
        }
        None
    }

    fn column(&self, name: &Identifier) -> Option<ColumnIndex> {
        find_column(&self.columns, name)
    }

    /// How many codes a rollup-key part has given.
    pub(crate) fn codes(&self, part: PartIndex) -> usize {
        self.codes[part.0].len()
    }

    /// The value a rollup-key part's code stands for.
    pub(crate) fn value(&self, part: PartIndex, code: u32) -> &Datum {
        self.codes[part.0].value(code)
    }

    /// Rejects a time so early or so late that some level's bucket for it
    /// would start or end outside the seconds `i64` holds.
    fn check_time(&self, seconds: i64) -> Result<(), RowError> {
        for level in &self.levels {
            let start = level.width.bucket_start(seconds);
            let end = start.and_then(|start| start.checked_add(i64::from(level.width.seconds)));
            if end.is_none() {
                return Err(RowError::TimeOutOfRange {
                    column: self.columns[TIME].clone(),
                });
            }
        }
        Ok(())
    }

    /// Rejects rows that would leave the rollup half updated.
    pub(crate) fn check(&self, row: &[Datum]) -> Result<(), RowError> {
        arity(self.column_count(), row.len())?;
        if !matches!(row[TIME], Datum::Null | Datum::Int(_)) {
            return Err(RowError::NotInt {
                column: self.columns[TIME].clone(),
            });
        }
        if let Datum::Int(seconds) = row[TIME] {
            self.check_time(seconds)?;
        }
        // Numbers before booleans, so a row wrong in both names a number.
        for read in &self.column_reads {
            let column = read.column.0;
            if read.reads == Reads::Numbers && !matches!(row[column], Datum::Null | Datum::Int(_)) {
                return Err(RowError::NotInt {
                    column: self.columns[column].clone(),
                });
            }
        }
        for read in &self.column_reads {
            let column = read.column.0;
            if read.reads == Reads::Booleans && !matches!(row[column], Datum::Null | Datum::Bool(_))
            {
                return Err(RowError::NotBool {
                    column: self.columns[column].clone(),
                });
            }
        }
        Ok(())
    }

    /// Appends the codes of `row`'s rollup key to `key`, giving new
    /// values their codes.
    fn code(&mut self, row: &[Datum], key: &mut Vec<u32>) {
        for (coded, codes) in self.key_columns.iter().zip(&mut self.codes) {
            key.push(codes.code(&coded.datum(row)));
        }
    }
}

/// The horizon of the level of `widths[index]`, when the view keeps that
/// width only for a time; `Spec::new` saw that a wider width follows it.
fn horizon(retained: &[Retention], widths: &[Width], index: usize) -> Option<Horizon> {
    for retention in retained {
        if retention.width == widths[index] {
            return Some(Horizon {
                kept_for: retention.kept_for,
                next_wider: widths[index + 1],
                kept_from: None,
            });
        }
    }
    None
}

/// Counts the cell a [`Shift`] made or took out into `cells`.
fn count_shift(cells: &mut usize, shift: Shift) {
    match shift {
        Shift::New => *cells += 1,
        Shift::Removed => *cells -= 1,
        Shift::Same => {}
    }
}

/// Adds a load's rows at every level, each wider level on a thread of
/// its own, and counts each level's new cells into `level_cells`.
fn add_on_threads(
    levels: &mut [Level],
    coded: CodedRows<'_>,
    inputs: &[Input],
    level_cells: &mut [usize],
) {
    let (date_bin, wider) = levels.split_first_mut().expect("a rollup has its width");
    let (date_bin_cells, wider_cells) = level_cells.split_first_mut().expect("one count per level");
    // `thread::scope` and `spawn` take closures: a scoped thread may
    // borrow the rows, and the scope waits for every thread it spawned.
    thread::scope(|scope| {
        let mut threads = Vec::with_capacity(wider.len());
        for (level, cells) in wider.iter_mut().zip(wider_cells) {
            threads.push(scope.spawn(move || *cells += level.add_coded(coded, inputs)));
        }
        *date_bin_cells += date_bin.add_coded(coded, inputs);
        for thread in threads {
            if let Err(payload) = thread.join() {
                panic::resume_unwind(payload);
            }
        }
    });
}
