//! What the physical stream reads off the WAL, checked against
//! PostgreSQL's own tools: the records `pg_waldump` prints, the rows
//! `pageinspect` sees on each page, and the values a `SELECT` reads.

use std::fmt::Write as _;
use std::time::Instant;

use straum_app::Lsn;
use straum_domain::{ColumnType, Datum};

use super::support::*;
use crate::catalog::Checked;
use crate::dsn::Dsn;
use crate::physical::{self, Physical};
use crate::source::parse_lsn;
use crate::value::{self};
use crate::wal::RelationFile;
use crate::wal::heap::{Heap, HeapTuple, PageId};
use crate::wal::messages::{Layout, Layouts};
use crate::wal::tuple::{self as row_values, Kind};
use crate::wire::{Connection, ConnectionKind};

/// The physical stream reads the records `pg_waldump` reads from the same
/// start: whole, in order, with the same blocks and full-page images,
/// under every `wal_compression`.
#[test]
fn reads_the_wal_pg_waldump_reads() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(
        &pg,
        "physical",
        "CREATE TABLE $S.t (id int PRIMARY KEY, body text);
         ALTER TABLE $S.t ALTER body SET STORAGE EXTERNAL;",
    );
    let (mut physical, start) = Physical::start(&pg.dsn).unwrap();
    for compression in ["off", "pglz", "lz4", "zstd"] {
        db.run(&format!("ALTER SYSTEM SET wal_compression = {compression}"));
        db.run("SELECT pg_reload_conf()");
        db.run("CHECKPOINT");
        write_records_of_every_kind(&mut db);
    }
    let last = db.insert_lsn();
    db.run("SELECT pg_switch_wal()");
    let mut read = Vec::new();
    let deadline = Instant::now() + LONG;
    loop {
        assert!(Instant::now() < deadline, "no switch record past {last}");
        let Some(record) = physical.next_record().unwrap() else {
            continue;
        };
        read.push(dumped(&record));
        if record.is_switch() && record.lsn >= last.0 {
            break;
        }
    }
    let switch = read.last().unwrap().lsn;
    let directory = db.rows("SHOW data_directory")[0][0].clone().unwrap();
    let out = pg.waldump(&directory, &["-s".to_owned(), start.to_string()]);
    let expected = dumped_through(&out, switch);
    assert_eq!(read[0].lsn, start.0);
    assert!(read.len() > 1000, "{} records", read.len());
    for (ours, theirs) in read.iter().zip(&expected) {
        assert_eq!(ours, theirs);
    }
    assert_eq!(read.len(), expected.len());
}

/// The records of `pg_waldump`'s output `out` up to the one at `switch`.
fn dumped_through(out: &str, switch: u64) -> Vec<DumpedRecord> {
    let mut records = Vec::new();
    for line in out.lines() {
        let Some(record) = waldump_line(line) else {
            continue;
        };
        if record.lsn > switch {
            break;
        }
        records.push(record);
    }
    records
}

/// Inserts, updates and deletes, records past a page, and savepoints.
fn write_records_of_every_kind(db: &mut Db) {
    db.run(
        "INSERT INTO $S.t SELECT i, md5(i::text) FROM generate_series(1, 400) AS i
         ON CONFLICT (id) DO UPDATE SET body = excluded.body || 'x'",
    );
    db.run("UPDATE $S.t SET body = body || 'y' WHERE id % 3 = 0");
    db.run("DELETE FROM $S.t WHERE id % 7 = 0");
    // Past a page: TOAST chunks, and a record split across pages.
    db.run(
        "INSERT INTO $S.t SELECT 1000 + i, string_agg(md5(j::text), '')
         FROM generate_series(1, 3) AS i, generate_series(1, 700) AS j GROUP BY i
         ON CONFLICT (id) DO NOTHING",
    );
    db.run(
        "BEGIN; DELETE FROM $S.t WHERE id = -1;
         SAVEPOINT a; INSERT INTO $S.t VALUES (-1, 'a'); RELEASE a;
         SAVEPOINT b; INSERT INTO $S.t VALUES (-2, 'b'); ROLLBACK TO b; COMMIT",
    );
}

