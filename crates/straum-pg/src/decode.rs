//! pgoutput messages turned into commits of fact and dim rows. pgoutput is
//! PostgreSQL's logical decoding output: a Relation message describes a
//! table (a relation, PostgreSQL's word), then Begin, row changes and
//! Commit follow. `pgoutput.rs` parses the bytes; the `Decoder` here keeps
//! where each spec table's columns sit and hands `stream.rs` whole
//! `Commit`s. The physical stream feeds it the same messages.

use std::{mem, slice};

use straum_app::{Change, Commit, Drift, Lsn};
use straum_domain::{ColumnType, Datum, FastMap, Identifier};

use crate::catalog::Checked;
use crate::error::Error;
use crate::pgoutput::{self, Field, Message, Relation, RelationColumn, Tuple};
use crate::problem::{ColumnProblem, Problem};
use crate::unexpected::Unexpected;
use crate::value::{Oid, read, type_of};

/// Turns pgoutput messages into commits of rows, each as the stream brings
/// it: `Fact::columns` or `Dim::columns`.
pub(crate) struct Decoder {
    /// Each with its types as last loaded or announced. A Relation that
    /// changes them changes what rows read as, so Straum stops.
    tables: Vec<Checked>,
    /// By relation id. A table read by two members of the spec maps to
    /// both.
    relations: FastMap<Oid, Vec<Mapping>>,
    transaction: Transaction,
}

/// The transaction so far, held until its Commit; `limit` bounds its
/// rows, since every change here is applied under the write lock, and
/// `byte_limit` bounds the bytes its changes hold.
struct Transaction {
    changes: Vec<Change>,
    /// Rows of the spec's tables changed so far, each once.
    rows: usize,
    limit: usize,
    /// The bytes of the changes held so far, as [`footprint`] counts.
    bytes: usize,
    /// `--max-stream-memory`.
    byte_limit: usize,
    /// The transaction being read, by the LSN of its commit record.
    lsn: Option<Lsn>,
}

/// Where a table's columns sit in a relation's tuples.
struct Mapping {
    table: TableIndex,
    /// For each column the table reads, its place in the relation's tuples.
    positions: Vec<usize>,
    /// How many columns the relation's tuples have.
    width: usize,
}

/// A table's place in `Decoder::tables`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TableIndex(usize);

impl Decoder {
    /// `tables` are the spec's tables, dims and facts; `limit` is the
    /// most rows of them one transaction may change, and `byte_limit` the
    /// most bytes its changes may hold.
    pub(crate) fn new(tables: Vec<Checked>, limit: usize, byte_limit: usize) -> Decoder {
        Decoder {
            tables,
            relations: FastMap::default(),
            transaction: Transaction {
                changes: Vec::new(),
                rows: 0,
                limit,
                bytes: 0,
                byte_limit,
                lsn: None,
            },
        }
    }

    /// A commit once its Commit message comes.
    pub(crate) fn feed(&mut self, message: &Message) -> Result<Option<Commit>, Error> {
        let relations = match message {
            Message::Begin { final_lsn } => {
                self.transaction.lsn = Some(Lsn(*final_lsn));
                return Ok(None);
            }
            Message::Commit { end_lsn, committed } => {
                self.transaction.lsn = None;
                self.transaction.rows = 0;
                self.transaction.bytes = 0;
                let changes = mem::take(&mut self.transaction.changes);
                return Ok(Some(Commit {
                    lsn: Lsn(*end_lsn),
                    committed: *committed,
                    changes,
                }));
            }
            Message::Relation(relation) => {
                self.relation(relation)?;
                return Ok(None);
            }
            Message::Insert { relation, .. }
            | Message::Update { relation, .. }
            | Message::Delete { relation, .. } => slice::from_ref(relation),
            Message::Truncate { relations } => relations.as_slice(),
            Message::Other => &[],
        };
        for &relation in relations {
            // A relation outside the spec holds none of its tables.
            let Some(mappings) = self.relations.get(&relation) else {
                continue;
            };
            if mappings.is_empty() {
                continue;
            }
            self.transaction.count_row()?;
            for mapping in mappings {
                let table = &self.tables[mapping.table.0];
                let change = match mapping.change(table, message)? {
                    Some(change) => change,
                    None => unfit(table, Drift::Altered),
                };
                self.transaction.hold(change)?;
            }
        }
        Ok(None)
    }

