//! Loads: a table's binary `COPY` reads as its text does, and the copy
//! reader handles every way a fake server's copy can go wrong. A load
//! after the stream started is refused. A table's estimate comes from the
//! catalog.

use std::cell::RefCell;

use straum_app::{Estimate, LoadCounts, Source};
use straum_domain::{ColumnType, Datum, Store};

use super::references::*;
use super::support::*;
use crate::catalog::Checked;
use crate::error::Error;
use crate::load;
use crate::value::read;
use crate::wire::{Connection, ConnectionKind};

/// The types of the columns of `every`, in order.
const EVERY_TYPE: [ColumnType; 12] = [
    ColumnType::Int,
    ColumnType::Int,
    ColumnType::Int,
    ColumnType::Numeric { scale: 2 },
    ColumnType::Numeric { scale: 0 },
    ColumnType::Numeric { scale: 4 },
    ColumnType::Bool,
    ColumnType::Text,
    ColumnType::Text,
    ColumnType::Text,
    ColumnType::Time { zoned: true },
    ColumnType::Time { zoned: false },
];

#[test]
fn binary_copies_read_as_text_does() {
    let pg = postgres();
    let mut connection = Connection::connect(&pg.dsn, ConnectionKind::Sql).unwrap();
    create_every_type(&mut connection);
    let columns = "a, b, c, d, e, f, g, h, i, j, k, l";
    let rows = connection
        .rows(&format!("SELECT {columns} FROM every"))
        .unwrap();
    let mut texts: Vec<Vec<Datum>> = Vec::new();
    for fields in &rows {
        texts.push(read_text_row(fields));
    }
    let mut binaries = Vec::new();
    let names: Vec<&str> = columns.split(", ").collect();
    let every = Checked::public("every", "every", &names, EVERY_TYPE.to_vec());
    load::copy(&mut connection, &every, |batch| {
        binaries.extend(batch.chunks(EVERY_TYPE.len()).map(<[Datum]>::to_vec));
        Ok(())
    })
    .unwrap();
    assert_eq!(texts.len(), 7);
    assert_eq!(binaries, texts);
    // The infinities read as a null time, both ways.
    assert_eq!(&texts[5][10..], [Datum::Null, Datum::Null]);
    // The earliest time, before Christ, reads as a time, not a null.
    assert_ne!(texts[6][10], Datum::Null);
    assert_ne!(texts[6][11], Datum::Null);
}

/// Table `every`, a column of each type Straum reads, with the extremes
/// of each.
fn create_every_type(connection: &mut Connection) {
    // The session zone must not reach a timestamp's value in either form.
    connection
        .execute(
            "SET TIME ZONE 'Europe/Oslo';
         CREATE TABLE every (a int2, b int4, c int8, d numeric(10, 2), e numeric(18, 0),
                             f numeric(18, 4), g boolean, h text, i varchar(5), j char(4),
                             k timestamptz, l timestamp);
         INSERT INTO every VALUES
           (-32768, 2147483647, -9223372036854775808, -12345678.9, 999999999999999999,
            0.0001, true, E'tab\\there\\\\ ø', 'abc', 'ab', '2024-02-29 13:47:05.999999+00',
            '2024-02-29 13:47:05.999999'),
           (0, 0, 0, 0, 0, 12345678901234.5678, false, '', '', '', '1969-12-31 23:59:59.5+00',
            '1969-12-31 23:59:59.5'),
           (NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL),
           (1, 1, 1, 0.01, -1, -0.5, true, 'x', 'xxxxx', 'xxxx', '0001-01-01 00:00:00+00',
            '0001-01-01 00:00:00'),
           (2, 2, 2, 99999999.99, 10000, 100000000, false, 'y', 'y', 'y',
            '294276-12-31 23:59:59+00', '294276-12-31 23:59:59'),
           (3, 3, 3, 1, 1, 1, true, 'z', 'z', 'z', 'infinity', '-infinity'),
           (4, 4, 4, 2, 2, 2, false, 'w', 'w', 'w', '4714-11-24 00:00:00+00 BC',
            '4714-11-24 00:00:00 BC');
         SET TIME ZONE 'UTC';",
        )
        .unwrap();
}

/// A row of `every` as PostgreSQL prints it, read as text.
fn read_text_row(fields: &[Option<String>]) -> Vec<Datum> {
    let mut row = Vec::new();
    for (column_type, field) in EVERY_TYPE.iter().zip(fields) {
        let datum = match field {
            None => Datum::Null,
            Some(text) => read(*column_type, text).unwrap(),
        };
        row.push(datum);
    }
    row
}