/// The rows Straum keeps for each page of a table are the rows on that
/// page in PostgreSQL, slot by slot, after inserts, `COPY`, `ON
/// CONFLICT`, HOT and other updates, rows moving page, deletes and
/// `VACUUM`, under each `wal_compression` Straum reads.
#[test]
fn keeps_the_rows_pageinspect_sees() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(
        &pg,
        "pages",
        "CREATE EXTENSION pageinspect;
         CREATE TABLE $S.t (id int PRIMARY KEY, n int, note text, body text)
             WITH (fillfactor = 60, autovacuum_enabled = off);",
    );
    let (mut physical, _) = Physical::start(&pg.dsn).unwrap();
    for (round, compression) in ["off", "pglz", "lz4"].into_iter().enumerate() {
        db.run(&format!("ALTER SYSTEM SET wal_compression = {compression}"));
        db.run("SELECT pg_reload_conf()");
        db.run("CHECKPOINT");
        churn_pages(&mut db, round * 10_000);
    }
    let redo = change_a_few_pages_after_a_checkpoint(&mut db);
    let TableFile { relation, pages } = relation_file(&mut db, "t");
    let Followed { heap, changes } = follow_to_now(&mut db, &mut physical, relation);
    assert!(changes > 3000, "{changes} changes");
    assert!(pages > 10, "{pages} pages");
    assert_eq!(
        pages_past_the_end(&heap, relation, pages),
        Vec::<u32>::new(),
        "pages past the end"
    );
    let mut kept = 0;
    for block in 0..pages {
        if page_matches_pageinspect(
            &mut db,
            &heap,
            PageId {
                file: relation,
                block,
            },
            redo,
        ) {
            kept += 1;
        }
    }
    assert!(
        (1..pages / 2).contains(&kept),
        "{kept} of {pages} pages kept"
    );
    assert_eq!(heap.bytes(), heap.recount());
}

/// The pages `heap` keeps of `relation` at or past block `pages`.
fn pages_past_the_end(heap: &Heap, relation: RelationFile, pages: u32) -> Vec<u32> {
    let mut past = Vec::new();
    for (id, _) in heap.pages() {
        if id.file == relation && id.block >= pages {
            past.push(id.block);
        }
    }
    past
}

/// Asserts that `heap` keeps page `id` exactly when PostgreSQL changed it
/// after `redo`, with the rows `pageinspect` sees; whether it keeps it.
fn page_matches_pageinspect(db: &mut Db, heap: &Heap, id: PageId, redo: Lsn) -> bool {
    let block = id.block;
    let rows = db.rows(&format!(
        "SELECT lsn FROM page_header(get_raw_page('$S.t', {block}))"
    ));
    let page_lsn = parse_lsn(rows[0][0].as_deref().unwrap()).unwrap();
    let page = heap.page(id);
    assert_eq!(
        page.is_some(),
        page_lsn > redo,
        "page {block}: LSN {page_lsn}, redo {redo}"
    );
    let Some(page) = page else {
        return false;
    };
    let theirs = page_items(db, block);
    let mut ours: Vec<String> = Vec::new();
    for (offnum, tuple) in page.rows() {
        ours.push(page_item(offnum, tuple));
    }
    assert_eq!(ours, theirs, "page {block}");
    true
}

/// Cuts empty pages off the end, then changes a few after a checkpoint:
/// its redo point.
fn change_a_few_pages_after_a_checkpoint(db: &mut Db) -> Lsn {
    // Empty pages at the end, which VACUUM cuts off.
    db.run("INSERT INTO $S.t SELECT 100000 + i, i, 'f', md5(i::text) FROM generate_series(1, 3000) AS i");
    db.run("DELETE FROM $S.t WHERE id > 100000");
    db.run("VACUUM $S.t");
    // After a checkpoint Straum keeps only the pages changed since its
    // redo point: the others come whole with their next change.
    db.run("CHECKPOINT");
    db.run("UPDATE $S.t SET n = n + 1 WHERE id IN (SELECT id FROM $S.t ORDER BY id LIMIT 3)");
    db.run("DELETE FROM $S.t WHERE id = (SELECT max(id) FROM $S.t)");
    // A lock changes no row Straum reads, yet sets the page's LSN.
    db.run(
        "BEGIN; SELECT id FROM $S.t WHERE id = (SELECT min(id) + 200 FROM $S.t) FOR UPDATE; COMMIT",
    );
    let rows = db.rows("SELECT redo_lsn FROM pg_control_checkpoint()");
    parse_lsn(rows[0][0].as_deref().unwrap()).unwrap()
}

