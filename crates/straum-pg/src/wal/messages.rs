//! Committed transactions as the pgoutput messages a logical slot sends
//! for them, so the physical stream feeds the same [`Decoder`]: `Begin`,
//! `Relation` before a table's first change, `Insert`, `Update` and
//! `Delete` with the whole old row, as under `REPLICA IDENTITY FULL`, and
//! `Commit`.

use std::collections::{HashMap, HashSet};

use straum_app::Commit;
use straum_domain::{Datum, FastMap};

use super::heap::{Change, Tuple};
use super::transactions::Committed;
use super::tuple::{self, Attribute, Value};
use super::xact::Invalidated;
use super::{RelationFile, WalError};
use crate::decode::Decoder;
use crate::error::Error;
use crate::pgoutput::{Field, Message, Old, Relation};
use crate::value::system_time;

/// A table the physical stream reads: its file, as pgoutput announces
/// it, and how its rows lie on its pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Table {
    pub(crate) file: RelationFile,
    /// Its id is the table's OID, as pgoutput's is; its columns leave out
    /// dropped ones, as pgoutput's do.
    pub(crate) relation: Relation,
    /// Every attribute by `attnum`, dropped ones too.
    pub(crate) attributes: Vec<Attribute>,
    /// The attribute each of `relation`'s columns is.
    pub(crate) columns: Vec<usize>,
    /// By attribute: the value of a column added with a default, for the
    /// rows written before it, which don't hold it (`attmissingval`).
    pub(crate) missing: Vec<Datum>,
}

pub(crate) struct Publisher {
    tables: FastMap<RelationFile, Table>,
    /// Enum labels by `pg_enum` OID.
    labels: HashMap<u32, String>,
    /// Tables whose `Relation` the decoder has.
    announced: HashSet<RelationFile>,
    /// Tables rewritten or dropped, by OID, for the next commit to say.
    unfit: Vec<u32>,
}

impl Publisher {
    pub(crate) fn new(tables: Vec<Table>, labels: HashMap<u32, String>) -> Publisher {
        Publisher {
            tables: tables.into_iter().map(|t| (t.file, t)).collect(),
            labels,
            announced: HashSet::new(),
            unfit: Vec::new(),
        }
    }

    pub(crate) fn files(&self) -> HashSet<RelationFile> {
        self.tables.keys().copied().collect()
    }

    /// The OIDs of the tables whose layout a commit with these
    /// invalidations may have changed: read them again, for
    /// [`Publisher::relayout`].
    pub(crate) fn stale(&self, invalidated: &[Invalidated]) -> Vec<u32> {
        let mut stale: Vec<u32> = (self.tables.values())
            .filter(|t| {
                invalidated.iter().any(|i| {
                    i.database == t.file.database
                        && (i.relation == t.relation.id || i.relation == 0)
                })
            })
            .map(|t| t.relation.id)
            .collect();
        stale.sort_unstable();
        stale
    }

    /// Takes the layouts read again for `stale`. A table in another file
    /// now, rewritten or truncated, or gone, no longer reads as loaded:
    /// its pages went with the old file. Any other is announced again
    /// before its next change, as pgoutput does after an invalidation,
    /// and the decoder sees what changed.
    pub(crate) fn relayout(&mut self, stale: &[u32], fresh: Vec<Table>) {
        for &oid in stale {
            let Some(file) = (self.tables.values())
                .find(|t| t.relation.id == oid)
                .map(|t| t.file)
            else {
                continue;
            };
            match fresh.iter().find(|t| t.relation.id == oid) {
                Some(table) if table.file == file => {
                    self.tables.insert(file, table.clone());
                    self.announced.remove(&file);
                }
                _ => self.unfit.push(oid),
            }
        }
    }

