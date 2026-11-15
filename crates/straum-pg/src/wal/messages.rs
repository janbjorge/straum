//! Committed transactions as the pgoutput messages a logical slot sends
//! for them, so the physical stream feeds the same [`Decoder`]: `Begin`,
//! `Relation` before a table's first change, `Insert`, `Update` and
//! `Delete` with the whole old row, as under `REPLICA IDENTITY FULL`, and
//! `Commit`.

use std::collections::{HashMap, HashSet};

use straum_app::Commit;
use straum_domain::{Datum, FastMap};

use super::heap::{HeapChange, HeapTuple};
use super::transactions::Committed;
use super::tuple::{self, Attribute, Value};
use super::xact::Invalidated;
use super::{Oid, Problem, RelationFile, WalError};
use crate::decode::Decoder;
use crate::error::Error;
use crate::pgoutput::{Field, Message, Old, Relation};
use crate::value::system_time;

/// A table the physical stream reads: its file, as pgoutput announces
/// it, and how its rows lie on its pages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Layout {
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

/// The tables the physical stream reads, and the labels of the enums
/// their columns have, by `pg_enum` OID, which is what a row stores.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Layouts {
    pub(crate) tables: Vec<Layout>,
    pub(crate) labels: HashMap<Oid, String>,
}

pub(crate) struct Publisher {
    tables: FastMap<RelationFile, Layout>,
    /// Enum labels by `pg_enum` OID.
    labels: HashMap<Oid, String>,
    /// Tables whose `Relation` the decoder has.
    announced: HashSet<RelationFile>,
    /// Tables rewritten or dropped, by OID, for the next commit to say.
    unfit: Vec<Oid>,
}

