//! Committed transactions as the pgoutput messages a logical slot sends
//! for them, so the physical stream feeds the same [`Decoder`]: `Begin`,
//! `Relation` before a table's first change, `Insert`, `Update` and
//! `Delete` with the whole old row, as under `REPLICA IDENTITY FULL`, and
//! `Commit`.

use std::collections::{HashMap, HashSet};

use straum_app::Commit;

use super::heap::{Change, Tuple};
use super::transactions::Committed;
use super::tuple::{self, Attribute, Kind, Value};
use super::xact::Invalidated;
use super::{RelationFile, WalError};
use crate::decode::Decoder;
use crate::error::Error;
use crate::pgoutput::{Field, Message, Old, Relation};
use crate::value::{
    BOOL, BPCHAR, INT2, INT4, INT8, NUMERIC, TEXT, TIMESTAMP, TIMESTAMPTZ, VARCHAR, system_time,
};

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
    /// By attribute: the value of a column added with a default, as text,
    /// for the rows written before it, which don't hold it
    /// (`attmissingval`).
    pub(crate) missing: Vec<Option<String>>,
}

/// How a column of this type reads off a page.
pub(crate) fn kind(type_oid: u32, enum_type: bool) -> Kind {
    match type_oid {
        INT2 | INT4 | INT8 => Kind::Int,
        NUMERIC => Kind::Numeric,
        BOOL => Kind::Bool,
        TEXT | VARCHAR | BPCHAR => Kind::Text,
        TIMESTAMP => Kind::Timestamp,
        TIMESTAMPTZ => Kind::Timestamptz,
        _ if enum_type => Kind::Enum,
        _ => Kind::Other,
    }
}

pub(crate) struct Publisher {
    tables: HashMap<RelationFile, Table>,
    /// Enum labels by `pg_enum` OID.
    labels: HashMap<u32, String>,
    /// Tables whose `Relation` the decoder has.
    announced: HashSet<RelationFile>,
    /// Tables rewritten or dropped, by OID, for the next commit to say.
    unfit: Vec<u32>,
}

/// A value as a message holds it, owned until the message is built.
enum Text {
    Null,
    Unchanged,
    Value(String),
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
            let (file, old, new) = match change {
                Change::Insert { file, new } => (file, None, Some(new)),
                Change::Update { file, old, new } => (file, Some(old), Some(new)),
                Change::Delete { file, old } => (file, Some(old), None),
            };
            let table = self
                .tables
                .get(file)
                .ok_or_else(|| problem(committed.lsn, "a change to a table Straum doesn't read"))?;
            if self.announced.insert(*file) {
                decoder.feed(Message::Relation(table.relation.clone()))?;
            }
            let read = |tuple: Option<&Tuple>| {
                tuple
                    .map(|t| texts(table, t, &self.labels, committed.lsn))
                    .transpose()
            };
            let (old, new) = (read(old)?, read(new)?);
            let relation = table.relation.id;
            let message = match (old.as_deref(), new.as_deref()) {
                (None, Some(new)) => Message::Insert {
                    relation,
                    new: fields(new),
                },
                (Some(old), Some(new)) => Message::Update {
                    relation,
                    old: Some(Old::Row(fields(old))),
                    new: fields(new),
                },
                (Some(old), None) => Message::Delete {
                    relation,
                    old: Old::Row(fields(old)),
                },
                (None, None) => unreachable!("every change has a row"),
            };
            decoder.feed(message)?;
        }
        decoder.feed(Message::Commit {
            end_lsn: committed.end,
            committed: system_time(committed.time),
        })
    }
}

/// The relation's columns of `tuple` as text.
fn texts(
    table: &Table,
    tuple: &Tuple,
    labels: &HashMap<u32, String>,
    lsn: u64,
) -> Result<Vec<Text>, Error> {
    let values = tuple::values(tuple, &table.attributes, labels).map_err(|p| problem(lsn, p))?;
    table
        .columns
        .iter()
        .map(|&i| {
            Ok(match &values[i] {
                Value::Text(text) => Text::Value(text.clone()),
                // Not a column Straum reads, which the decoder never looks
                // at.
                Value::Null | Value::Skipped => Text::Null,
                // pgoutput, too, leaves out a value TOASTed out of line
                // that the change kept.
                Value::External => Text::Unchanged,
                Value::Missing => match &table.missing[i] {
                    Some(text) => Text::Value(text.clone()),
                    None => Text::Null,
                },
            })
        })
        .collect()
}