    /// Every table of the spec `relation` holds no longer reads as
    /// loaded, though no message says how: the physical stream's tables
    /// rewritten or dropped. Between a Begin and its Commit.
    pub(crate) fn unfit(&mut self, relation: Oid, drift: Drift) {
        for mapping in self.relations.get(&relation).into_iter().flatten() {
            self.transaction
                .changes
                .push(unfit(&self.tables[mapping.table.0], drift));
        }
    }

    /// Between a Begin and its Commit.
    pub(crate) fn in_transaction(&self) -> bool {
        self.transaction.lsn.is_some()
    }

    /// Maps the relation's columns for each table of the spec it holds. A
    /// table whose columns now read differently is read again; one that
    /// lost a column, or whose column turned to a type Straum doesn't
    /// read, stops the stream, and the setup check on the next start says
    /// what is wrong. A table loaded under another name now, or another
    /// table under its name, no longer reads as loaded.
    fn relation(&mut self, relation: &Relation) -> Result<(), Error> {
        let mut mappings = Vec::new();
        for (index, table) in self.tables.iter_mut().enumerate() {
            let same_name = table.schema.as_str() == relation.schema
                && table.table_name.as_str() == relation.name;
            let same_table = table.oid == relation.id;
            if !same_name && !same_table {
                continue;
            }
            if same_name != same_table {
                self.transaction.changes.push(unfit(table, Drift::Altered));
                continue;
            }
            let mut positions = Vec::new();
            let mut types = Vec::new();
            for column in &table.columns {
                let Some(position) = position_in(relation, column) else {
                    return Err(changed(table, column));
                };
                let Some(column_type) = read_as(table, &relation.columns[position]) else {
                    return Err(changed(table, column));
                };
                positions.push(position);
                types.push(column_type);
            }
            if table.types != types {
                self.transaction.changes.push(unfit(table, Drift::Altered));
                table.types = types;
            }
            mappings.push(Mapping {
                table: TableIndex(index),
                positions,
                width: relation.columns.len(),
            });
        }
        self.relations.insert(relation.id, mappings);
        Ok(())
    }
}

impl Transaction {
    /// Counts one more row changed, or stops at the limit. The count is
    /// per transaction, not per table: the lock is held for all of it at
    /// once. A row counts once, however many tables of the spec read it.
    fn count_row(&mut self) -> Result<(), Error> {
        if self.rows >= self.limit {
            return Err(Error::Transaction {
                lsn: self.lsn.unwrap_or(Lsn(0)),
                limit: self.limit,
            });
        }
        self.rows += 1;
        Ok(())
    }

    /// Holds `change`, or stops once the changes held pass the byte
    /// limit: a transaction under the row limit can still hold gigabytes
    /// of text.
    fn hold(&mut self, change: Change) -> Result<(), Error> {
        self.bytes = self.bytes.saturating_add(footprint(&change));
        if self.bytes > self.byte_limit {
            return Err(Error::TransactionMemory {
                lsn: self.lsn.unwrap_or(Lsn(0)),
                limit: self.byte_limit,
            });
        }
        self.changes.push(change);
        Ok(())
    }
}

/// The bytes `change` holds, near enough to bound memory by: the change
/// and its rows' values, text by its length.
// Leaves out the tables' names, shared with the spec, and `Vec` slack.
pub(crate) fn footprint(change: &Change) -> usize {
    size_of::<Change>()
        + match change {
            Change::Insert { row, .. } => row_footprint(row),
            Change::Update { old, new, .. } => row_footprint(old) + row_footprint(new),
            Change::Delete { old, .. } => row_footprint(old),
            Change::Truncate { .. } | Change::Unfit { .. } => 0,
        }
}