impl Publisher {
    pub(crate) fn new(layouts: Layouts) -> Publisher {
        let mut tables = FastMap::default();
        for table in layouts.tables {
            tables.insert(table.file, table);
        }
        Publisher {
            tables,
            labels: layouts.labels,
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
    pub(crate) fn stale(&self, invalidated: &[Invalidated]) -> Vec<Oid> {
        let mut stale = Vec::new();
        for table in self.tables.values() {
            for message in invalidated {
                if invalidates(*message, table) {
                    stale.push(table.relation.id);
                    break;
                }
            }
        }
        stale.sort_unstable();
        stale
    }

    /// Takes the layouts read again for `stale`. A table in another file
    /// now, rewritten or truncated, or gone, no longer reads as loaded:
    /// its pages went with the old file. Any other is announced again
    /// before its next change, as pgoutput does after an invalidation,
    /// and the decoder sees what changed. A column added may be of an
    /// enum no table had before, so the labels come along.
    pub(crate) fn relayout(&mut self, stale: &[Oid], fresh: Layouts) {
        for &oid in stale {
            let Some(file) = self.file_with_oid(oid) else {
                continue;
            };
            match table_with_oid(&fresh.tables, oid) {
                Some(table) if table.file == file => {
                    self.tables.insert(file, table.clone());
                    self.announced.remove(&file);
                }
                _ => self.unfit.push(oid),
            }
        }
        self.labels.extend(fresh.labels);
    }

    /// The file of the table Straum reads whose OID is `oid`.
    fn file_with_oid(&self, oid: Oid) -> Option<RelationFile> {
        for table in self.tables.values() {
            if table.relation.id == oid {
                return Some(table.file);
            }
        }
        None
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
        decoder.feed(&Message::Begin {
            final_lsn: committed.lsn,
        })?;
        for oid in std::mem::take(&mut self.unfit) {
            // The decoder knows a table by its Relation.
            if let Some(file) = self.file_with_oid(oid)
                && let Some(table) = self.tables.get(&file)
                && self.announced.insert(file)
            {
                decoder.feed(&Message::Relation(table.relation.clone()))?;
            }
            decoder.unfit(oid);
        }
        for change in &committed.changes {
            let file = match change {
                HeapChange::Insert { file, .. }
                | HeapChange::Update { file, .. }
                | HeapChange::Delete { file, .. } => file,
            };
            let Some(table) = self.tables.get(file) else {
                return Err(problem(committed.lsn, Problem::UntrackedTable));
            };
            if self.announced.insert(*file) {
                decoder.feed(&Message::Relation(table.relation.clone()))?;
            }
            let (labels, lsn) = (&self.labels, committed.lsn);
            let relation = table.relation.id;
            decoder.feed(&match change {
                HeapChange::Insert { new, .. } => Message::Insert {
                    relation,
                    new: fields(table, new, labels, lsn)?,
                },
                HeapChange::Update { old, new, .. } => Message::Update {
                    relation,
                    old: Some(Old::Row(fields(table, old, labels, lsn)?)),
                    new: fields(table, new, labels, lsn)?,
                },
                HeapChange::Delete { old, .. } => Message::Delete {
                    relation,
                    old: Old::Row(fields(table, old, labels, lsn)?),
                },
            })?;
        }
        decoder.feed(&Message::Commit {
            end_lsn: committed.end,
            committed: system_time(committed.time),
        })
    }
}

/// The table in `tables` whose OID is `oid`.
fn table_with_oid(tables: &[Layout], oid: Oid) -> Option<&Layout> {
    for table in tables {
        if table.relation.id == oid {
            return Some(table);
        }
    }
    None
}

/// Whether `message` invalidates `table`: relation 0 is every table of
/// its database.
fn invalidates(message: Invalidated, table: &Layout) -> bool {
    message.database == table.file.database
        && (message.relation == table.relation.id || message.relation == 0)
}

/// The relation's columns of `tuple`, as a message holds them.
fn fields(
    table: &Layout,
    tuple: &HeapTuple,
    labels: &HashMap<Oid, String>,
    lsn: u64,
) -> Result<Vec<Field<'static>>, Error> {
    let mut values = match tuple::values(tuple, &table.attributes, labels) {
        Ok(values) => values,
        Err(row_problem) => return Err(problem(lsn, row_problem)),
    };
    let mut fields = Vec::with_capacity(table.columns.len());
    for &attribute in &table.columns {
        fields.push(
            match std::mem::replace(&mut values[attribute], Value::Missing) {
                Value::Datum(datum) => Field::Datum(datum),
                // pgoutput, too, leaves out a value TOASTed out of line that
                // the change kept.
                Value::External => Field::Unchanged,
                Value::Missing => Field::Datum(table.missing[attribute].clone()),
            },
        );
    }
    Ok(fields)
}

fn problem(lsn: u64, problem: Problem) -> Error {
    Error::Wal(WalError::At { lsn, problem })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use straum_app::Change;
    use straum_domain::{ColumnType, Datum, Identifier};

    use super::*;
    use crate::catalog::Checked;
    use crate::pgoutput::RelationColumn;
    use crate::value::{INT4, INT8, TEXT};
    use crate::wal::test_support::{FILE, attribute, short_varlena as short};
    use crate::wal::tuple::{Kind, VARLENA_LENGTH};

    const INT: Kind = Kind::Read(ColumnType::Int);
    /// `point`'s type OID, a type Straum doesn't read.
    const POINT: u32 = 600;
    /// `sizeof(Point)`: two `float8`s, aligned to 8.
    const POINT_SIZE: usize = 16;
    const POINT_LENGTH: i16 = 16;
    const POINT_ALIGN: usize = 8;
    /// `t_hoff` of a row without nulls: `SizeofHeapTupleHeader`, 23,
    /// rounded up to `MAXALIGN`, so one byte of padding before the columns.
    const ROW_HEADER: u8 = 24;
    const ROW_PADDING: usize = 1;
    /// A TOAST pointer to a value on disk: `varattrib_1b_e`'s header byte
    /// and `va_tag` `VARTAG_ONDISK`, then a 16-byte `varatt_external`.
    const TOAST_POINTER: [u8; 2] = [0x01, 18];
    const EXTERNAL_POINTER: usize = 16;

    fn column(name: &str, type_oid: Oid) -> RelationColumn {
        RelationColumn {
            name: name.to_owned(),
            type_oid,
            typmod: -1,
        }
    }

    /// `public.zones`: `id int4`, a dropped `int4`, `name text`, `p
    /// point`, and `late int4`, added after the rows written here.
    fn table() -> Layout {
        Layout {
            file: FILE,
            relation: Relation {
                id: 77,
                schema: "public".to_owned(),
                name: "zones".to_owned(),
                columns: vec![
                    column("id", INT4),
                    column("name", TEXT),
                    column("p", POINT),
                    column("late", INT4),
                ],
            },
            attributes: vec![
                attribute(4, 4, INT),
                attribute(4, 4, Kind::Other),
                attribute(VARLENA_LENGTH, 4, Kind::Read(ColumnType::Text)),
                attribute(POINT_LENGTH, POINT_ALIGN, Kind::Other),
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
    fn row(id: i32, name: &[u8]) -> HeapTuple {
        let mut body = vec![0; ROW_PADDING];
        body.extend(id.to_le_bytes());
        body.extend(9i32.to_le_bytes());
        body.extend(name);
        // The point aligns from where the columns start, past the padding.
        let columns = body.len() - ROW_PADDING;
        body.resize(columns.next_multiple_of(POINT_ALIGN) + ROW_PADDING, 0);
        body.extend([1; POINT_SIZE]);
        HeapTuple {
            infomask2: 4,
            infomask: 0,
            hoff: ROW_HEADER,
            body: body.into(),
        }
    }

    fn committed(changes: Vec<HeapChange>) -> Committed {
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

    fn layouts(tables: Vec<Layout>) -> Layouts {
        Layouts {
            tables,
            labels: HashMap::new(),
        }
    }

    fn publish(changes: Vec<HeapChange>) -> Result<Option<straum_app::Commit>, Error> {
        let mut publisher = Publisher::new(layouts(vec![table()]));
        publisher.publish(&committed(changes), &mut decoder())
    }

    fn zone_row(name: &str) -> Vec<Datum> {
        vec![Datum::Int(1), Datum::Text(name.into())]
    }

    fn zones() -> Identifier {
        Identifier::new("zones").unwrap()
    }

    #[test]
    fn publishes_changes_as_a_logical_slot_does() {
        let first = row(1, &short("a"));
        let second = row(1, &short("b"));
        let changes = vec![
            HeapChange::Insert {
                file: FILE,
                new: first.clone(),
            },
            HeapChange::Update {
                file: FILE,
                old: first.clone(),
                new: second.clone(),
            },
            HeapChange::Delete {
                file: FILE,
                old: second,
            },
        ];
        let commit = publish(changes).unwrap().unwrap();
        assert_eq!(commit.lsn.0, 0x140);
        assert_eq!(commit.committed, system_time(1_000_000));
        assert_eq!(
            commit.changes,
            [
                Change::Insert {
                    table: zones(),
                    row: zone_row("a"),
                },
                Change::Update {
                    table: zones(),
                    old: zone_row("a"),
                    new: zone_row("b"),
                },
                Change::Delete {
                    table: zones(),
                    old: zone_row("b"),
                },
            ]
        );
        assert_eq!(
            Publisher::new(layouts(vec![table()])).files(),
            [FILE].into()
        );
    }

    #[test]
    fn leaves_out_values_toasted_out_of_line() {
        let mut pointer = TOAST_POINTER.to_vec();
        pointer.extend([0; EXTERNAL_POINTER]);
        let toasted = row(1, &pointer);
        let changes = vec![HeapChange::Update {
            file: FILE,
            old: toasted.clone(),
            new: toasted,
        }];
        let commit = publish(changes).unwrap().unwrap();
        // Unchanged in the old row too: there is nothing to take it from.
        assert!(
            matches!(commit.changes[..], [Change::Unfit { .. }]),
            "{commit:?}"
        );
    }

    fn message(result: Result<Option<straum_app::Commit>, Error>) -> String {
        result.unwrap_err().to_string()
    }

    fn insert_of_a() -> Committed {
        committed(vec![HeapChange::Insert {
            file: FILE,
            new: row(1, &short("a")),
        }])
    }

    #[test]
    fn rejects_what_it_cant_publish() {
        let other = RelationFile {
            relation: 1,
            ..FILE
        };
        let elsewhere = vec![HeapChange::Insert {
            file: other,
            new: row(1, &short("a")),
        }];
        assert_eq!(
            message(publish(elsewhere)),
            "WAL at 0/100 has a change to a table Straum doesn't read"
        );
        let mut cut = row(1, &short("a"));
        // The padding, `id` and one byte of the dropped column.
        cut.body = cut.body[..ROW_PADDING + 5].into();
        assert_eq!(
            message(publish(vec![HeapChange::Insert {
                file: FILE,
                new: cut
            }])),
            "WAL at 0/100 has a row shorter than its columns"
        );
        // A row from before `late` was added holds no value for it; the
        // catalog's default is the value, which Straum doesn't read.
    }

    #[test]
    fn reads_a_column_added_without_a_default_as_null() {
        let mut publisher = Publisher::new(layouts(vec![table()]));
        let changes = vec![HeapChange::Insert {
            file: FILE,
            new: row(2, &short("z")),
        }];
        let commit = publisher.publish(&committed(changes), &mut decoder());
        let commit = commit.unwrap().unwrap();
        let row = vec![Datum::Int(2), Datum::Text("z".into())];
        assert_eq!(
            commit.changes,
            [Change::Insert {
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
        let mut publisher = Publisher::new(layouts(vec![table]));
        let changes = vec![HeapChange::Insert {
            file: FILE,
            new: row(3, &short("c")),
        }];
        let commit = publisher.publish(&committed(changes), &mut decoder);
        let row = vec![Datum::Int(3), Datum::Int(7)];
        assert_eq!(
            commit.unwrap().unwrap().changes,
            [Change::Insert {
                table: zones(),
                row,
            }]
        );
    }

    #[test]
    fn finds_the_tables_a_commit_invalidates() {
        let publisher = Publisher::new(layouts(vec![table()]));
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
        let mut publisher = Publisher::new(layouts(vec![table()]));
        let mut decoder = decoder();
        publisher.publish(&insert_of_a(), &mut decoder).unwrap();
        // Nothing changed for the decoder: rows read on.
        publisher.relayout(&[77], layouts(vec![table()]));
        let commit = (publisher.publish(&insert_of_a(), &mut decoder))
            .unwrap()
            .unwrap();
        assert!(matches!(commit.changes[..], [Change::Insert { .. }]));
        // `name` now reads as another type: the decoder says unfit.
        let mut retyped = table();
        retyped.relation.columns[1].type_oid = INT8;
        retyped.attributes[2] = Attribute {
            length: 8,
            align: 8,
            kind: INT,
        };
        publisher.relayout(&[77], layouts(vec![retyped]));
        let mut changes = Vec::new();
        // `id`, the dropped `int4`, `name` now an `int8`, then the point.
        let mut body = vec![0; ROW_PADDING];
        body.extend(1i32.to_le_bytes());
        body.extend([0; 4]);
        body.extend(5i64.to_le_bytes());
        body.extend([1; POINT_SIZE]);
        changes.push(HeapChange::Insert {
            file: FILE,
            new: HeapTuple {
                infomask2: 4,
                infomask: 0,
                hoff: ROW_HEADER,
                body: body.into(),
            },
        });
        let commit = publisher.publish(&committed(changes), &mut decoder);
        let commit = commit.unwrap().unwrap();
        assert!(
            matches!(commit.changes[0], Change::Unfit { .. }),
            "{commit:?}"
        );
        // An OID it doesn't read: nothing.
        publisher.relayout(&[78], Layouts::default());
        assert_eq!(
            publisher
                .publish(&committed(Vec::new()), &mut decoder)
                .unwrap(),
            None
        );
    }

    #[test]
    fn a_table_rewritten_or_dropped_is_unfit() {
        let rewritten = Layout {
            file: RelationFile {
                relation: 16_999,
                ..FILE
            },
            ..table()
        };
        for fresh in [vec![rewritten], Vec::new()] {
            let mut publisher = Publisher::new(layouts(vec![table()]));
            let mut decoder = decoder();
            publisher.relayout(&[77], layouts(fresh));
            // Said even by a commit that changed no row.
            let commit = publisher.publish(&invalidating(5, 77), &mut decoder);
            let commit = commit.unwrap().unwrap();
            assert_eq!(commit.changes, [Change::Unfit { table: zones() }]);
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