    /// Feeds `committed` to `decoder` as messages: the commit it makes.
    /// One that changed no table and made none unfit makes none, as
    /// pgoutput sends nothing for it.
    pub(crate) fn publish(
        &mut self,
        committed: &Committed,
        decoder: &mut Decoder,
    ) -> Result<Option<Commit>, Error> {
        if committed.changes.is_empty() && self.unfit.is_empty() {
            return Ok(None);
        }
        decoder.feed(Message::Begin {
            final_lsn: committed.lsn,
        })?;
        for oid in std::mem::take(&mut self.unfit) {
            // The decoder knows a table by its Relation.
            if let Some(table) = self.tables.values().find(|t| t.relation.id == oid)
                && self.announced.insert(table.file)
            {
                decoder.feed(Message::Relation(table.relation.clone()))?;
            }
            decoder.unfit(oid);
        }
        for change in &committed.changes {
            let file = match change {
                Change::Insert { file, .. }
                | Change::Update { file, .. }
                | Change::Delete { file, .. } => file,
            };
            let table = self
                .tables
                .get(file)
                .ok_or_else(|| problem(committed.lsn, "a change to a table Straum doesn't read"))?;
            if self.announced.insert(*file) {
                decoder.feed(Message::Relation(table.relation.clone()))?;
            }
            let row = |tuple: &Tuple| fields(table, tuple, &self.labels, committed.lsn);
            let relation = table.relation.id;
            decoder.feed(match change {
                Change::Insert { new, .. } => Message::Insert {
                    relation,
                    new: row(new)?,
                },
                Change::Update { old, new, .. } => Message::Update {
                    relation,
                    old: Some(Old::Row(row(old)?)),
                    new: row(new)?,
                },
                Change::Delete { old, .. } => Message::Delete {
                    relation,
                    old: Old::Row(row(old)?),
                },
            })?;
        }
        decoder.feed(Message::Commit {
            end_lsn: committed.end,
            committed: system_time(committed.time),
        })
    }
}

/// The relation's columns of `tuple`, as a message holds them.
fn fields(
    table: &Table,
    tuple: &Tuple,
    labels: &HashMap<u32, String>,
    lsn: u64,
) -> Result<Vec<Field<'static>>, Error> {
    let mut values =
        tuple::values(tuple, &table.attributes, labels).map_err(|p| problem(lsn, p))?;
    Ok(table
        .columns
        .iter()
        .map(
            |&i| match std::mem::replace(&mut values[i], Value::Missing) {
                Value::Datum(datum) => Field::Datum(datum),
                // pgoutput, too, leaves out a value TOASTed out of line that
                // the change kept.
                Value::External => Field::Unchanged,
                Value::Missing => Field::Datum(table.missing[i].clone()),
            },
        )
        .collect())
}