/// The one element of an array literal, as `attmissingval::text`
/// prints it: `{7}`, or quoted with backslash escapes when it holds a
/// space, comma, quote, brace or backslash.
pub(crate) fn element(array: &str) -> Option<String> {
    let inner = array.strip_prefix('{')?.strip_suffix('}')?;
    let Some(quoted) = inner.strip_prefix('"') else {
        let plain = !inner.is_empty()
            && !inner.contains([',', '"', '\\', '{', '}'])
            && !inner.eq_ignore_ascii_case("null");
        return plain.then(|| inner.to_owned());
    };
    let mut text = String::new();
    let mut chars = quoted.chars();
    loop {
        match chars.next()? {
            '"' => return chars.next().is_none().then_some(text),
            '\\' => text.push(chars.next()?),
            c => text.push(c),
        }
    }
}

fn problem(lsn: u64, problem: &'static str) -> Error {
    Error::Wal(WalError::Heap { lsn, problem })
}

fn fields(texts: &[Text]) -> Vec<Field<'_>> {
    texts
        .iter()
        .map(|text| match text {
            Text::Null => Field::Null,
            Text::Unchanged => Field::Unchanged,
            Text::Value(value) => Field::Text(value),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use straum_app::Change as Applied;
    use straum_domain::{ColumnType, Datum, Identifier};

    use super::*;
    use crate::catalog::Checked;
    use crate::pgoutput::Column;

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
                attribute(4, 4, Kind::Int),
                attribute(4, 4, Kind::Other),
                attribute(-1, 4, Kind::Text),
                attribute(16, 8, Kind::Other),
                attribute(4, 4, Kind::Int),
            ],
            columns: vec![0, 2, 3, 4],
            missing: vec![None; 5],
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
            body,
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
            "WAL record at 0/00000100 has a change to a table Straum doesn't read"
        );
        let mut cut = row(1, &short("a"));
        cut.body.truncate(6);
        assert_eq!(
            message(publish(insert(FILE, cut))),
            "WAL record at 0/00000100 has a row shorter than its columns"
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
    fn kinds_by_type() {
        let cases = [
            (INT2, false, Kind::Int),
            (INT4, false, Kind::Int),
            (INT8, false, Kind::Int),
            (NUMERIC, false, Kind::Numeric),
            (BOOL, false, Kind::Bool),
            (TEXT, false, Kind::Text),
            (VARCHAR, false, Kind::Text),
            (BPCHAR, false, Kind::Text),
            (TIMESTAMP, false, Kind::Timestamp),
            (TIMESTAMPTZ, false, Kind::Timestamptz),
            (90_000, true, Kind::Enum),
            (600, false, Kind::Other),
        ];
        for (type_oid, enum_type, expected) in cases {
            assert_eq!(kind(type_oid, enum_type), expected, "{type_oid}");
        }
    }

    #[test]
    fn reads_the_default_of_a_column_added_later() {
        let mut table = table();
        table.missing[4] = Some("7".to_owned());
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
    fn reads_array_elements() {
        let cases = [
            ("{7}", Some("7")),
            ("{-1.50}", Some("-1.50")),
            (
                "{\"2024-01-01 00:00:00+00\"}",
                Some("2024-01-01 00:00:00+00"),
            ),
            ("{\"a,b\"}", Some("a,b")),
            ("{\"say \\\"hi\\\" \\\\ back\"}", Some("say \"hi\" \\ back")),
            ("{\"\"}", Some("")),
            ("{\"NULL\"}", Some("NULL")),
            ("{NULL}", None),
            ("{}", None),
            ("{a,b}", None),
            ("7", None),
            ("{7", None),
            ("{\"open}", None),
            ("{\"a\"b}", None),
            ("{\"a\\\"}", None),
        ];
        for (array, expected) in cases {
            assert_eq!(element(array).as_deref(), expected, "{array}");
        }
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
            kind: Kind::Int,
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
                body,
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
