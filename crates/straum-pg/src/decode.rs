//! pgoutput messages turned into commits of spec rows: which spec tables a
//! relation holds, where their columns sit, and how its values read.

use std::collections::HashMap;
use std::mem;

use straum_app::{Change, Commit, Lsn, Old};
use straum_domain::{Datum, Ident};

use crate::catalog::Table;
use crate::error::Error;
use crate::pgoutput::{self, Field, Message, Relation, Tuple};
use crate::value::{read, type_of};
use straum_domain::ColumnType;

/// Turns pgoutput messages into commits of spec rows.
pub(crate) struct Decoder {
    tables: Vec<Table>,
    /// By relation id. A table read by two facts, or a fact and a dim, maps
    /// to both.
    relations: HashMap<u32, Vec<Mapping>>,
    /// Each spec table's kinds as last loaded or announced. A Relation that
    /// changes them changes what rows read as, so straum starts over.
    kinds: HashMap<Ident, Vec<ColumnType>>,
    changes: Vec<Change>,
    in_transaction: bool,
}

/// Where a spec table's columns sit in a relation's tuples.
struct Mapping {
    table: Ident,
    positions: Vec<usize>,
    kinds: Vec<ColumnType>,
    key: usize,
    key_kind: ColumnType,
    width: usize,
}

impl Decoder {
    pub(crate) fn new(tables: Vec<Table>) -> Decoder {
        let kinds = tables
            .iter()
            .map(|t| (t.name.clone(), t.kinds.clone()))
            .collect();
        Decoder {
            tables,
            relations: HashMap::new(),
            kinds,
            changes: Vec::new(),
            in_transaction: false,
        }
    }

    /// A commit once its Commit message comes.
    pub(crate) fn feed(&mut self, message: Message) -> Result<Option<Commit>, Error> {
        match message {
            Message::Begin => self.in_transaction = true,
            Message::Commit { end_lsn, time } => {
                self.in_transaction = false;
                let changes = mem::take(&mut self.changes);
                return Ok(Some(Commit {
                    lsn: Lsn(end_lsn),
                    time,
                    changes,
                }));
            }
            Message::Relation(relation) => self.relation(&relation)?,
            Message::Insert { relation, new } => {
                for m in mappings(&self.relations, relation) {
                    let change = match m.row(&new, None)? {
                        Some(row) => Change::Insert {
                            table: m.table.clone(),
                            row,
                        },
                        None => altered(m),
                    };
                    self.changes.push(change);
                }
            }
            Message::Update { relation, old, new } => {
                for m in mappings(&self.relations, relation) {
                    let old = match &old {
                        Some(pgoutput::Old::Row(tuple)) => m.row(tuple, None)?.map(Old::Row),
                        Some(pgoutput::Old::Key(tuple)) => Some(Old::Key(m.key(tuple)?)),
                        None => Some(Old::SameKey),
                    };
                    let fallback = match &old {
                        Some(Old::Row(row)) => Some(row.as_slice()),
                        _ => None,
                    };
                    let change = match (m.row(&new, fallback)?, old) {
                        (Some(new), Some(old)) => Change::Update {
                            table: m.table.clone(),
                            old,
                            new,
                        },
                        _ => altered(m),
                    };
                    self.changes.push(change);
                }
            }
            Message::Delete { relation, old } => {
                for m in mappings(&self.relations, relation) {
                    let old = match &old {
                        pgoutput::Old::Row(tuple) => m.row(tuple, None)?.map(Old::Row),
                        pgoutput::Old::Key(tuple) => Some(Old::Key(m.key(tuple)?)),
                    };
                    let change = match old {
                        Some(old) => Change::Delete {
                            table: m.table.clone(),
                            old,
                        },
                        None => altered(m),
                    };
                    self.changes.push(change);
                }
            }
            Message::Truncate { relations } => {
                for relation in relations {
                    for m in mappings(&self.relations, relation) {
                        self.changes.push(Change::Truncate {
                            table: m.table.clone(),
                        });
                    }
                }
            }
            Message::Other => {}
        }
        Ok(None)
    }

    /// Between a Begin and its Commit.
    pub(crate) fn in_transaction(&self) -> bool {
        self.in_transaction
    }

