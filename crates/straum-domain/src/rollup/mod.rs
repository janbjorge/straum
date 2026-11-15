//! A rollup: every cell of one fact, at each bucket width the view keeps.
//! A row comes in, its rollup-key values are coded to small integers, and
//! it adds to the cell of those codes in the bucket its time falls in, at
//! every width; a removed row takes back out what it added. An ask reads
//! the cells of the buckets that cover its range. The store keeps one
//! rollup per fact, and a second, bound one for a fact whose joins allow.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::ops::Range;
use std::{mem, panic, thread};

use crate::aggregate::{Aggregate, Reads};
use crate::codes::Codes;
use crate::identifier::Identifier;
use crate::spec::{Fact, KeyPart};
use crate::value::{ColumnType, Datum};

mod cells;
mod level;
mod row_error;
mod sealing;
mod state;

pub(crate) use cells::Cells;
pub(crate) use level::Bucket;
use level::{CodedRows, Level, MoveCell, Shift};
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
struct LevelIndex(usize);

/// One bucket of a rollup: its width and its start, both in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct BucketId {
    pub(crate) width: i64,
    pub(crate) start: i64,
}

/// A bucket an ask reads, hot or sealed.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CoveredBucket<'r> {
    pub(crate) id: BucketId,
    // Borrowed: a bucket's cells are the bulk of the store.
    pub(crate) bucket: &'r Bucket,
}

/// Which buckets [`Rollup::cover`] picks for a range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cover {
    /// The widest buckets that fit, narrower ones at the ends.
    WidestFit,
    /// Only `date_bin` buckets, as an ask grouped by the bucket needs.
    EachDateBin,
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
    /// One per rollup-key column: its values under small codes, so a cell
    /// key is a few integers.
    codes: Vec<Codes>,
    /// The `date_bin` width first, then the wider ones.
    levels: Vec<Level>,
    /// Cells at the `date_bin` width, sealed ones included.
    cells: usize,
    /// Sealed buckets, every width.
    sealed: usize,
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

fn position_of(columns: &[Identifier], name: &Identifier) -> ColumnIndex {
    for (position, column) in columns.iter().enumerate() {
        if column == name {
            return ColumnIndex(position);
        }
    }
    panic!("{COLUMNS}")
}

impl Rollup {
    /// Keyed as [`Fact::rollup_key`]: the cells an ask reads through the
    /// projections.
    pub(crate) fn new(fact: &Fact) -> Rollup {
        let columns = fact.columns();
        let parts: Vec<KeyPart<'_>> = fact.rollup_key().collect();
        let key_columns = coded(&columns, &parts);
        Rollup::build(fact, columns, key_columns)
    }

    /// Keyed by the `by` keys and every column the refs read, which a row
    /// carries after the fact's columns, so an ask reads no projection
    /// (ADR-0038). A column read is named as the view names it; a fact
    /// column of that name comes first, so the measure inputs find theirs.
    pub(crate) fn bound(fact: &Fact) -> Rollup {
        let mut columns = fact.columns();
        let parts: Vec<KeyPart<'_>> = fact.by_parts().collect();
        let mut key_columns = coded(&columns, &parts);
        for (_, attribute) in fact.attributes() {
            key_columns.push(Coded::Value(ColumnIndex(columns.len())));
            columns.push(attribute.name.clone());
        }
        Rollup::build(fact, columns, key_columns)
    }