/// First changes since the checkpoint, so each brings a page image, then
/// HOT and other updates, rows moving page, deletes, `COPY`, `ON
/// CONFLICT` and `VACUUM`, with ids from `base`.
fn churn_pages(db: &mut Db, base: usize) {
    db.run("UPDATE $S.t SET n = n + 1 WHERE id % 9 = 1");
    db.run("DELETE FROM $S.t WHERE id % 17 = 2");
    db.run("UPDATE $S.t SET body = repeat(body, 5) WHERE id % 19 = 3");
    db.run(&format!(
        "INSERT INTO $S.t SELECT {base} + i, i, CASE WHEN i % 5 = 0 THEN NULL ELSE 'n' END,
                 repeat(md5(i::text), 1 + i % 4)
             FROM generate_series(1, 300) AS i"
    ));
    // HOT: n has no index. The long body shares a prefix and suffix.
    db.run("UPDATE $S.t SET n = n + 1 WHERE id % 3 = 0");
    db.run("UPDATE $S.t SET body = body || 'tail' WHERE id % 4 = 0");
    db.run("UPDATE $S.t SET body = 'head' || body WHERE id % 6 = 0");
    db.run("UPDATE $S.t SET note = 'x' WHERE note IS NULL AND id % 2 = 0");
    // Not HOT, and rows too big to stay on their page.
    db.run("UPDATE $S.t SET id = id + 5000 WHERE id % 7 = 0");
    db.run("UPDATE $S.t SET body = repeat(body, 6) WHERE id % 11 = 0");
    db.run("DELETE FROM $S.t WHERE id % 13 = 0");
    db.run(&format!(
        "COPY (SELECT {base} + 9000 + i, i, 'c', md5(i::text)
                   FROM generate_series(1, 200) AS i) TO '/tmp/rows.csv'"
    ));
    db.run("COPY $S.t FROM '/tmp/rows.csv'");
    db.run(&format!(
        "INSERT INTO $S.t VALUES ({base} + 1, 0, 'dup', 'dup'), ({base} + 8999, 0, 'new', 'new')
             ON CONFLICT (id) DO NOTHING"
    ));
    db.run("VACUUM $S.t");
    db.run("UPDATE $S.t SET n = n + 1 WHERE id % 5 = 1");
}

/// The rows `pageinspect` sees on page `block` of `$S.t`, as
/// [`page_item`] puts them.
fn page_items(db: &mut Db, block: u32) -> Vec<String> {
    let rows = db.rows(&format!(
        "SELECT lp, t_hoff, t_infomask2 & 2047, coalesce(t_bits, ''), encode(t_data, 'hex')
         FROM heap_page_items(get_raw_page('$S.t', {block})) WHERE lp_flags = 1
         ORDER BY lp"
    ));
    let mut items = Vec::new();
    for row in rows {
        let fields: Vec<String> = row.into_iter().map(Option::unwrap).collect();
        items.push(fields.join(" "));
    }
    items
}

/// The pages the stream keeps of a table, and how many changes to its
/// rows it read.
struct Followed {
    heap: Heap,
    changes: usize,
}

/// The pages of `relation` once the stream has read all WAL written so
/// far, and how many changes to its rows that was.
fn follow_to_now(db: &mut Db, physical: &mut Physical, relation: RelationFile) -> Followed {
    let mut heap = Heap::new([relation].into(), physical.major());
    let last = db.insert_lsn();
    db.run("SELECT pg_switch_wal()");
    let deadline = Instant::now() + LONG;
    let mut changes = 0;
    loop {
        assert!(Instant::now() < deadline, "no switch record past {last}");
        let Some(record) = physical.next_record().unwrap() else {
            continue;
        };
        changes += heap.apply(&record).unwrap().len();
        if record.is_switch() && record.lsn >= last.0 {
            return Followed { heap, changes };
        }
    }
}