/// The bytes of one row's values, text by its length.
fn row_footprint(row: &[Datum]) -> usize {
    let mut bytes = size_of_val(row);
    for datum in row {
        if let Datum::Text(text) = datum {
            bytes += text.len();
        }
    }
    bytes
}

/// Where the relation has the column named `column`, if it has one.
fn position_in(relation: &Relation, column: &Identifier) -> Option<usize> {
    for (position, candidate) in relation.columns.iter().enumerate() {
        if candidate.name == column.as_str() {
            return Some(position);
        }
    }
    None
}

fn changed(table: &Checked, column: &Identifier) -> Error {
    Error::Setup(vec![Problem::Column {
        table: table.names(),
        column: column.clone(),
        problem: ColumnProblem::Changed,
    }])
}

/// How the decoder reads a relation's column: an enum of the table's as
/// its label's text.
fn read_as(table: &Checked, column: &RelationColumn) -> Option<ColumnType> {
    if let Some(column_type) = type_of(column.type_oid, column.typmod) {
        return Some(column_type);
    }
    if table.enums.contains(&column.type_oid) {
        return Some(ColumnType::Text);
    }
    None
}

fn unfit(table: &Checked, drift: Drift) -> Change {
    Change::Unfit {
        table: table.name.clone(),
        drift,
    }
}

impl Mapping {
    /// The change a row message makes to the table; `None` when it can't
    /// be read whole.
    fn change(&self, table: &Checked, message: &Message) -> Result<Option<Change>, Error> {
        Ok(match message {
            Message::Insert { new, .. } => {
                let Some(row) = self.row(table, new, None)? else {
                    return Ok(None);
                };
                Some(Change::Insert {
                    table: table.name.clone(),
                    row,
                })
            }
            Message::Update { old, new, .. } => {
                let old = match old {
                    Some(old) => self.old(table, old)?,
                    None => None,
                };
                match (self.row(table, new, old.as_deref())?, old) {
                    (Some(new), Some(old)) => Some(Change::Update {
                        table: table.name.clone(),
                        old,
                        new,
                    }),
                    _ => None,
                }
            }
            Message::Delete { old, .. } => {
                let Some(old) = self.old(table, old)? else {
                    return Ok(None);
                };
                Some(Change::Delete {
                    table: table.name.clone(),
                    old,
                })
            }
            Message::Truncate { .. } => Some(Change::Truncate {
                table: table.name.clone(),
            }),
            Message::Begin { .. }
            | Message::Commit { .. }
            | Message::Relation(_)
            | Message::Other => None,
        })
    }

    /// The row a tuple holds. A value PostgreSQL left out because the
    /// change kept it comes from `old`; `None` when there is no old row to
    /// take it from.
    fn row(
        &self,
        table: &Checked,
        tuple: &Tuple,
        old: Option<&[Datum]>,
    ) -> Result<Option<Vec<Datum>>, Error> {
        self.fits(tuple)?;
        let mut row = Vec::with_capacity(self.positions.len());
        for (index, &position) in self.positions.iter().enumerate() {
            let datum = match (&tuple[position], old) {
                (Field::Null, _) => Datum::Null,
                (Field::Text(text), _) => read(table.types[index], text)
                    .map_err(|error| error.at(&table.qualified(), &table.columns[index]))?,
                (Field::Datum(datum), _) => datum.clone(),
                (Field::Unchanged, Some(old)) => old[index].clone(),
                (Field::Unchanged, None) => return Ok(None),
            };
            row.push(datum);
        }
        Ok(Some(row))
    }