    fn build(fact: &Fact, columns: Vec<Identifier>, key_columns: Vec<Coded>) -> Rollup {
        let mut inputs = Vec::new();
        for measure in &fact.measures {
            let aggregate = &measure.aggregate;
            let (Some(kind), Some(name)) = (aggregate.state_kind(), aggregate.input()) else {
                continue;
            };
            let input = Input {
                kind,
                column: position_of(&columns, name),
            };
            if !inputs.contains(&input) {
                inputs.push(input);
            }
        }
        let mut levels = Vec::with_capacity(1 + fact.time.wider.len());
        levels.push(Level {
            width: i64::from(fact.time.width.seconds),
            buckets: BTreeMap::new(),
        });
        for width in &fact.time.wider {
            levels.push(Level {
                width: i64::from(width.seconds),
                buckets: BTreeMap::new(),
            });
        }
        let mut codes = Vec::with_capacity(key_columns.len());
        codes.resize_with(key_columns.len(), Codes::default);
        Rollup {
            levels,
            codes,
            key_columns,
            inputs,
            cells: 0,
            sealed: 0,
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
        self.cells
    }

    /// Sealed buckets, every width.
    pub(crate) fn sealed_count(&self) -> usize {
        self.sealed
    }

    /// From the start of the first `date_bin` bucket to the end of the
    /// last, sealed ones included. Rows without a time are in no bucket
    /// an ask reads, so they count for nothing here.
    pub(crate) fn range(&self) -> Option<Range<i64>> {
        let level = &self.levels[DATE_BIN.0];
        // `flatten` passes over the `None` key of the null-time bucket.
        let mut starts = level.buckets.keys().flatten();
        let first = *starts.next()?;
        let last = match starts.next_back() {
            Some(last) => *last,
            None => first,
        };
        Some(first..last + level.width)
    }

    /// Drops every cell and code; the types stay.
    pub(crate) fn clear(&mut self) {
        for level in &mut self.levels {
            level.buckets.clear();
        }
        for codes in &mut self.codes {
            *codes = Codes::default();
        }
        self.cells = 0;
        self.sealed = 0;
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
                self.types[self.column(column).expect(COLUMNS).0].scale()
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
        for (position, level) in self.levels.iter_mut().enumerate() {
            let new = level.add(row, &key, &self.inputs);
            if LevelIndex(position) == DATE_BIN {
                self.cells += usize::from(new);
            }
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
        self.cells += add_on_threads(&mut self.levels, coded, &self.inputs);
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
        for (position, level) in self.levels.iter_mut().enumerate() {
            let cell_gone = level.remove(row, key, &self.inputs)?;
            if cell_gone && LevelIndex(position) == DATE_BIN {
                self.cells -= 1;
            }
        }
        Ok(())
    }

    /// Moves one keyed cell's aggregates into or out of the bound cell
    /// `key` names, at every width: `count` rows and entry `source_cell`
    /// of `source`, the keyed cell's states.
    pub(crate) fn adjust(
        &mut self,
        time: Option<i64>,
        key: &[u32],
        count: u64,
        source: &[State],
        source_cell: usize,
        direction: Direction,
    ) -> Result<(), RowError> {
        let time = match time {
            Some(time) => Datum::Int(time),
            None => Datum::Null,
        };
        let move_cell = MoveCell {
            key,
            count,
            source,
            source_cell,
            direction,
        };
        for (position, level) in self.levels.iter_mut().enumerate() {
            let shift = level.adjust(&self.inputs, &time, move_cell)?;
            if LevelIndex(position) != DATE_BIN {
                continue;
            }
            match shift {
                Shift::New => self.cells += 1,
                Shift::Removed => self.cells -= 1,
                Shift::Same => {}
            }
        }
        Ok(())
    }

    /// The code of `datum` among a rollup-key part's values, given now if
    /// new.
    pub(crate) fn code_part(&mut self, part: PartIndex, datum: &Datum) -> u32 {
        self.codes[part.0].code(datum)
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

    /// The `date_bin` width, in seconds.
    pub(crate) fn date_bin_width(&self) -> i64 {
        self.levels[DATE_BIN.0].width
    }

    /// Hot `date_bin` buckets, oldest first. `None` is the null-time
    /// bucket: no ask reads it, and a dim change still has to move it.
    pub(crate) fn hot_date_bin_starts(&self) -> Vec<Option<i64>> {
        let level = &self.levels[DATE_BIN.0];
        let mut starts = Vec::new();
        for (start, bucket) in &level.buckets {
            if let Bucket::Hot(_) = bucket {
                starts.push(*start);
            }
        }
        starts
    }

    /// The cells of a hot `date_bin` bucket; none if it is sealed or has
    /// no cells.
    pub(crate) fn hot_date_bin(&self, start: Option<i64>) -> Option<&Cells> {
        let bucket = self.levels[DATE_BIN.0].buckets.get(&start)?;
        bucket.cells()
    }

    /// A sealed `date_bin` bucket overlaps `[from, to)`. Asks are aligned
    /// to that width, so a bucket in the range starts inside it.
    pub(crate) fn overlaps_sealed(&self, from: i64, to: i64) -> bool {
        let level = &self.levels[DATE_BIN.0];
        for (_, bucket) in level.buckets.range(Some(from)..Some(to)) {
            if let Bucket::Sealed { .. } = bucket {
                return true;
            }
        }
        false
    }

    /// Where among the cells' states an aggregate finds its own; none for
    /// a count of rows, which reads the row counts.
    pub(crate) fn state_of(&self, aggregate: &Aggregate) -> Option<InputIndex> {
        let kind = aggregate.state_kind()?;
        let column = self.column(aggregate.input()?)?;
        let wanted = Input { kind, column };
        for (position, input) in self.inputs.iter().enumerate() {
            if *input == wanted {
                return Some(InputIndex(position));
            }
        }
        None
    }

    fn column(&self, name: &Identifier) -> Option<ColumnIndex> {
        for (position, column) in self.columns.iter().enumerate() {
            if column == name {
                return Some(ColumnIndex(position));
            }
        }
        None
    }

    /// How many codes a rollup-key part has given.
    pub(crate) fn codes(&self, part: PartIndex) -> usize {
        self.codes[part.0].len()
    }

    /// The value a rollup-key part's code stands for.
    pub(crate) fn value(&self, part: PartIndex, code: u32) -> &Datum {
        self.codes[part.0].value(code)
    }

    /// Rejects rows that would leave the rollup half updated.
    fn check(&self, row: &[Datum]) -> Result<(), RowError> {
        arity(self.column_count(), row.len())?;
        if !matches!(row[TIME], Datum::Null | Datum::Int(_)) {
            return Err(RowError::NotInt {
                column: self.columns[TIME].clone(),
            });
        }
        // Numbers before booleans, so a row wrong in both names a number.
        for input in &self.inputs {
            let column = input.column.0;
            if input.kind.reads() == Reads::Numbers
                && !matches!(row[column], Datum::Null | Datum::Int(_))
            {
                return Err(RowError::NotInt {
                    column: self.columns[column].clone(),
                });
            }
        }
        for input in &self.inputs {
            let column = input.column.0;
            if input.kind.reads() == Reads::Booleans
                && !matches!(row[column], Datum::Null | Datum::Bool(_))
            {
                return Err(RowError::NotBool {
                    column: self.columns[column].clone(),
                });
            }
        }
        if self.touches_sealed(&row[TIME]) {
            return Err(RowError::Sealed);
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

/// Adds a load's rows at every level, each wider level on a thread of
/// its own; how many `date_bin` cells are new.
fn add_on_threads(levels: &mut [Level], coded: CodedRows<'_>, inputs: &[Input]) -> usize {
    let (date_bin, wider) = levels.split_first_mut().expect("a rollup has its width");
    // `thread::scope` and `spawn` take closures: a scoped thread may
    // borrow the rows, and the scope waits for every thread it spawned.
    thread::scope(|scope| {
        let mut threads = Vec::with_capacity(wider.len());
        for level in wider {
            threads.push(scope.spawn(move || level.add_coded(coded, inputs)));
        }
        let new_cells = date_bin.add_coded(coded, inputs);
        for thread in threads {
            if let Err(payload) = thread.join() {
                panic::resume_unwind(payload);
            }
        }
        new_cells
    })
}
