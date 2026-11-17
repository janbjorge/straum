//! Heap fixtures: the records PostgreSQL logged for one table after a
//! checkpoint, and the rows a `SELECT` read of it at the end, kept per
//! major in `fixtures/`. A test applies the records to a [`Heap`] and
//! checks the rows it holds are the ones selected, so a layout read at a
//! wrong offset fails without a server. `capture_heap_fixture` in
//! `tests/records.rs` writes them. Each file is little-endian: the
//! table's relation file (three `u32`), the major, the record count, each
//! record as its LSN, end and length then its bytes, the row count, and
//! each row as its `id` and the length and bytes of its `body`.

use crate::wal::RelationFile;

/// One record as logged.
pub(crate) struct LoggedRecord {
    pub(crate) lsn: u64,
    pub(crate) end: u64,
    pub(crate) bytes: Vec<u8>,
}

/// A row of the fixture's table `t (id int, body text)`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct FixtureRow {
    pub(crate) id: i32,
    pub(crate) body: String,
}

/// A heap fixture, as [`write`] lays it out.
pub(crate) struct Fixture {
    pub(crate) file: RelationFile,
    pub(crate) major: u32,
    pub(crate) records: Vec<LoggedRecord>,
    pub(crate) rows: Vec<FixtureRow>,
}

/// `fixture` as bytes.
pub(crate) fn write(fixture: &Fixture) -> Vec<u8> {
    let mut out = Vec::new();
    for value in [
        fixture.file.tablespace,
        fixture.file.database,
        fixture.file.relation,
        fixture.major,
        u32::try_from(fixture.records.len()).unwrap(),
    ] {
        out.extend(value.to_le_bytes());
    }
    for record in &fixture.records {
        out.extend(record.lsn.to_le_bytes());
        out.extend(record.end.to_le_bytes());
        out.extend(u32::try_from(record.bytes.len()).unwrap().to_le_bytes());
        out.extend(&record.bytes);
    }
    out.extend(u32::try_from(fixture.rows.len()).unwrap().to_le_bytes());
    for row in &fixture.rows {
        out.extend(row.id.to_le_bytes());
        out.extend(u32::try_from(row.body.len()).unwrap().to_le_bytes());
        out.extend(row.body.as_bytes());
    }
    out
}

/// The fixture `bytes` hold, as [`write`] laid them out.
pub(crate) fn read(bytes: &[u8]) -> Fixture {
    let mut reader = Reader { bytes, at: 0 };
    let file = RelationFile {
        tablespace: reader.u32(),
        database: reader.u32(),
        relation: reader.u32(),
    };
    let major = reader.u32();
    let mut records = Vec::new();
    for _ in 0..reader.u32() {
        let lsn = reader.u64();
        let end = reader.u64();
        let length = reader.u32() as usize;
        records.push(LoggedRecord {
            lsn,
            end,
            bytes: reader.take(length).to_vec(),
        });
    }
    let mut rows = Vec::new();
    for _ in 0..reader.u32() {
        let id = i32::from_le_bytes(reader.take(4).try_into().unwrap());
        let length = reader.u32() as usize;
        let body = String::from_utf8(reader.take(length).to_vec()).unwrap();
        rows.push(FixtureRow { id, body });
    }
    assert_eq!(reader.at, bytes.len(), "bytes past the fixture's end");
    Fixture {
        file,
        major,
        records,
        rows,
    }
}

/// Where [`read`] is in the fixture's bytes.
struct Reader<'b> {
    // Borrows the fixture's bytes for one read.
    bytes: &'b [u8],
    at: usize,
}

impl Reader<'_> {
    fn take(&mut self, length: usize) -> &[u8] {
        let taken = &self.bytes[self.at..self.at + length];
        self.at += length;
        taken
    }

    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take(4).try_into().unwrap())
    }

    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take(8).try_into().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use straum_domain::{ColumnType, Datum};

    use super::*;
    use crate::wal::heap::Heap;
    use crate::wal::test_support::{attribute, decode};
    use crate::wal::tuple::{Kind, Value, values};

    /// Each major's fixture, as captured from its server.
    const FIXTURES: [(&str, &[u8]); 3] = [
        ("17", include_bytes!("fixtures/pg17.bin")),
        ("18", include_bytes!("fixtures/pg18.bin")),
        ("19", include_bytes!("fixtures/pg19.bin")),
    ];

    /// The rows `fixture`'s records leave on its table's pages, read as
    /// `t (id int, body text)`.
    fn replayed(fixture: &Fixture) -> Vec<FixtureRow> {
        let mut heap = Heap::new([fixture.file].into(), fixture.major);
        for logged in &fixture.records {
            let record = decode(logged.lsn, logged.end, logged.bytes.clone()).unwrap();
            heap.apply(&record).unwrap();
        }
        let columns = [
            attribute(4, 4, Kind::Read(ColumnType::Int)),
            attribute(-1, 4, Kind::Read(ColumnType::Text)),
        ];
        let mut rows = Vec::new();
        for (id, page) in heap.pages() {
            assert_eq!(id.file, fixture.file);
            for (_, tuple) in page.rows() {
                let read = values(tuple, &columns, &HashMap::new()).unwrap();
                let [
                    Value::Datum(Datum::Int(id)),
                    Value::Datum(Datum::Text(body)),
                ] = &read[..]
                else {
                    panic!("a row read as {read:?}");
                };
                rows.push(FixtureRow {
                    id: i32::try_from(*id).unwrap(),
                    body: body.to_string(),
                });
            }
        }
        rows.sort();
        rows
    }

    /// The records each major logged rebuild the rows it then selected:
    /// inserts, a bulk `COPY`, updates, deletes and vacuums, read at
    /// PostgreSQL's own offsets rather than the ones the code names.
    #[test]
    fn real_records_rebuild_the_rows_selected() {
        for (major, bytes) in FIXTURES {
            let fixture = read(bytes);
            assert!(
                fixture.records.len() > 400,
                "{major}: {} records",
                fixture.records.len()
            );
            assert_eq!(replayed(&fixture), fixture.rows, "{major}");
        }
    }

    /// A fixture reads back as written.
    #[test]
    fn a_fixture_reads_back_as_written() {
        let written = Fixture {
            file: RelationFile {
                tablespace: 1,
                database: 2,
                relation: 3,
            },
            major: 18,
            records: vec![LoggedRecord {
                lsn: 10,
                end: 20,
                bytes: vec![1, 2, 3],
            }],
            rows: vec![FixtureRow {
                id: -4,
                body: "ø".into(),
            }],
        };
        let back = read(&write(&written));
        assert_eq!((back.file, back.major), (written.file, written.major));
        assert_eq!(back.records[0].bytes, [1, 2, 3]);
        assert_eq!((back.records[0].lsn, back.records[0].end), (10, 20));
        assert_eq!(back.rows, written.rows);
    }
}