    /// The old row of an update or delete as Straum needs it; `None` when
    /// the change can't be read whole. A key-only old row is one: the table
    /// no longer has `REPLICA IDENTITY FULL`, so Straum stops and the
    /// setup check says so.
    fn old(&self, table: &Checked, old: &pgoutput::Old) -> Result<Option<Vec<Datum>>, Error> {
        match old {
            pgoutput::Old::Row(tuple) => self.row(table, tuple, None),
            pgoutput::Old::Key(tuple) => {
                self.fits(tuple)?;
                Ok(None)
            }
        }
    }

    fn fits(&self, tuple: &Tuple) -> Result<(), Error> {
        if tuple.len() == self.width {
            return Ok(());
        }
        Err(Error::Protocol(Unexpected::TupleWidth {
            columns: tuple.len(),
            relation_columns: self.width,
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::pgoutput::RelationColumn;
    use crate::wal::test_support::Rng;

    const INT4: u32 = 23;
    const TEXT: u32 = 25;
    const UUID: u32 = 2950;
    const FLOAT8: u32 = 701;
    /// Rows a transaction may change in these tests.
    const LIMIT: usize = 16;
    /// Bytes a transaction may hold in these tests: far above any but
    /// the tests of the byte limit, which set their own.
    const BYTE_LIMIT: usize = 1 << 30;

    fn id(name: &str) -> Identifier {
        Identifier::new(name).unwrap()
    }

    fn unfit_named(name: &str) -> Change {
        Change::Unfit {
            table: id(name),
            drift: Drift::Altered,
        }
    }

    /// `pg_class.oid` of `public.zones` when it was loaded.
    const ZONES_OID: u32 = 7;

    /// Two facts read `public.zones`: `zones`, reading `id` and `name`, and
    /// `visits`, reading `name` alone.
    fn zones_decoder() -> Decoder {
        Decoder::new(
            vec![
                Checked {
                    oid: ZONES_OID,
                    ..Checked::public(
                        "zones",
                        "zones",
                        &["id", "name"],
                        vec![ColumnType::Int, ColumnType::Text],
                    )
                },
                Checked {
                    oid: ZONES_OID,
                    ..Checked::public("visits", "zones", &["name"], vec![ColumnType::Text])
                },
            ],
            LIMIT,
            BYTE_LIMIT,
        )
    }

    /// A Relation for `public.zones`.
    fn relation(id: u32, columns: &[(&str, u32)]) -> Message<'static> {
        named_relation(id, "zones", columns)
    }

    /// A Relation for `public.<name>`.
    fn named_relation(id: u32, name: &str, columns: &[(&str, u32)]) -> Message<'static> {
        let mut relation_columns = Vec::new();
        for &(name, type_oid) in columns {
            relation_columns.push(RelationColumn {
                name: name.into(),
                type_oid,
                typmod: -1,
            });
        }
        Message::Relation(Relation {
            id,
            schema: "public".into(),
            name: name.into(),
            columns: relation_columns,
        })
    }

    fn zones() -> Message<'static> {
        relation(7, &[("ref", UUID), ("id", INT4), ("name", TEXT)])
    }

    fn row<'a>(id: Field<'a>, name: Field<'a>) -> Tuple<'a> {
        vec![Field::Text("r-1"), id, name]
    }

    fn feed(decoder: &mut Decoder, messages: Vec<Message>) -> Result<Vec<Change>, Error> {
        decoder.feed(&Message::Begin { final_lsn: 9 })?;
        for message in messages {
            decoder.feed(&message)?;
        }
        let time = UNIX_EPOCH + Duration::from_secs(9);
        let commit = (decoder.feed(&Message::Commit {
            end_lsn: 9,
            committed: time,
        }))?
        .unwrap();
        assert_eq!((commit.lsn, commit.committed), (Lsn(9), time));
        Ok(commit.changes)
    }

    fn int(n: i64) -> Datum {
        Datum::Int(n)
    }

    fn text(value: &str) -> Datum {
        Datum::Text(value.into())
    }

    /// A row of `public.zones` whose name PostgreSQL left out, unchanged.
    fn unchanged() -> Tuple<'static> {
        row(Field::Text("1"), Field::Unchanged)
    }