#[test]
fn no_load_once_the_stream_runs() {
    let pg = postgres();
    let mut db = Db::new(&pg, "streaming", TABLES);
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    let start = source.open(&spec).unwrap().start;
    db.run(&Rng::new(1).trip(1));
    let commit = source.next_commit(LONG).unwrap().unwrap();
    assert!(commit.lsn > start);
    let mut store = Store::new(spec.clone());
    let error = source.load(&[id("trips")], &mut store, &LoadCounts::default());
    assert_eq!(
        error.unwrap_err().to_string(),
        "the replication stream stopped"
    );
}

/// A failed load ends the snapshot and the slot at once, rather than
/// through the wait before the next start: nothing of the source is left
/// to read from.
#[test]
fn a_failed_load_closes_the_source() {
    let pg = postgres();
    let mut db = Db::new(&pg, "failedload", TABLES);
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    source.open(&spec).unwrap();
    db.run(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
          WHERE application_name = 'straum' AND pid <> pg_backend_pid()",
    );
    let mut store = Store::new(spec.clone());
    assert!(
        source
            .load(&[id("zones")], &mut store, &LoadCounts::default())
            .is_err()
    );
    let stopped = "the replication stream stopped";
    assert_eq!(source.wal().unwrap_err().to_string(), stopped);
    assert_eq!(
        source
            .load(&[id("trips")], &mut store, &LoadCounts::default())
            .unwrap_err()
            .to_string(),
        stopped
    );
}

/// A fact table two views read is copied once for both: PostgreSQL counts
/// its rows read once, and each view answers as PostgreSQL does.
#[test]
fn a_table_two_views_read_is_scanned_once() {
    let pg = postgres();
    let mut db = Db::new(&pg, "onescan", TASKS);
    insert_task_tables(&mut db, &mut Rng::new(29), 200);
    let mut writer = db.writer(&[TASKS_SPEC, PICKS_SPEC].concat());
    started(&mut writer);
    let app = writer.app().clone();
    // PostgreSQL counts a session's scans for certain once it has ended.
    drop(writer);
    // Rows, not scans: the setup's own session counts a scan of the table
    // that reads none, whenever it sends its counts.
    let read = db.sql(
        "SELECT seq_tup_read FROM pg_stat_user_tables
          WHERE schemaname = '$S' AND relname = 'tasks' AND seq_tup_read > 0",
    );
    wait_for(&mut db.connection, &read);
    assert_eq!(db.rows(&read), [[Some("200".to_owned())]]);
    compare(&mut db, &app, &task_oracles()).expect("tasks ready");
    compare(&mut db, &app, &picks_oracles()).expect("picks ready");
}