/// Rows of every type `$S.v` holds, ids `from` to `to`.
fn datum_rows(from: i32, to: i32) -> String {
    format!(
            "INSERT INTO $S.v (id, a, b, flag, n, whole, free, t, v, c, ts, tz, mood, p, label, l, z)
             SELECT (i - 150)::int8 * 1000000007, ((i * 7919) % 65535 - 32767)::int2, i * -123457,
                 CASE i % 3 WHEN 0 THEN NULL ELSE i % 2 = 0 END,
                 (i - 150) * 333333333.3333, i * 99991,
                 CASE i % 10 WHEN 0 THEN 'NaN' WHEN 1 THEN 'Infinity' WHEN 2 THEN '-Infinity'
                     WHEN 3 THEN NULL WHEN 4 THEN i * 1e-30 WHEN 5 THEN i * 1e40
                     WHEN 6 THEN -i ELSE i::numeric / 7 END,
                 CASE i % 4 WHEN 0 THEN '' WHEN 1 THEN 'héllo ' || i
                     WHEN 2 THEN repeat('x', 200 + i) END,
                 'v' || i, left(i::text, 3),
                 CASE i % 17 WHEN 0 THEN 'infinity' WHEN 1 THEN '0044-03-15 12:00:00 BC'
                     WHEN 2 THEN '1999-12-31 23:59:59.999999' WHEN 3 THEN '10000-02-29 01:02:03'
                     ELSE timestamp '1900-01-01' + i * interval '97 days 1 hour 7 minutes 3.25 seconds'
                 END,
                 CASE i % 19 WHEN 0 THEN '-infinity'
                     ELSE timestamptz '1970-01-01 00:00:00+00' + i * interval '1234567.891011 seconds'
                 END,
                 (enum_range(NULL::$S.mood))[1 + i % 3], point(i, -i), 'n' || i,
                 CASE i % 5 WHEN 0 THEN repeat('lz4', 1000 + i) WHEN 1 THEN 'short' END,
                 CASE i % 5 WHEN 1 THEN repeat('pglz', 800 + i)
                     WHEN 2 THEN (SELECT string_agg(md5(i::text || j), '')
                                  FROM generate_series(1, 300) AS j)
                 END
             FROM generate_series({from}, {to}) AS i"
        )
}

/// Every value Straum reads off the pages of a table is the datum the
/// logical stream reads of the text a `SELECT` gets, which pgoutput's
/// matches: each type Straum reads, nulls, values
/// compressed with pglz and lz4, rows from before a column was dropped or
/// added, and rows an update rebuilt from the old one.
#[test]
fn reads_the_values_select_reads() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(
        &pg,
        "datums",
        "CREATE TYPE $S.mood AS ENUM ('sad', 'ok', 'happy');
         CREATE TABLE $S.v (
             id int8 PRIMARY KEY, a int2, b int4, flag bool, gone int4,
             n numeric(18, 4), whole numeric(10, 0), free numeric, t text,
             v varchar(20), c char(5), ts timestamp, tz timestamptz,
             mood $S.mood, p point, label name,
             l text COMPRESSION lz4, z text COMPRESSION pglz
         ) WITH (autovacuum_enabled = off);",
    );
    let (mut physical, _) = Physical::start(&pg.dsn).unwrap();
    write_datums(&mut db);
    let TableFile { relation, pages } = relation_file(&mut db, "v");
    let Followed { heap, .. } = follow_to_now(&mut db, &mut physical, relation);
    let layouts = datums_layouts(&pg.dsn);
    let [table] = &layouts.tables[..] else {
        panic!("{:?}", layouts.tables)
    };
    assert_eq!(table.file, relation);
    assert_eq!(
        table.attributes.len(),
        table.columns.len() + 1,
        "one dropped column"
    );
    let names = column_names(table);
    let select = selected_as_read(table, &names);
    let expected = db.rows(&format!(
        "SELECT ctid::text, {select} FROM $S.v ORDER BY ctid"
    ));
    assert_eq!(expected.len(), 300);
    assert!(pages > 5, "{pages} pages");
    let mut counts = Counts::default();
    for row in expected {
        assert_row_reads_as_selected(&heap, &layouts, &names, &row, &mut counts);
    }
    assert!(counts.external >= 30 && counts.missing >= 400, "{counts:?}");
    let compressed = db.rows(
        "SELECT count(*) FILTER (WHERE pg_column_compression(l) = 'lz4'),
                count(*) FILTER (WHERE pg_column_compression(z) = 'pglz')
         FROM $S.v",
    );
    for count in &compressed[0] {
        let count: u32 = count.as_deref().unwrap().parse().unwrap();
        assert!(count >= 40, "{compressed:?}");
    }
}