    /// Maps the relation's columns for each spec table it holds. A table
    /// whose columns now read differently is read again; one that lost a
    /// column, or whose column turned to a type straum doesn't read, stops the
    /// stream, and the setup check on the next start says what is wrong.
    fn relation(&mut self, relation: &Relation) -> Result<(), Error> {
        let mut mappings = Vec::new();
        let tables = self
            .tables
            .iter()
            .filter(|t| t.schema.as_str() == relation.schema && t.table.as_str() == relation.name);
        for table in tables {
            let position = |column: &Ident| {
                relation
                    .columns
                    .iter()
                    .position(|c| c.name == column.as_str())
                    .ok_or_else(|| changed(table, column))
            };
            let mut positions = Vec::new();
            let mut kinds = Vec::new();
            for column in &table.columns {
                let i = position(column)?;
                let c = &relation.columns[i];
                positions.push(i);
                kinds.push(type_of(c.type_oid, c.typmod).ok_or_else(|| changed(table, column))?);
            }
            let key = position(&table.key)?;
            let key_kind = match table.columns.iter().position(|c| *c == table.key) {
                Some(i) => kinds[i],
                None => {
                    let c = &relation.columns[key];
                    type_of(c.type_oid, c.typmod).unwrap_or(ColumnType::Text)
                }
            };
            let known = self.kinds.insert(table.name.clone(), kinds.clone());
            if known.as_ref() != Some(&kinds) {
                self.changes.push(Change::Altered {
                    table: table.name.clone(),
                });
            }
            mappings.push(Mapping {
                table: table.name.clone(),
                positions,
                kinds,
                key,
                key_kind,
                width: relation.columns.len(),
            });
        }
        self.relations.insert(relation.id, mappings);
        Ok(())
    }
}

/// The spec tables a relation holds; none for a relation straum doesn't read.
fn mappings(relations: &HashMap<u32, Vec<Mapping>>, relation: u32) -> &[Mapping] {
    relations.get(&relation).map_or(&[], Vec::as_slice)
}

fn changed(table: &Table, column: &Ident) -> Error {
    Error::Setup(vec![format!(
        "{}: column {column} of {} was dropped or changed type",
        table.name,
        table.qualified()
    )])
}

fn altered(m: &Mapping) -> Change {
    Change::Altered {
        table: m.table.clone(),
    }
}

impl Mapping {
    /// The spec row a tuple holds. A value PostgreSQL left out because the
    /// change kept it comes from `old`; `None` when there is no old row to
    /// take it from.
    fn row(&self, tuple: &Tuple, old: Option<&[Datum]>) -> Result<Option<Vec<Datum>>, Error> {
        self.fits(tuple)?;
        let mut row = Vec::with_capacity(self.positions.len());
        for (i, (&position, kind)) in self.positions.iter().zip(&self.kinds).enumerate() {
            let datum = match (tuple[position], old) {
                (Field::Null, _) => Datum::Null,
                (Field::Text(text), _) => read(*kind, text)?,
                (Field::Unchanged, Some(old)) => old[i].clone(),
                (Field::Unchanged, None) => return Ok(None),
            };
            row.push(datum);
        }
        Ok(Some(row))
    }

    fn key(&self, tuple: &Tuple) -> Result<Datum, Error> {
        self.fits(tuple)?;
        match tuple[self.key] {
            Field::Text(text) => Ok(read(self.key_kind, text)?),
            Field::Null | Field::Unchanged => {
                Err(Error::Protocol("an old key without a value".into()))
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
    use crate::pgoutput::Column;

    const INT4: u32 = 23;
    const TEXT: u32 = 25;
    const UUID: u32 = 2950;
    const FLOAT8: u32 = 701;

    fn id(s: &str) -> Ident {
        Ident::new(s).unwrap()
    }

    /// The spec reads `public.zones` twice: as dim `zones`, keyed on `id`,
    /// and as fact `visits`, keyed on `ref`.
    fn decoder() -> Decoder {
        let table =
            |name: &str, columns: &[&str], kinds: Vec<ColumnType>, key: &str, key_kind| Table {
                name: id(name),
                schema: id("public"),
                table: id("zones"),
                columns: columns.iter().map(|c| id(c)).collect(),
                kinds,
                key: id(key),
                key_kind,
            };
        Decoder::new(vec![
            table(
                "zones",
                &["id", "name"],
                vec![ColumnType::Int, ColumnType::Text],
                "id",
                ColumnType::Int,
            ),
            table(
                "visits",
                &["name"],
                vec![ColumnType::Text],
                "ref",
                ColumnType::Text,
            ),
        ])
    }

    fn relation(id: u32, columns: &[(&str, u32)]) -> Message<'static> {
        Message::Relation(Relation {
            id,
            schema: "public".into(),
            name: "zones".into(),
            columns: columns
                .iter()
                .map(|&(name, type_oid)| Column {
                    name: name.into(),
                    type_oid,
                    typmod: -1,
                })
                .collect(),
        })
    }

    fn zones() -> Message<'static> {
        relation(7, &[("ref", UUID), ("id", INT4), ("name", TEXT)])
    }

    fn row(id: Field<'static>, name: Field<'static>) -> Tuple<'static> {
        vec![Field::Text("r-1"), id, name]
    }

    fn feed(decoder: &mut Decoder, messages: Vec<Message>) -> Result<Vec<Change>, Error> {
        decoder.feed(Message::Begin)?;
        for m in messages {
            decoder.feed(m)?;
        }
        let time = UNIX_EPOCH + Duration::from_secs(9);
        let commit = (decoder.feed(Message::Commit { end_lsn: 9, time }))?.unwrap();
        assert_eq!((commit.lsn, commit.time), (Lsn(9), time));
        Ok(commit.changes)
    }