thread_local! {
    /// What the writer under test reported, line by line.
    static REPORTED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn record(line: &str) {
    REPORTED.with_borrow_mut(|lines| lines.push(line.to_owned()));
}

/// A table `ANALYZE` has read is estimated at its rows, all of them, as
/// the sample takes up to 30000, and the size of its main fork; one
/// never analysed has rows unknown, never 0 or -1. The writer says both
/// before its first copy, and each table's rows after its copy.
#[test]
fn estimates_a_table_by_its_last_analyze() {
    let pg = postgres();
    let mut db = Db::new(&pg, "estimate", TABLES);
    db.run(
        "INSERT INTO $S.trips (id, at) SELECT n, to_timestamp(n) FROM generate_series(1, 1000) n;
         ANALYZE $S.trips;",
    );
    let spec = db.spec(SPEC);
    let mut source = db.pg();
    source.open(&spec).unwrap();
    let trips = source.estimate(&[id("trips")]).unwrap();
    assert_eq!(trips.rows, Some(1000));
    let size = db.rows("SELECT pg_relation_size('$S.trips')");
    assert_eq!(size[0][0], Some(trips.bytes.to_string()));
    assert!(trips.bytes > 0);
    let zones = source.estimate(&[id("zones")]).unwrap();
    let unknown = Estimate {
        rows: None,
        bytes: 0,
    };
    assert_eq!(zones, unknown);
    drop(source);

    let mut writer = db.writer(SPEC).reporting(record);
    started(&mut writer);
    let lines = REPORTED.take();
    assert!(
        lines[0].contains("estimated unknown rows and "),
        "{lines:?}"
    );
    assert!(lines[0].contains("estimate.zones (dim zones) unknown rows, 0.0 MiB, columns id, "));
    assert!(
        lines[0].contains("estimate.trips (view trips) 1000 rows, "),
        "{lines:?}"
    );
    assert!(lines[2].starts_with("loaded view trips in "), "{lines:?}");
    assert!(lines[2].contains(" s: 1000 rows, "), "{lines:?}");
}

/// Timeouts the database sets for every session cancel none of Straum's
/// statements, which a load and its snapshot outlast: each connection
/// switches them off.
#[test]
fn loads_whatever_timeouts_the_database_sets() {
    let pg = postgres();
    let mut db = Db::new(&pg, "timeouts", TABLES);
    for setting in [
        "statement_timeout",
        "lock_timeout",
        "idle_in_transaction_session_timeout",
        "transaction_timeout",
    ] {
        db.run(&format!("ALTER DATABASE postgres SET {setting} = '1ms'"));
    }
    let mut writer = db.writer(SPEC);
    started(&mut writer);
    db.run(&Rng::new(1).trip(1));
    applied(&mut writer);
    // A session of its own takes the database's setting, and is cut.
    let mut cut = Connection::connect(&pg.dsn, ConnectionKind::Sql).unwrap();
    let error = cut.rows("SET statement_timeout = '1ms'; SELECT pg_sleep(0.1)");
    assert!(error.unwrap_err().to_string().contains("statement timeout"));
}

/// `CopyOutResponse` for `columns` binary columns.
fn copy_out(columns: i16) -> Vec<u8> {
    let mut body = vec![1];
    body.extend(columns.to_be_bytes());
    for _ in 0..columns {
        body.extend(1_i16.to_be_bytes());
    }
    message(b'H', &body)
}

/// The signature, no flags and a header extension of `extension`.
fn copy_header(extension: &[u8]) -> Vec<u8> {
    let mut header = b"PGCOPY\n\xff\r\n\0".to_vec();
    header.extend(0_i32.to_be_bytes());
    header.extend(u32::try_from(extension.len()).unwrap().to_be_bytes());
    header.extend(extension);
    header
}

/// A binary copy row of `int4` fields, `None` for null.
fn copy_tuple(fields: &[Option<i32>]) -> Vec<u8> {
    let mut row = i16::try_from(fields.len()).unwrap().to_be_bytes().to_vec();
    for field in fields {
        match field {
            None => row.extend((-1_i32).to_be_bytes()),
            Some(value) => {
                row.extend(4_i32.to_be_bytes());
                row.extend(value.to_be_bytes());
            }
        }
    }
    row
}

/// The field count that ends a binary copy.
const COPY_END: [u8; 2] = (-1_i16).to_be_bytes();

/// The rows a fake server's copy of two `int4` columns gives, sent as
/// `messages` `CopyData` bodies, then `after`.
fn copied(messages: &[Vec<u8>], after: &[u8]) -> Result<Vec<Vec<Datum>>, Error> {
    let mut reply = copy_out(2);
    for body in messages {
        reply.extend(copy_data(body));
    }
    reply.extend(after);
    let mut connection = connect(vec![now(signed_in()), now(reply)], ConnectionKind::Sql)?;
    let mut rows = Vec::new();
    load::copy(&mut connection, &two_ints(), |batch| {
        rows.extend(batch.chunks(2).map(<[Datum]>::to_vec));
        Ok(())
    })?;
    Ok(rows)
}

/// Two `int4` columns, `a` and `b` of `t`, as a fake copy sends.
fn two_ints() -> Checked {
    Checked::public("t", "t", &["a", "b"], vec![ColumnType::Int; 2])
}

/// The end of a copy of two rows, then `ReadyForQuery`.
fn copy_done() -> Vec<u8> {
    let mut messages = message(b'c', b"");
    messages.extend(message(b'C', b"COPY 2\0"));
    messages.extend(message(b'Z', b"I"));
    messages
}

#[test]
fn reads_binary_copies() {
    let rows = [
        vec![Datum::Int(1), Datum::Null],
        vec![Datum::Int(-3), Datum::Int(4)],
    ];
    // As PostgreSQL sends it: the header with the first row, then a
    // message per row and one for the end.
    let mut first = copy_header(b"");
    first.extend(copy_tuple(&[Some(1), None]));
    let sent = [first, copy_tuple(&[Some(-3), Some(4)]), COPY_END.to_vec()];
    assert_eq!(copied(&sent, &copy_done()).unwrap(), rows);
    // Header, rows and end in one message, with a header extension.
    let mut one = copy_header(b"ext");
    one.extend(copy_tuple(&[Some(1), None]));
    one.extend(copy_tuple(&[Some(-3), Some(4)]));
    one.extend(COPY_END);
    assert_eq!(copied(&[one], &copy_done()).unwrap(), rows);
    // An empty table.
    let mut empty = copy_header(b"");
    empty.extend(COPY_END);
    assert_eq!(
        copied(&[empty], &copy_done()).unwrap(),
        Vec::<Vec<Datum>>::new()
    );
}

#[test]
fn copies_hand_over_bounded_batches() {
    let count = 2 * load::BATCH + 7;
    let mut sent = vec![copy_header(b"")];
    for row in 0..count {
        sent.push(copy_tuple(&[Some(i32::try_from(row).unwrap()), None]));
    }
    sent.push(COPY_END.to_vec());
    let mut reply = copy_out(2);
    for body in &sent {
        reply.extend(copy_data(body));
    }
    reply.extend(copy_done());
    let mut connection = connect(
        vec![now(signed_in()), now(reply.clone())],
        ConnectionKind::Sql,
    )
    .unwrap();
    let mut batches = Vec::new();
    let mut next = 0;
    load::copy(&mut connection, &two_ints(), |batch| {
        batches.push(batch.len() / 2);
        for row in batch.chunks(2) {
            assert_eq!(row, [Datum::Int(next), Datum::Null]);
            next += 1;
        }
        Ok(())
    })
    .unwrap();
    let full = load::BATCH;
    assert_eq!(batches, [full, full, 7]);

    // A failed batch stops the copy, and its error is the one returned.
    let mut connection = connect(vec![now(signed_in()), now(reply)], ConnectionKind::Sql).unwrap();
    let mut calls = 0;
    let error = load::copy(&mut connection, &two_ints(), |_| {
        calls += 1;
        Err(Error::Stopped)
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "the replication stream stopped");
    assert_eq!(calls, 1);
}

/// `parts` one after another.
fn joined(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

#[test]
fn binary_copy_errors() {
    let header = copy_header(b"");
    let row = copy_tuple(&[Some(1), Some(2)]);
    for (sent, expected) in broken_copies(&header, &row) {
        let error = copied(&sent, &copy_done()).unwrap_err().to_string();
        assert!(error.starts_with(expected), "{error}");
    }
    // The server fails mid-copy, or sends what a copy doesn't.
    let mut failed = error_response("57014", "canceling statement");
    failed.extend(message(b'Z', b"I"));
    let error = copied(&[joined(&[&header, &row])], &failed).unwrap_err();
    assert_eq!(error.to_string(), "PostgreSQL: canceling statement (57014)");
    let error = copied(&[header], &message(b'D', &0_i16.to_be_bytes())).unwrap_err();
    assert_eq!(error.to_string(), "PostgreSQL sent a message during a copy");
    // Notices and parameters along the way are fine.
    let mut chatty = message(b'N', b"SNOTICE\0Mhello\0\0");
    chatty.extend(message(b'S', b"TimeZone\0UTC\0"));
    chatty.extend(copy_done());
    let mut empty = copy_header(b"");
    empty.extend(COPY_END);
    assert_eq!(copied(&[empty], &chatty).unwrap(), Vec::<Vec<Datum>>::new());
}

/// Copies a server could send that are broken, from `header` and a two
/// column `row`, each with the start of the error it gets.
// `'static`: each expected error is a literal written here.
fn broken_copies(header: &[u8], row: &[u8]) -> [(Vec<Vec<u8>>, &'static str); 10] {
    let mut cut = joined(&[header, row]);
    cut.pop();
    let mut cut_header = copy_header(b"ext");
    cut_header.pop();
    [
        (
            vec![b"1\t2\n".to_vec()],
            "PostgreSQL sent a copy that isn't binary",
        ),
        (
            vec![header[..13].to_vec()],
            "PostgreSQL sent a copy row cut short",
        ),
        (vec![cut_header], "PostgreSQL sent a copy header cut short"),
        (
            vec![joined(&[header, &copy_tuple(&[Some(1)])])],
            "PostgreSQL sent a copy row of 1 columns, not 2",
        ),
        (
            vec![joined(&[header, &row[..3]])],
            "PostgreSQL sent a copy row cut short",
        ),
        (vec![cut], "PostgreSQL sent a copy value cut short"),
        (
            vec![joined(&[header, row, &COPY_END, row])],
            "PostgreSQL sent data after the end of a copy",
        ),
        (
            vec![joined(&[header, row])],
            "PostgreSQL sent a copy without its end",
        ),
        (
            vec![joined(&[
                header,
                &2_i16.to_be_bytes(),
                &4_i32.to_be_bytes(),
                &1_i32.to_be_bytes(),
                &3_i32.to_be_bytes(),
                &[0, 0, 1],
                &COPY_END,
            ])],
            "can't read \"\\\\x000001\" as an integer in column b of public.t",
        ),
        // Only -1 is null: a length below it isn't a value.
        (
            vec![joined(&[
                header,
                &2_i16.to_be_bytes(),
                &(-2_i32).to_be_bytes(),
                &4_i32.to_be_bytes(),
                &1_i32.to_be_bytes(),
                &COPY_END,
            ])],
            "PostgreSQL sent a copy value of length -2",
        ),
    ]
}