/// The names of `table`'s live columns.
fn column_names(table: &Layout) -> Vec<String> {
    let mut names = Vec::new();
    for column in &table.relation.columns {
        names.push(column.name.clone());
    }
    names
}

/// Asserts the row at `row`'s ctid, read off the pages in `heap`, holds
/// the values the `SELECT` gave in the rest of `row`.
fn assert_row_reads_as_selected(
    heap: &Heap,
    layouts: &Layouts,
    names: &[String],
    row: &[Option<String>],
    counts: &mut Counts,
) {
    let table = &layouts.tables[0];
    let ctid = row[0].as_deref().unwrap();
    let Some((block, offnum)) = ctid.trim_matches(['(', ')']).split_once(',') else {
        panic!("no ctid in {ctid}");
    };
    let id = PageId {
        file: table.file,
        block: block.parse().unwrap(),
    };
    let Some(page) = heap.page(id) else {
        panic!("no row at {ctid}");
    };
    let Some(tuple) = page.get(offnum.parse().unwrap()) else {
        panic!("no row at {ctid}");
    };
    let values = row_values::values(tuple, &table.attributes, &layouts.labels).unwrap();
    let columns = table.columns.iter().zip(&row[1..]).zip(names);
    for ((&attribute, theirs), name) in columns {
        let ours = &values[attribute];
        let fits = value_fits(table, attribute, ours, theirs.as_deref(), counts);
        assert!(fits, "{ctid} {name}: ours {ours:?}, theirs {theirs:?}");
    }
}

/// Rows before a column is dropped and others added, then updates.
fn write_datums(db: &mut Db) {
    db.run("CHECKPOINT");
    db.run(&datum_rows(1, 200));
    db.run("ALTER TABLE $S.v DROP COLUMN gone");
    db.run(
        "ALTER TABLE $S.v ADD late int DEFAULT 7, ADD late_null int,
             ADD late_text text DEFAULT 'a \"quoted\", {value} \\ here',
             ADD late_at timestamptz DEFAULT '2024-01-01 00:00:00.5+00'",
    );
    db.run(&datum_rows(201, 300));
    db.run("UPDATE $S.v SET b = b + 1 WHERE id % 3 = 0");
    db.run("UPDATE $S.v SET t = t || 'z', late = 8 WHERE id % 4 = 1");
}

/// The layout of `datums.v` and its enum's labels, every live column
/// read: one the spec skips is only stepped over.
fn datums_layouts(dsn: &Dsn) -> Layouts {
    let mut connection = Connection::connect(dsn, ConnectionKind::Sql).unwrap();
    let mut columns = Vec::new();
    for name in [
        "id",
        "a",
        "b",
        "flag",
        "n",
        "whole",
        "free",
        "t",
        "v",
        "c",
        "ts",
        "tz",
        "mood",
        "p",
        "label",
        "l",
        "z",
        "late",
        "late_null",
        "late_text",
        "late_at",
    ] {
        columns.push(id(name));
    }
    let checked = Checked {
        name: id("v"),
        oid: 0,
        schema: id("datums"),
        table_name: id("v"),
        columns,
        types: Vec::new(),
        enums: Vec::new(),
    };
    physical::read_layouts(&mut connection, &[checked]).unwrap()
}

/// The `SELECT` list of `table`'s live columns, named `names`, with a
/// time as the binary load reads it: the text reader takes no year
/// before 1 or past 9999.
fn selected_as_read(table: &Layout, names: &[String]) -> String {
    let mut selected = Vec::new();
    for (&attribute, name) in table.columns.iter().zip(names) {
        selected.push(match table.attributes[attribute].kind {
            Kind::Read(ColumnType::Time { .. }) => format!("floor(extract(epoch FROM {name}))"),
            _ => name.clone(),
        });
    }
    selected.join(", ")
}

/// The values a page left out of line, and those it lacks because their
/// column came later.
#[derive(Debug, Default)]
struct Counts {
    external: u32,
    missing: u32,
}