    fn int(n: i64) -> Datum {
        Datum::Int(n)
    }

    fn text(s: &str) -> Datum {
        Datum::Text(s.into())
    }

    #[test]
    fn rows_for_each_spec_table() {
        let mut d = decoder();
        let (z, v) = (id("zones"), id("visits"));
        let changes = feed(
            &mut d,
            vec![
                zones(),
                Message::Insert {
                    relation: 7,
                    new: row(Field::Text("1"), Field::Text("Bronx")),
                },
                Message::Update {
                    relation: 7,
                    old: Some(pgoutput::Old::Row(row(
                        Field::Text("1"),
                        Field::Text("Bronx"),
                    ))),
                    new: row(Field::Text("1"), Field::Unchanged),
                },
                Message::Update {
                    relation: 7,
                    old: None,
                    new: row(Field::Text("1"), Field::Null),
                },
                Message::Delete {
                    relation: 7,
                    old: pgoutput::Old::Key(row(Field::Text("1"), Field::Null)),
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
                    table: z.clone(),
                    row: bronx.clone()
                },
                Change::Insert {
                    table: v.clone(),
                    row: vec![text("Bronx")]
                },
                Change::Update {
                    table: z.clone(),
                    old: Old::Row(bronx.clone()),
                    new: bronx
                },
                Change::Update {
                    table: v.clone(),
                    old: Old::Row(vec![text("Bronx")]),
                    new: vec![text("Bronx")]
                },
                Change::Update {
                    table: z.clone(),
                    old: Old::SameKey,
                    new: vec![int(1), Datum::Null]
                },
                Change::Update {
                    table: v.clone(),
                    old: Old::SameKey,
                    new: vec![Datum::Null]
                },
                Change::Delete {
                    table: z.clone(),
                    old: Old::Key(int(1))
                },
                // A fact's key of a type straum doesn't read comes as text.
                Change::Delete {
                    table: v.clone(),
                    old: Old::Key(text("r-1"))
                },
                Change::Truncate { table: z },
                Change::Truncate { table: v },
            ]
        );
    }

    #[test]
    fn values_left_out_read_the_table_again() {
        let mut d = decoder();
        let unchanged = || row(Field::Text("1"), Field::Unchanged);
        let changes = feed(
            &mut d,
            vec![
                zones(),
                Message::Insert {
                    relation: 7,
                    new: unchanged(),
                },
                Message::Update {
                    relation: 7,
                    old: Some(pgoutput::Old::Row(unchanged())),
                    new: unchanged(),
                },
                Message::Update {
                    relation: 7,
                    old: Some(pgoutput::Old::Key(row(Field::Text("2"), Field::Null))),
                    new: unchanged(),
                },
                Message::Delete {
                    relation: 7,
                    old: pgoutput::Old::Row(unchanged()),
                },
            ],
        )
        .unwrap();
        let altered = |t: &str| Change::Altered { table: id(t) };
        let pair = [altered("zones"), altered("visits")];
        let expected: Vec<Change> = (0..4).flat_map(|_| pair.clone()).collect();
        assert_eq!(changes, expected);
    }

    #[test]
    fn relations_that_change() {
        let mut d = decoder();
        // Not a spec table: its changes are skipped.
        let mut other = relation(8, &[("x", INT4)]);
        if let Message::Relation(r) = &mut other {
            r.name = "other".into();
        }
        // The same kinds in another order; then a column that reads
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
            &mut d,
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
                Change::Altered { table: id("zones") },
            ]
        );

        let lost = relation(7, &[("ref", UUID), ("name", TEXT)]);
        let unreadable = relation(7, &[("ref", UUID), ("id", FLOAT8), ("name", TEXT)]);
        let no_key = relation(7, &[("id", INT4), ("name", TEXT)]);
        for (message, expected) in [
            (
                lost,
                "zones: column id of public.zones was dropped or changed type",
            ),
            (
                unreadable,
                "zones: column id of public.zones was dropped or changed type",
            ),
            (
                no_key,
                "visits: column ref of public.zones was dropped or changed type",
            ),
        ] {
            let err = decoder().feed(message).unwrap_err().to_string();
            assert_eq!(err, expected);
        }
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
                Message::Delete {
                    relation: 7,
                    old: pgoutput::Old::Key(vec![Field::Null, Field::Null, Field::Null]),
                },
                "PostgreSQL sent an old key without a value",
            ),
            (
                Message::Insert {
                    relation: 7,
                    new: row(Field::Text("x"), Field::Null),
                },
                r#"can't read "x" as an integer"#,
            ),
        ];
        for (message, expected) in cases {
            let mut d = decoder();
            d.feed(zones()).unwrap();
            assert_eq!(d.feed(message).unwrap_err().to_string(), expected);
        }
    }
}