fn problem(lsn: u64, problem: &'static str) -> Error {
    Error::Wal(WalError::At { lsn, problem })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use straum_app::Change as Applied;
    use straum_domain::{ColumnType, Datum, Identifier};

    use super::*;
    use crate::catalog::Checked;
    use crate::pgoutput::Column;
    use crate::value::{INT4, INT8, TEXT};
    use crate::wal::tuple::Kind;

    const INT: Kind = Kind::Read(ColumnType::Int);

    const FILE: RelationFile = RelationFile {
        tablespace: 1663,
        database: 5,
        relation: 16_384,
    };

    /// `public.zones`: `id int4`, a dropped `int4`, `name text`, `p
    /// point`, and `late int4`, added after the rows written here.
    fn table() -> Table {
        let attribute = |length, align, kind| Attribute {
            length,
            align,
            kind,
        };
        let column = |name: &str, type_oid| Column {
            name: name.to_owned(),
            type_oid,
            typmod: -1,
        };
        Table {
            file: FILE,
            relation: Relation {
                id: 77,
                schema: "public".to_owned(),
                name: "zones".to_owned(),
                columns: vec![
                    column("id", INT4),
                    column("name", TEXT),
                    column("p", 600),
                    column("late", INT4),
                ],
            },
            attributes: vec![
                attribute(4, 4, INT),
                attribute(4, 4, Kind::Other),
                attribute(-1, 4, Kind::Read(ColumnType::Text)),
                attribute(16, 8, Kind::Other),
                attribute(4, 4, INT),
            ],
            columns: vec![0, 2, 3, 4],
            missing: vec![Datum::Null; 5],
        }
    }

    fn decoder() -> Decoder {
        let checked = Checked::public(
            "zones",
            "zones",
            &["id", "name"],
            vec![ColumnType::Int, ColumnType::Text],
        );
        Decoder::new(vec![checked], 10)
    }

    /// A row of the first four attributes: `name` as given, its varlena
    /// header included.
    fn row(id: i32, name: &[u8]) -> Tuple {
        let mut body = vec![0];
        body.extend(id.to_le_bytes());
        body.extend(9i32.to_le_bytes());
        body.extend(name);
        body.resize(body.len().next_multiple_of(8) + 1, 0);
        body.extend([1; 16]);
        Tuple {
            infomask2: 4,
            infomask: 0,
            hoff: 24,
            body: body.into(),
        }
    }

    fn short(text: &str) -> Vec<u8> {
        let mut bytes = vec![u8::try_from((text.len() + 1) << 1 | 1).unwrap()];
        bytes.extend(text.as_bytes());
        bytes
    }

    fn committed(changes: Vec<Change>) -> Committed {
        Committed {
            xid: 700,
            lsn: 0x100,
            end: 0x140,
            time: 1_000_000,
            changes,
            invalidated: Vec::new(),
        }
    }

    fn invalidating(database: u32, relation: u32) -> Committed {
        Committed {
            invalidated: vec![Invalidated { database, relation }],
            ..committed(Vec::new())
        }
    }

    fn publish(changes: Vec<Change>) -> Result<Option<straum_app::Commit>, Error> {
        let mut publisher = Publisher::new(vec![table()], HashMap::new());
        publisher.publish(&committed(changes), &mut decoder())
    }

    fn zones() -> Identifier {
        Identifier::new("zones").unwrap()
    }

    #[test]
    fn publishes_changes_as_a_logical_slot_does() {
        let a = row(1, &short("a"));
        let b = row(1, &short("b"));
        let changes = vec![
            Change::Insert {
                file: FILE,
                new: a.clone(),
            },
            Change::Update {
                file: FILE,
                old: a.clone(),
                new: b.clone(),
            },
            Change::Delete { file: FILE, old: b },
        ];
        let commit = publish(changes).unwrap().unwrap();
        assert_eq!(commit.lsn.0, 0x140);
        assert_eq!(commit.committed, system_time(1_000_000));
        let row = |name: &str| vec![Datum::Int(1), Datum::Text(name.into())];
        assert_eq!(
            commit.changes,
            [
                Applied::Insert {
                    table: zones(),
                    row: row("a"),
                },
                Applied::Update {
                    table: zones(),
                    old: row("a"),
                    new: row("b"),
                },
                Applied::Delete {
                    table: zones(),
                    old: row("b"),
                },
            ]
        );
        assert_eq!(
            Publisher::new(vec![table()], HashMap::new()).files(),
            [FILE].into()
        );
    }

    #[test]
    fn leaves_out_values_toasted_out_of_line() {
        let mut pointer = vec![0x01, 18];
        pointer.extend([0; 16]);
        let toasted = row(1, &pointer);
        let changes = vec![Change::Update {
            file: FILE,
            old: toasted.clone(),
            new: toasted,
        }];
        let commit = publish(changes).unwrap().unwrap();
        // Unchanged in the old row too: there is nothing to take it from.
        assert!(
            matches!(commit.changes[..], [Applied::Unfit { .. }]),
            "{commit:?}"
        );
    }

    #[test]
    fn rejects_what_it_cant_publish() {
        let message =
            |result: Result<Option<straum_app::Commit>, Error>| result.unwrap_err().to_string();
        let other = RelationFile {
            relation: 1,
            ..FILE
        };
        let insert = |file, new| vec![Change::Insert { file, new }];
        assert_eq!(
            message(publish(insert(other, row(1, &short("a"))))),
            "WAL at 0/100 has a change to a table Straum doesn't read"
        );
        let mut cut = row(1, &short("a"));
        cut.body = cut.body[..6].into();
        assert_eq!(
            message(publish(insert(FILE, cut))),
            "WAL at 0/100 has a row shorter than its columns"
        );
        // A row from before `late` was added holds no value for it; the
        // catalog's default is the value, which Straum doesn't read.
    }

    #[test]
    fn reads_a_column_added_without_a_default_as_null() {
        let mut publisher = Publisher::new(vec![table()], HashMap::new());
        let changes = vec![Change::Insert {
            file: FILE,
            new: row(2, &short("z")),
        }];
        let commit = publisher.publish(&committed(changes), &mut decoder());
        let commit = commit.unwrap().unwrap();
        let row = vec![Datum::Int(2), Datum::Text("z".into())];
        assert_eq!(
            commit.changes,
            [Applied::Insert {
                table: zones(),
                row,
            }]
        );
    }

    #[test]
    fn reads_the_default_of_a_column_added_later() {
        let mut table = table();
        table.missing[4] = Datum::Int(7);
        table.relation.columns[3].name = "late".to_owned();
        let checked = Checked::public(
            "zones",
            "zones",
            &["id", "late"],
            vec![ColumnType::Int, ColumnType::Int],
        );
        let mut decoder = Decoder::new(vec![checked], 10);
        let mut publisher = Publisher::new(vec![table], HashMap::new());
        let changes = vec![Change::Insert {
            file: FILE,
            new: row(3, &short("c")),
        }];
        let commit = publisher.publish(&committed(changes), &mut decoder);
        let row = vec![Datum::Int(3), Datum::Int(7)];
        assert_eq!(
            commit.unwrap().unwrap().changes,
            [Applied::Insert {
                table: zones(),
                row,
            }]
        );
    }

    #[test]
    fn finds_the_tables_a_commit_invalidates() {
        let publisher = Publisher::new(vec![table()], HashMap::new());
        assert_eq!(publisher.stale(&invalidating(5, 77).invalidated), [77]);
        assert_eq!(publisher.stale(&invalidating(5, 0).invalidated), [77]);
        assert_eq!(
            publisher.stale(&invalidating(5, 78).invalidated),
            Vec::<u32>::new()
        );
        assert_eq!(
            publisher.stale(&invalidating(6, 77).invalidated),
            Vec::<u32>::new()
        );
        assert_eq!(
            publisher.stale(&committed(Vec::new()).invalidated),
            Vec::<u32>::new()
        );
    }

    #[test]
    fn announces_a_table_again_after_it_changes() {
        let mut publisher = Publisher::new(vec![table()], HashMap::new());
        let mut decoder = decoder();
        let insert = || {
            committed(vec![Change::Insert {
                file: FILE,
                new: row(1, &short("a")),
            }])
        };
        publisher.publish(&insert(), &mut decoder).unwrap();
        // Nothing changed for the decoder: rows read on.
        publisher.relayout(&[77], vec![table()]);
        let commit = publisher.publish(&insert(), &mut decoder).unwrap().unwrap();
        assert!(matches!(commit.changes[..], [Applied::Insert { .. }]));
        // `name` now reads as another type: the decoder says unfit.
        let mut retyped = table();
        retyped.relation.columns[1].type_oid = INT8;
        retyped.attributes[2] = Attribute {
            length: 8,
            align: 8,
            kind: INT,
        };
        publisher.relayout(&[77], vec![retyped]);
        let mut changes = Vec::new();
        let mut body = vec![0];
        body.extend(1i32.to_le_bytes());
        body.extend([0; 4]);
        body.extend(5i64.to_le_bytes());
        body.extend([1; 16]);
        changes.push(Change::Insert {
            file: FILE,
            new: Tuple {
                infomask2: 4,
                infomask: 0,
                hoff: 24,
                body: body.into(),
            },
        });
        let commit = publisher.publish(&committed(changes), &mut decoder);
        let commit = commit.unwrap().unwrap();
        assert!(
            matches!(commit.changes[0], Applied::Unfit { .. }),
            "{commit:?}"
        );
        // An OID it doesn't read: nothing.
        publisher.relayout(&[78], Vec::new());
        assert_eq!(
            publisher
                .publish(&committed(Vec::new()), &mut decoder)
                .unwrap(),
            None
        );
    }

    #[test]
    fn a_table_rewritten_or_dropped_is_unfit() {
        let rewritten = Table {
            file: RelationFile {
                relation: 16_999,
                ..FILE
            },
            ..table()
        };
        for fresh in [vec![rewritten], Vec::new()] {
            let mut publisher = Publisher::new(vec![table()], HashMap::new());
            let mut decoder = decoder();
            publisher.relayout(&[77], fresh);
            // Said even by a commit that changed no row.
            let commit = publisher.publish(&invalidating(5, 77), &mut decoder);
            let commit = commit.unwrap().unwrap();
            assert_eq!(commit.changes, [Applied::Unfit { table: zones() }]);
            // Said once.
            assert_eq!(
                publisher
                    .publish(&invalidating(5, 77), &mut decoder)
                    .unwrap(),
                None
            );
        }
    }
}