/// Whether `ours`, read off the page for `attribute`, is what `theirs`,
/// PostgreSQL's text, reads as.
fn value_fits(
    table: &Layout,
    attribute: usize,
    ours: &row_values::Value,
    theirs: Option<&str>,
    counts: &mut Counts,
) -> bool {
    let kind = table.attributes[attribute].kind;
    match ours {
        row_values::Value::Datum(datum) => logical_read(kind, theirs).as_ref() == Some(datum),
        row_values::Value::External => {
            counts.external += 1;
            match theirs {
                Some(text) => text.len() > 8000,
                None => false,
            }
        }
        row_values::Value::Missing => {
            counts.missing += 1;
            logical_read(kind, theirs).as_ref() == Some(&table.missing[attribute])
        }
    }
}

/// What the logical stream reads of PostgreSQL's text.
fn logical_read(kind: Kind, text: Option<&str>) -> Option<Datum> {
    let Some(text) = text else {
        return Some(Datum::Null);
    };
    match kind {
        Kind::Read(ColumnType::Time { .. }) => match text {
            "Infinity" | "-Infinity" => Some(Datum::Null),
            seconds => seconds.parse().ok().map(Datum::Int),
        },
        Kind::Read(column_type) => value::read(column_type, text).ok(),
        Kind::Enum(_) => Some(Datum::Text(text.into())),
        Kind::Other => Some(Datum::Null),
    }
}

/// `HEAP_NATTS_MASK`: the bits of `t_infomask2` that count columns.
const COLUMN_COUNT_MASK: u16 = 0x07FF;
/// `HEAP_HASNULL`: the row has a null bitmap.
const HAS_NULLS: u16 = 0x0001;
/// `offsetof(HeapTupleHeaderData, t_bits)`: where the null bitmap starts.
const NULL_BITMAP_OFFSET: usize = 23;

/// A row as `heap_page_items` shows it: slot, `t_hoff`, the number of
/// columns, the null bitmap as bits and the columns in hex.
fn page_item(offnum: u16, tuple: &HeapTuple) -> String {
    let columns = tuple.infomask2 & COLUMN_COUNT_MASK;
    let bitmap_bytes = usize::from(tuple.hoff) - NULL_BITMAP_OFFSET;
    let mut bits = String::new();
    if tuple.infomask & HAS_NULLS != 0 {
        // Whole bytes of it, as pageinspect prints them.
        for bit in 0..usize::from(columns).div_ceil(8) * 8 {
            let byte = tuple.body[bit / 8];
            bits.push(if byte >> (bit % 8) & 1 == 1 { '1' } else { '0' });
        }
    }
    let mut hex = String::new();
    for byte in &tuple.body[bitmap_bytes..] {
        write!(hex, "{byte:02x}").unwrap();
    }
    format!("{offnum} {} {columns} {bits} {hex}", tuple.hoff)
}

/// A WAL record as `pg_waldump` prints it: its LSN, then in `text` its
/// total length, transaction and block references.
#[derive(Debug, PartialEq, Eq)]
struct DumpedRecord {
    lsn: u64,
    text: String,
}

/// A record as `pg_waldump` prints it.
fn dumped(record: &crate::wal::Record) -> DumpedRecord {
    let mut text = format!("{} {}", record.total_length(), record.xid);
    for block in &record.blocks {
        let file = block.file;
        write!(
            text,
            " #{} rel {}/{}/{}",
            block.id, file.tablespace, file.database, file.relation
        )
        .unwrap();
        if block.fork != 0 {
            let fork = ["main", "fsm", "vm", "init"][usize::from(block.fork)];
            write!(text, " fork {fork}").unwrap();
        }
        write!(text, " blk {}", block.number).unwrap();
        match &block.image {
            Some(image) if image.apply => text.push_str(" FPW"),
            Some(_) => text.push_str(" FPW for WAL verification"),
            None => {}
        }
    }
    DumpedRecord {
        lsn: record.lsn,
        text,
    }
}

/// One line of `pg_waldump` as [`dumped`] puts a record.
fn waldump_line(line: &str) -> Option<DumpedRecord> {
    let (_, total) = after(line, "len (rec/tot):")?.split_once('/')?;
    let total = total.split(',').next()?.trim();
    let xid = after(line, "tx:")?.split(',').next()?.trim();
    let lsn = parse_lsn(after(line, "lsn:")?.split(',').next()?.trim())?;
    let mut text = format!("{total} {xid}");
    for block in line.split("blkref ").skip(1) {
        let block = block.trim_end().trim_end_matches(',');
        text.push(' ');
        text.push_str(block.replacen(':', "", 1).as_str());
    }
    Some(DumpedRecord { lsn: lsn.0, text })
}
