//! pgoutput messages turned into commits of fact and dim rows. pgoutput is
//! PostgreSQL's logical decoding output: a Relation message describes a
//! table (a relation, PostgreSQL's word), then Begin, row changes and
//! Commit follow. `pgoutput.rs` parses the bytes; the `Decoder` here keeps
//! where each spec table's columns sit and hands `stream.rs` whole
//! `Commit`s. The physical stream feeds it the same messages.

use std::collections::HashMap;
use std::{mem, slice};

use straum_app::{Change, Commit, Lsn};
use straum_domain::{ColumnType, Datum, Identifier};

use crate::catalog::Checked;
use crate::error::Error;
use crate::pgoutput::{self, Field, Message, Relation, RelationColumn, Tuple};
use crate::problem::{ColumnProblem, Problem, TableNames};
use crate::value::{Oid, read, type_of};

/// Turns pgoutput messages into commits of rows, each as the stream brings
/// it: `Fact::columns` or `Dim::columns`.
pub(crate) struct Decoder {
    /// Each with its types as last loaded or announced. A Relation that
    /// changes them changes what rows read as, so Straum starts over.
    tables: Vec<Checked>,
    /// By relation id. A table read by two members of the spec maps to
    /// both.
    relations: HashMap<Oid, Vec<Mapping>>,
    transaction: Transaction,
}

/// The transaction so far, held until its Commit; `limit` bounds it,
/// since every change here is applied under the write lock.
struct Transaction {
    changes: Vec<Change>,
    limit: usize,
    /// The transaction being read, by the LSN of its commit record.
    lsn: Option<Lsn>,
}

/// Where a table's columns sit in a relation's tuples.
struct Mapping {
    table: TableIndex,
    /// For each column the table reads, its place in the relation's tuples.
    positions: Vec<usize>,
    types: Vec<ColumnType>,
    /// How many columns the relation's tuples have.
    width: usize,
}

/// A table's place in `Decoder::tables`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TableIndex(usize);

impl Decoder {
    /// `tables` are the spec's tables, dims and facts; `limit` is the
    /// most changes to them one transaction may hold.
    pub(crate) fn new(tables: Vec<Checked>, limit: usize) -> Decoder {
        Decoder {
            tables,
            relations: HashMap::new(),
            transaction: Transaction {
                changes: Vec::new(),
                limit,
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
            for mapping in self.relations.get(&relation).into_iter().flatten() {
                let table = &self.tables[mapping.table.0];
                let change = match mapping.change(table, message)? {
                    Some(change) => change,
                    None => unfit(table),
                };
                self.transaction.hold(change)?;
            }
        }
        Ok(None)
    }

    /// Every table of the spec `relation` holds no longer reads as
    /// loaded, though no message says how: the physical stream's tables
    /// rewritten or dropped. Between a Begin and its Commit.
    pub(crate) fn unfit(&mut self, relation: Oid) {
        for mapping in self.relations.get(&relation).into_iter().flatten() {
            self.transaction
                .changes
                .push(unfit(&self.tables[mapping.table.0]));
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
    /// what is wrong.
    fn relation(&mut self, relation: &Relation) -> Result<(), Error> {
        let mut mappings = Vec::new();
        for (index, table) in self.tables.iter_mut().enumerate() {
            if table.schema.as_str() != relation.schema
                || table.table_name.as_str() != relation.name
            {
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
                self.transaction.changes.push(unfit(table));
                table.types.clone_from(&types);
            }
            mappings.push(Mapping {
                table: TableIndex(index),
                positions,
                types,
                width: relation.columns.len(),
            });
        }
        self.relations.insert(relation.id, mappings);
        Ok(())
    }
}

impl Transaction {
    /// Holds a change for the commit, or stops at the limit. The count is per
    /// transaction, not per table: the lock is held for all of it at once.
    fn hold(&mut self, change: Change) -> Result<(), Error> {
        if self.changes.len() >= self.limit {
            return Err(Error::Transaction {
                lsn: self.lsn.unwrap_or(Lsn(0)),
                limit: self.limit,
            });
        }
        self.changes.push(change);
        Ok(())
    }
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
        table: TableNames {
            name: table.name.clone(),
            qualified: table.qualified(),
        },
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

fn unfit(table: &Checked) -> Change {
    Change::Unfit {
        table: table.name.clone(),
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
        for (index, (&position, column_type)) in self.positions.iter().zip(&self.types).enumerate()
        {
            let datum = match (&tuple[position], old) {
                (Field::Null, _) => Datum::Null,
                (Field::Text(text), _) => read(*column_type, text)
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
    /// no longer has `REPLICA IDENTITY FULL`, so Straum starts over and the
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
        Err(Error::Protocol(format!(
            "a tuple of {} columns for a relation of {}",
            tuple.len(),
            self.width
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::pgoutput::RelationColumn;

    const INT4: u32 = 23;
    const TEXT: u32 = 25;
    const UUID: u32 = 2950;
    const FLOAT8: u32 = 701;
    /// Changes a transaction may hold in these tests; the longest feeds ten.
    const LIMIT: usize = 16;

    fn id(name: &str) -> Identifier {
        Identifier::new(name).unwrap()
    }

    fn unfit_named(name: &str) -> Change {
        Change::Unfit { table: id(name) }
    }

    /// Two facts read `public.zones`: `zones`, reading `id` and `name`, and
    /// `visits`, reading `name` alone.
    fn zones_decoder() -> Decoder {
        Decoder::new(
            vec![
                Checked::public(
                    "zones",
                    "zones",
                    &["id", "name"],
                    vec![ColumnType::Int, ColumnType::Text],
                ),
                Checked::public("visits", "zones", &["name"], vec![ColumnType::Text]),
            ],
            LIMIT,
        )
    }

    fn relation(id: u32, columns: &[(&str, u32)]) -> Message<'static> {
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
            name: "zones".into(),
            columns: relation_columns,
        })
    }

    fn zones() -> Message<'static> {
        relation(7, &[("ref", UUID), ("id", INT4), ("name", TEXT)])
    }

    fn row(id: Field<'static>, name: Field<'static>) -> Tuple<'static> {
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
            enums: vec![777],
            ..Checked::public("moods", "zones", &["mood"], vec![ColumnType::Text])
        };
        let mut decoder = Decoder::new(vec![moods], LIMIT);
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
        decoder.unfit(8);
        decoder.unfit(7);
        let got = decoder.feed(&commit).unwrap().unwrap();
        assert_eq!(got.changes, [unfit_named("zones"), unfit_named("visits")]);
    }

    #[test]
    fn relations_that_change() {
        let mut decoder = zones_decoder();
        // Not in the spec: its changes are skipped.
        let mut other = relation(8, &[("x", INT4)]);
        if let Message::Relation(relation) = &mut other {
            relation.name = "other".into();
        }
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
                Change::Unfit { table: id("zones") },
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

    /// The limit counts a transaction's changes to every fact together: each
    /// insert here is a change to both facts.
    #[test]
    fn a_transaction_past_the_cap_stops_the_stream() {
        // LIMIT / 2 inserts make LIMIT changes: fine, twice over.
        let mut decoder = zones_decoder();
        for _ in 0..2 {
            let changes = feed(&mut decoder, zones_and_inserts(LIMIT / 2)).unwrap();
            assert_eq!(changes.len(), LIMIT);
        }
        // One more insert is one change past the limit, in the transaction
        // that commits at 9.
        let mut decoder = zones_decoder();
        let error = feed(&mut decoder, zones_and_inserts(LIMIT / 2 + 1)).unwrap_err();
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
        assert!(decoder.in_transaction());
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