    /// An insert of zone 1, the Bronx.
    fn insert_bronx() -> Message<'static> {
        Message::Insert {
            relation: 7,
            new: row(Field::Text("1"), Field::Text("Bronx")),
        }
    }

    /// The zones relation, then `count` inserts.
    fn zones_and_inserts(count: usize) -> Vec<Message<'static>> {
        let mut messages = vec![zones()];
        for _ in 0..count {
            messages.push(insert_bronx());
        }
        messages
    }

    /// An enum's OID is its own, so a relation names it only through the
    /// catalog's list; another OID under the column's name is a change.
    #[test]
    fn an_enum_column_reads_as_its_label() {
        let moods = Checked {
            oid: ZONES_OID,
            enums: vec![777],
            ..Checked::public("moods", "zones", &["mood"], vec![ColumnType::Text])
        };
        let mut decoder = Decoder::new(vec![moods], LIMIT, BYTE_LIMIT);
        let changes = feed(
            &mut decoder,
            vec![
                relation(7, &[("mood", 777)]),
                Message::Insert {
                    relation: 7,
                    new: vec![Field::Text("ok")],
                },
            ],
        )
        .unwrap();
        assert_eq!(
            changes,
            [Change::Insert {
                table: id("moods"),
                row: vec![text("ok")]
            }]
        );
        let error = feed(&mut decoder, vec![relation(7, &[("mood", 778)])]).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("moods: column mood of public.zones was dropped or changed type"),
            "{error}"
        );
    }

    #[test]
    fn rows_for_each_fact() {
        let mut decoder = zones_decoder();
        let (zones_name, visits_name) = (id("zones"), id("visits"));
        let changes = feed(
            &mut decoder,
            vec![
                zones(),
                Message::Insert {
                    relation: 7,
                    new: row(Field::Text("1"), Field::Text("Bronx")),
                },
                // An unchanged TOASTed value comes from the old row.
                Message::Update {
                    relation: 7,
                    old: Some(pgoutput::Old::Row(row(
                        Field::Text("1"),
                        Field::Text("Bronx"),
                    ))),
                    new: row(Field::Text("1"), Field::Unchanged),
                },
                Message::Delete {
                    relation: 7,
                    old: pgoutput::Old::Row(row(Field::Text("1"), Field::Null)),
                },
                Message::Truncate {
                    relations: vec![7, 8],
                },
                Message::Other,
            ],
        )
        .unwrap();
        let bronx = vec![int(1), text("Bronx")];
        assert_eq!(
            changes,
            [
                Change::Insert {
                    table: zones_name.clone(),
                    row: bronx.clone()
                },
                Change::Insert {
                    table: visits_name.clone(),
                    row: vec![text("Bronx")]
                },
                Change::Update {
                    table: zones_name.clone(),
                    old: bronx.clone(),
                    new: bronx
                },
                Change::Update {
                    table: visits_name.clone(),
                    old: vec![text("Bronx")],
                    new: vec![text("Bronx")]
                },
                Change::Delete {
                    table: zones_name.clone(),
                    old: vec![int(1), Datum::Null]
                },
                Change::Delete {
                    table: visits_name.clone(),
                    old: vec![Datum::Null]
                },
                Change::Truncate { table: zones_name },
                Change::Truncate { table: visits_name },
            ]
        );
    }

    /// Without the whole old row, an update or delete can't take the row
    /// out of its cell: the fact has lost `REPLICA IDENTITY FULL`, or a
    /// value it kept was left out with no old row to take it from.
    #[test]
    fn changes_without_the_old_row_start_over() {
        let mut decoder = zones_decoder();
        let changes = feed(
            &mut decoder,
            vec![
                zones(),
                Message::Insert {
                    relation: 7,
                    new: unchanged(),
                },
                Message::Update {
                    relation: 7,
                    old: None,
                    new: row(Field::Text("1"), Field::Text("Bronx")),
                },
                Message::Update {
                    relation: 7,
                    old: Some(pgoutput::Old::Key(row(Field::Text("1"), Field::Null))),
                    new: row(Field::Text("1"), Field::Text("Bronx")),
                },
                Message::Update {
                    relation: 7,
                    old: Some(pgoutput::Old::Row(unchanged())),
                    new: unchanged(),
                },
                Message::Delete {
                    relation: 7,
                    old: pgoutput::Old::Key(row(Field::Text("1"), Field::Null)),
                },
                Message::Delete {
                    relation: 7,
                    old: pgoutput::Old::Row(unchanged()),
                },
            ],
        )
        .unwrap();
        let mut expected = Vec::new();
        for _ in 0..6 {
            expected.extend([unfit_named("zones"), unfit_named("visits")]);
        }
        assert_eq!(changes, expected);
    }

    /// The table loaded under another name now, or another table under its
    /// name, no longer reads as loaded; its rows aren't read.
    #[test]
    fn a_table_renamed_or_replaced_is_unfit() {
        let renamed = named_relation(
            ZONES_OID,
            "zones_old",
            &[("ref", UUID), ("id", INT4), ("name", TEXT)],
        );
        let replaced = relation(
            ZONES_OID + 1,
            &[("ref", UUID), ("id", INT4), ("name", TEXT)],
        );
        for message in [renamed, replaced] {
            let mut decoder = zones_decoder();
            let changes = feed(
                &mut decoder,
                vec![
                    Message::Begin { final_lsn: 0x10 },
                    message,
                    Message::Insert {
                        relation: ZONES_OID,
                        new: row(Field::Text("1"), Field::Text("Bronx")),
                    },
                ],
            )
            .unwrap();
            assert_eq!(changes, [unfit_named("zones"), unfit_named("visits")]);
        }
    }

    #[test]
    fn a_relation_made_unfit_unfits_its_tables() {
        let mut decoder = zones_decoder();
        let begin = Message::Begin { final_lsn: 0x10 };
        let commit = Message::Commit {
            end_lsn: 0x20,
            committed: UNIX_EPOCH,
        };
        decoder.feed(&zones()).unwrap();
        decoder.feed(&begin).unwrap();
        // Not a relation the decoder knows: nothing to say.
        decoder.unfit(8, Drift::Altered);
        decoder.unfit(7, Drift::Altered);
        let got = decoder.feed(&commit).unwrap().unwrap();
        assert_eq!(got.changes, [unfit_named("zones"), unfit_named("visits")]);
    }

    #[test]
    fn relations_that_change() {
        let mut decoder = zones_decoder();
        // Not in the spec: its changes are skipped.
        let other = named_relation(8, "other", &[("x", INT4)]);
        // The same types in another order; then a column that reads
        // differently.
        let reordered = relation(
            7,
            &[
                ("name", TEXT),
                ("extra", FLOAT8),
                ("id", INT4),
                ("ref", UUID),
            ],
        );
        let retyped = relation(7, &[("ref", UUID), ("id", TEXT), ("name", TEXT)]);
        let changes = feed(
            &mut decoder,
            vec![
                other,
                Message::Insert {
                    relation: 8,
                    new: vec![Field::Text("1")],
                },
                zones(),
                reordered,
                Message::Insert {
                    relation: 7,
                    new: vec![
                        Field::Text("Bronx"),
                        Field::Null,
                        Field::Text("1"),
                        Field::Null,
                    ],
                },
                retyped,
            ],
        )
        .unwrap();
        assert_eq!(
            changes,
            [
                Change::Insert {
                    table: id("zones"),
                    row: vec![int(1), text("Bronx")]
                },
                Change::Insert {
                    table: id("visits"),
                    row: vec![text("Bronx")]
                },
                Change::Unfit {
                    table: id("zones"),
                    drift: Drift::Altered,
                },
            ]
        );

        let lost = relation(7, &[("ref", UUID), ("name", TEXT)]);
        let unreadable = relation(7, &[("ref", UUID), ("id", FLOAT8), ("name", TEXT)]);
        for (message, expected) in [
            (
                lost,
                "zones: column id of public.zones was dropped or changed type",
            ),
            (
                unreadable,
                "zones: column id of public.zones was dropped or changed type",
            ),
        ] {
            let error = zones_decoder().feed(&message).unwrap_err().to_string();
            assert_eq!(error, expected);
        }
    }

    /// The limit counts a transaction's rows, each once however many
    /// facts read its table: each insert here is a change to both facts.
    #[test]
    fn a_transaction_past_the_cap_stops_the_stream() {
        // LIMIT inserts are LIMIT rows: fine, twice over.
        let mut decoder = zones_decoder();
        for _ in 0..2 {
            let changes = feed(&mut decoder, zones_and_inserts(LIMIT)).unwrap();
            assert_eq!(changes.len(), 2 * LIMIT);
        }
        // One more insert is one row past the limit, in the transaction
        // that commits at 9.
        let mut decoder = zones_decoder();
        let error = feed(&mut decoder, zones_and_inserts(LIMIT + 1)).unwrap_err();
        assert_past_the_limit(&error);
        assert!(decoder.in_transaction());
    }

    fn assert_past_the_limit(error: &Error) {
        assert!(
            matches!(
                error,
                Error::Transaction {
                    lsn: Lsn(9),
                    limit: LIMIT
                }
            ),
            "{error:?}"
        );
    }

    /// The zones relation, then an insert of each of `names`.
    fn inserts_named(names: &[String]) -> Vec<Message<'_>> {
        let mut messages = vec![zones()];
        for name in names {
            messages.push(Message::Insert {
                relation: 7,
                new: row(Field::Text("1"), Field::Text(name)),
            });
        }
        messages
    }

    /// The bytes `changes` hold, as the byte limit counts them.
    fn bytes_of(changes: &[Change]) -> usize {
        changes.iter().map(footprint).sum()
    }

    /// A transaction whose changes hold exactly the byte limit commits,
    /// and the count starts again at each commit.
    #[test]
    fn a_transaction_at_the_byte_limit_commits() {
        let changes = feed(&mut zones_decoder(), zones_and_inserts(3)).unwrap();
        let mut decoder = zones_decoder();
        decoder.transaction.byte_limit = bytes_of(&changes);
        for _ in 0..2 {
            assert_eq!(feed(&mut decoder, zones_and_inserts(3)).unwrap(), changes);
        }
    }

    /// One byte under what a transaction's changes hold stops the stream,
    /// in the transaction that commits at 9.
    #[test]
    fn a_transaction_past_the_byte_limit_stops_the_stream() {
        let changes = feed(&mut zones_decoder(), zones_and_inserts(3)).unwrap();
        let limit = bytes_of(&changes) - 1;
        let mut decoder = zones_decoder();
        decoder.transaction.byte_limit = limit;
        let error = feed(&mut decoder, zones_and_inserts(3)).unwrap_err();
        assert!(
            matches!(error, Error::TransactionMemory { lsn: Lsn(9), limit: held } if held == limit),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!(
                "transaction 0/9 holds more than {limit} bytes of rows Straum reads, \
                 the --max-stream-memory limit"
            )
        );
    }

    /// Random transactions of random text through one decoder, against a
    /// reference that sums each transaction's changes alone: each commits
    /// when the sum is at the limit or under it, and stops the stream
    /// past it.
    #[test]
    fn the_byte_limit_holds_each_transaction_alone() {
        for seed in 0..200 {
            let mut rng = Rng(seed);
            let limit = 200 + rng.below_usize(2_000);
            let mut decoder = zones_decoder();
            decoder.transaction.byte_limit = limit;
            let mut reference = zones_decoder();
            for _ in 0..20 {
                let mut names = Vec::new();
                for _ in 0..rng.below_usize(4) {
                    names.push("x".repeat(rng.below_usize(300)));
                }
                let expected = feed(&mut reference, inserts_named(&names)).unwrap();
                let bytes = bytes_of(&expected);
                match feed(&mut decoder, inserts_named(&names)) {
                    Ok(changes) => {
                        assert!(bytes <= limit, "seed {seed}: {bytes} of {limit} committed");
                        assert_eq!(changes, expected, "seed {seed}");
                    }
                    Err(Error::TransactionMemory { .. }) => {
                        assert!(bytes > limit, "seed {seed}: {bytes} of {limit} stopped");
                        break;
                    }
                    Err(error) => panic!("seed {seed}: {error}"),
                }
            }
        }
    }

    /// `pg_class.oid` of `public.boroughs`.
    const BOROUGHS_OID: u32 = 8;
    /// `pg_class.oid` of `public.other`, a table outside the spec.
    const OTHER_OID: u32 = 9;

    /// `zones_decoder`, and the dim `boroughs` reading `public.boroughs`.
    fn zones_and_boroughs_decoder() -> Decoder {
        let mut decoder = zones_decoder();
        decoder.tables.push(Checked {
            oid: BOROUGHS_OID,
            ..Checked::public("boroughs", "boroughs", &["id"], vec![ColumnType::Int])
        });
        decoder
    }

    /// A relation `public.<name>` of one `int4` column, `id`.
    fn one_column(id: u32, name: &str) -> Message<'static> {
        named_relation(id, name, &[("id", INT4)])
    }

    /// `zones` inserts into `public.zones`, `boroughs` into
    /// `public.boroughs` and LIMIT into `public.other`, with their
    /// relations.
    fn inserts_into_three_tables(zones: usize, boroughs: usize) -> Vec<Message<'static>> {
        let mut messages = zones_and_inserts(zones);
        messages.push(one_column(BOROUGHS_OID, "boroughs"));
        messages.push(one_column(OTHER_OID, "other"));
        for _ in 0..boroughs {
            messages.push(Message::Insert {
                relation: BOROUGHS_OID,
                new: vec![Field::Text("1")],
            });
        }
        for _ in 0..LIMIT {
            messages.push(Message::Insert {
                relation: OTHER_OID,
                new: vec![Field::Text("1")],
            });
        }
        messages
    }

    /// The count is per transaction, summed over every table of the spec
    /// it changes; a row of a table outside the spec counts nothing.
    #[test]
    fn the_cap_sums_the_rows_of_every_table() {
        let zones = LIMIT / 2;
        let boroughs = LIMIT - zones;
        let mut decoder = zones_and_boroughs_decoder();
        let changes = feed(&mut decoder, inserts_into_three_tables(zones, boroughs)).unwrap();
        assert_eq!(changes.len(), 2 * zones + boroughs);
        let mut decoder = zones_and_boroughs_decoder();
        let error = feed(&mut decoder, inserts_into_three_tables(zones, boroughs + 1));
        assert_past_the_limit(&error.unwrap_err());
    }

    #[test]
    fn tuples_that_dont_fit() {
        let cases = [
            (
                Message::Insert {
                    relation: 7,
                    new: vec![Field::Null],
                },
                "PostgreSQL sent a tuple of 1 columns for a relation of 3",
            ),
            (
                Message::Delete {
                    relation: 7,
                    old: pgoutput::Old::Key(vec![Field::Null; 1]),
                },
                "PostgreSQL sent a tuple of 1 columns for a relation of 3",
            ),
            (
                Message::Insert {
                    relation: 7,
                    new: row(Field::Text("x"), Field::Null),
                },
                r#"can't read "x" as an integer in column id of public.zones"#,
            ),
        ];
        for (message, expected) in cases {
            let mut decoder = zones_decoder();
            decoder.feed(&zones()).unwrap();
            assert_eq!(decoder.feed(&message).unwrap_err().to_string(), expected);
        }
    }
}
