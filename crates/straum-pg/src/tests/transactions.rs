//! How transactions end in the WAL, checked against `pg_waldump`, and how
//! a load lines up with the physical stream: a load in a snapshot plus
//! the commits the snapshot doesn't see is the table. Also the tables
//! rewritten or dropped while the stream runs.

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use straum_app::{Commit, Lsn};
use straum_domain::{ColumnType, Datum};
use testcontainers::ImageExt;

use super::decoding::Pipeline;
use super::references::*;
use super::support::*;
use crate::physical::Physical;
use crate::source::parse_lsn;
use crate::value::{self};
use crate::wal::heap::{Heap, HeapChange};
use crate::wal::transactions::{Committed, Consistency, TransactionSnapshot, Transactions};
use crate::wal::xact::{self, Event};
use crate::wire::{Connection, ConnectionKind};

/// Straum reads how each transaction ends, with its subtransactions, and
/// which run at each `RUNNING_XACTS`, as `pg_waldump` describes them:
/// commits, aborts, savepoints kept and rolled back, past 64
/// subtransactions, two-phase commit, and catalog changes, whose commit
/// records carry relation files, dropped stats and invalidations.
#[test]
fn reads_the_endings_pg_waldump_reads() {
    let pg = plain(image(&tag()).with_cmd([
        "-c",
        "wal_level=replica",
        "-c",
        "max_prepared_transactions=4",
    ]));
    let mut db = Db::new(&pg, "endings", "CREATE TABLE $S.t (id int)");
    let (mut physical, start) = Physical::start(&pg.dsn).unwrap();
    let mut open = Connection::connect(&pg.dsn, ConnectionKind::Sql).unwrap();
    open.execute("BEGIN").unwrap();
    open.execute(&db.sql("INSERT INTO $S.t VALUES (0)"))
        .unwrap();
    end_transactions_every_way(&mut db);
    open.execute("COMMIT").unwrap();
    db.run("ALTER TABLE $S.t ADD note text DEFAULT 'x'");
    db.run("CREATE INDEX ON $S.t (id)");
    db.run("ANALYZE $S.t");
    let last = db.insert_lsn();
    db.run("SELECT pg_switch_wal()");
    let ours = endings_read_before(&mut physical, last);
    let directory = db.rows("SHOW data_directory")[0][0].clone().unwrap();
    let arguments = [
        "-s".to_owned(),
        start.to_string(),
        "-e".to_owned(),
        last.to_string(),
    ];
    let out = pg.waldump(&directory, &arguments);
    let mut theirs: Vec<Ending> = Vec::new();
    for line in out.lines() {
        if let Some(ending) = described_ending(line) {
            theirs.push(ending);
        }
    }
    for (ours, theirs) in ours.iter().zip(&theirs) {
        assert_eq!(ours, theirs);
    }
    assert_eq!(ours.len(), theirs.len());
    assert_every_way_of_ending(&ours);
}

/// The endings the physical stream reads up to the switch record past
/// `last`, of those before `last`.
fn endings_read_before(physical: &mut Physical, last: Lsn) -> Vec<Ending> {
    let mut endings = Vec::new();
    let deadline = Instant::now() + LONG;
    loop {
        assert!(Instant::now() < deadline, "no switch record past {last}");
        let Some(record) = physical.next_record().unwrap() else {
            continue;
        };
        if let Some(event) = xact::event(&record, physical.major()).unwrap()
            && record.lsn < last.0
        {
            endings.push(Ending {
                lsn: record.lsn,
                text: described_event(&event),
            });
        }
        if record.is_switch() && record.lsn >= last.0 {
            return endings;
        }
    }
}

/// Commits, aborts and `RUNNING_XACTS`, one transaction with 100
/// subtransactions, one running at a checkpoint, and relcache
/// invalidations are all among `ours`.
fn assert_every_way_of_ending(ours: &[Ending]) {
    assert!(
        count_starting(ours, "COMMIT") >= 8
            && count_starting(ours, "ABORT") >= 3
            && count_starting(ours, "RUNNING") >= 2
    );
    let mut many_subxacts = false;
    let mut running_at_a_checkpoint = false;
    let mut relcache = 0;
    for ending in ours {
        let text = &ending.text;
        if text.matches(' ').count() > 100 {
            many_subxacts = true;
        }
        if text.starts_with("RUNNING") && !text.ends_with("[]") {
            running_at_a_checkpoint = true;
        }
        if text.contains("relcache") {
            relcache += 1;
        }
    }
    assert!(many_subxacts, "100 subxacts");
    assert!(
        running_at_a_checkpoint,
        "a transaction running at a checkpoint"
    );
    assert!(relcache >= 3, "relcache invalidations");
}

/// Commits, aborts, savepoints kept and rolled back, past 64
/// subtransactions, two-phase commit and catalog changes, with a
/// checkpoint while two are prepared.
fn end_transactions_every_way(db: &mut Db) {
    db.run("CHECKPOINT");
    db.run("INSERT INTO $S.t VALUES (1)");
    db.run("BEGIN; INSERT INTO $S.t VALUES (2); ROLLBACK");
    db.run(
        "BEGIN; INSERT INTO $S.t VALUES (3); SAVEPOINT a; INSERT INTO $S.t VALUES (4);
         SAVEPOINT b; INSERT INTO $S.t VALUES (5); ROLLBACK TO b; COMMIT",
    );
    let mut many = String::new();
    for savepoint in 0..100 {
        write!(
            many,
            "SAVEPOINT s{savepoint}; INSERT INTO $S.t VALUES ({savepoint});"
        )
        .unwrap();
    }
    db.run(&format!("BEGIN; {many} COMMIT"));
    db.run(&format!("BEGIN; {many} ROLLBACK"));
    db.run(
        "BEGIN; INSERT INTO $S.t VALUES (6); SAVEPOINT a; INSERT INTO $S.t VALUES (7);
         PREPARE TRANSACTION 'kept'",
    );
    db.run("BEGIN; INSERT INTO $S.t VALUES (8); PREPARE TRANSACTION 'dropped'");
    db.run("CHECKPOINT");
    db.run("COMMIT PREPARED 'kept'");
    db.run("ROLLBACK PREPARED 'dropped'");
    db.run("BEGIN; CREATE TABLE $S.gone (id int); INSERT INTO $S.gone VALUES (1); COMMIT");
    db.run("BEGIN; DROP TABLE $S.gone; CREATE TABLE $S.other (id int); COMMIT");
    db.run(
        "BEGIN; CREATE TABLE $S.prepared (id int); DROP TABLE $S.other;
         PREPARE TRANSACTION 'ddl'",
    );
    db.run("COMMIT PREPARED 'ddl'");
}

/// A transaction's end or a `RUNNING_XACTS` record: its LSN, and in
/// `text` what [`described_ending`] reads of `pg_waldump`'s line.
#[derive(Debug, PartialEq, Eq)]
struct Ending {
    lsn: u64,
    text: String,
}

/// An event as [`described_ending`] puts it.
fn described_event(event: &Event) -> String {
    match event {
        Event::Commit(commit) => format!("COMMIT {}", ending(commit)),
        Event::Abort(abort) => format!("ABORT {}", ending(abort)),
        Event::Running(running) => format!(
            "RUNNING {} {} {:?}",
            running.next_xid, running.oldest_xid, running.xids
        ),
    }
}

/// How many of `endings` start with `kind`.
fn count_starting(endings: &[Ending], kind: &str) -> usize {
    let mut count = 0;
    for ending in endings {
        if ending.text.starts_with(kind) {
            count += 1;
        }
    }
    count
}

/// An ending as [`described_ending`] puts it: xid, time as `pg_waldump`
/// prints it, subtransactions.
fn ending(ending: &xact::Ending) -> String {
    let Datum::Int(seconds) = value::time(ending.time) else {
        panic!("{ending:?}")
    };
    let micros = ending.time.rem_euclid(1_000_000);
    let mut text = format!("{} {seconds}.{micros:06}", ending.xid);
    for subxact in &ending.subxacts {
        write!(text, " {subxact}").unwrap();
    }
    for invalidated in &ending.invalidated {
        write!(text, " relcache {}", invalidated.relation).unwrap();
    }
    text
}

/// The part of `pg_waldump`'s description after `prefix`.
// `'a`: the part returned lies inside `description`, not a copy.
fn described<'a>(description: &'a str, prefix: &str) -> Option<&'a str> {
    for part in description.split("; ") {
        if let Some(rest) = part.strip_prefix(prefix) {
            return Some(rest);
        }
    }
    None
}

/// A transaction's end or a `RUNNING_XACTS` line of `pg_waldump`.
fn described_ending(line: &str) -> Option<Ending> {
    let lsn = parse_lsn(after(line, "lsn:")?.split(',').next()?.trim())?.0;
    let record_xid = after(line, "tx:")?.split(',').next()?.trim();
    let description = after(line, "desc: ")?;
    let (kind, rest) = description.split_once(' ')?;
    let text = match kind {
        "COMMIT" | "COMMIT_PREPARED" | "ABORT" | "ABORT_PREPARED" => {
            described_end(kind, rest, record_xid)?
        }
        "RUNNING_XACTS" => {
            let words: Vec<&str> = rest.split_whitespace().collect();
            let (next, oldest) = (words[1], words[5].trim_end_matches(';'));
            let xids = running_xids(rest);
            format!("RUNNING {next} {oldest} {xids:?}")
        }
        _ => return None,
    };
    Some(Ending { lsn, text })
}

/// A commit or abort `pg_waldump` describes as `kind` then `rest`, of the
/// transaction the line names unless `rest` names a prepared one.
fn described_end(kind: &str, rest: &str, record_xid: &str) -> Option<String> {
    let (xid, rest) = match rest.split_once(": ") {
        Some((xid, time)) if all_digits(xid) => (xid, time),
        _ => (record_xid, rest),
    };
    // `2024-01-15 10:30:00.000001 UTC`, as Unix seconds.
    let (whole, fraction) = rest.split(';').next()?.trim().split_once('.')?;
    let Ok(Datum::Int(seconds)) = value::read(ColumnType::Time { zoned: false }, whole) else {
        return None;
    };
    let time = format!("{seconds}.{}", fraction.strip_suffix(" UTC")?);
    let mut text = format!(
        "{} {xid} {time}",
        &kind[..kind.find('_').unwrap_or(kind.len())]
    );
    let subxacts = described(rest, "subxacts: ").unwrap_or_default();
    for subxact in subxacts.split_whitespace() {
        write!(text, " {subxact}").unwrap();
    }
    let messages = described(rest, "inval msgs: ").unwrap_or_default();
    let words: Vec<&str> = messages.split_whitespace().collect();
    for pair in words.windows(2) {
        if pair[0] == "relcache" {
            write!(text, " relcache {}", pair[1]).unwrap();
        }
    }
    Some(text)
}

/// Whether `text` is all ASCII digits.
fn all_digits(text: &str) -> bool {
    for byte in text.bytes() {
        if !byte.is_ascii_digit() {
            return false;
        }
    }
    true
}

/// The xids a `RUNNING_XACTS` description lists.
fn running_xids(description: &str) -> Vec<u32> {
    let mut xids = Vec::new();
    for part in description.split("; ") {
        if !part.contains(" xacts:") {
            continue;
        }
        let Some((_, listed)) = part.split_once(':') else {
            return xids;
        };
        for xid in listed.split_whitespace() {
            xids.push(xid.parse().unwrap());
        }
        return xids;
    }
    xids
}

/// A table rewritten, truncated or dropped no longer reads as loaded:
/// its old pages are gone, and the physical stream says so with an
/// `Unfit` for each, at the commit that did it. DDL that keeps the file
/// says nothing.
#[test]
fn rewritten_and_dropped_tables_are_unfit() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "rewrites", TABLES);
    db.run("INSERT INTO $S.zones VALUES (1, 'Queens', 1.5), (2, 'Bronx', NULL)");
    let mut rng = Rng::new(5);
    for id in 0..20 {
        db.run(&rng.trip(id));
    }
    db.run("INSERT INTO $S.logins VALUES (1, now(), 1)");
    let (mut stream, _) = Physical::start(&pg.dsn).unwrap();
    db.run("SELECT pg_log_standby_snapshot()");
    let spec = db.spec(SPEC);
    let mut pipeline = Pipeline::new(&pg, &spec, &db.name, stream.major());
    let steps = [
        ("ALTER TABLE $S.trips ADD extra int", None),
        // A value the spec reads, TOASTed out of line: the page holds a
        // pointer, and the chunks may be gone before Straum could read
        // them.
        (
            "INSERT INTO $S.trips (id, kind) SELECT 999, string_agg(md5(i::text), '')
             FROM generate_series(1, 400) AS i",
            Some("trips"),
        ),
        ("VACUUM FULL $S.zones", Some("zones")),
        ("TRUNCATE $S.logins", Some("logins")),
        ("ALTER TABLE $S.trips ALTER tip TYPE bigint", Some("trips")),
        ("DROP TABLE $S.zones", Some("zones")),
    ];
    for (sql, unfit) in steps {
        db.run(sql);
        let commits = pipeline.through_now(&mut db, &mut stream);
        let expected: Vec<String> = match unfit {
            Some(table) => vec![table.to_owned()],
            None => Vec::new(),
        };
        assert_eq!(unfit_tables(&commits), expected, "{sql}");
    }
}

/// The tables `commits` say no longer fit, in order.
fn unfit_tables(commits: &[Commit]) -> Vec<String> {
    let mut names = Vec::new();
    for commit in commits {
        for change in &commit.changes {
            if let straum_app::Change::Unfit { table } = change {
                names.push(table.to_string());
            }
        }
    }
    names
}

/// A load in a snapshot taken once the physical stream is consistent,
/// plus the commits that snapshot doesn't see, is the table: with
/// writers inserting and deleting all along, and a transaction open from
/// before the stream started until after.
#[test]
fn lines_the_load_up_with_the_stream() {
    let pg = postgres_at_replica(&tag());
    let mut db = Db::new(&pg, "aligned", TABLES);
    let mut open = Connection::connect(&pg.dsn, ConnectionKind::Sql).unwrap();
    open.execute("BEGIN").unwrap();
    open.execute(&db.sql("INSERT INTO $S.trips (id) VALUES (-1)"))
        .unwrap();
    // The redo point Straum starts from is past that insert.
    db.run("CHECKPOINT");
    let stop = Arc::new(AtomicBool::new(false));
    let mut writers = Vec::new();
    for index in 0..3 {
        writers.push(insert_and_delete_until(&stop, &db, index));
    }
    thread::sleep(Duration::from_millis(200));
    let (stream, _) = Physical::start(&pg.dsn).unwrap();
    // Without it the first RUNNING_XACTS comes with the next checkpoint
    // or within 15 seconds.
    db.run("SELECT pg_log_standby_snapshot()");
    let mut connection = Connection::connect(&pg.dsn, ConnectionKind::Sql).unwrap();
    let TableFile { relation, .. } = relation_file(&mut db, "trips");
    let mut reader = WalReader {
        heap: Heap::new([relation].into(), stream.major()),
        stream,
        transactions: Transactions::new(ROW_LIMIT),
    };
    let mut open = Some(open);
    let before = read_until_consistent(&mut reader, &mut open);
    let Load {
        snapshot,
        rows: loaded,
    } = load(&mut connection, &db);
    if let Some(mut open) = open {
        open.execute("COMMIT").unwrap();
    }
    let unseen_before = unseen_in(&before, &snapshot);
    checkpoint_then_stop(&mut db, &stop, writers);
    let last = db.insert_lsn();
    db.run("SELECT pg_switch_wal()");
    let unseen_after = unseen_through(&mut reader, &snapshot, last);
    let total: i64 = db.rows("SELECT count(*) FROM $S.trips")[0][0]
        .as_deref()
        .unwrap()
        .parse()
        .unwrap();
    let after = unseen_after.commits;
    assert!(after > 20, "{after} commits after the load");
    let count = loaded + unseen_before.rows + unseen_after.rows;
    let before = unseen_before.commits;
    assert_eq!(count, total, "loaded {loaded}, {before} before");
}

/// Reads until the stream is consistent and returns the transactions it
/// committed whole on the way. The transaction in `open` holds the stream
/// back until it ends, so it commits once the first record has come.
fn read_until_consistent(reader: &mut WalReader, open: &mut Option<Connection>) -> Vec<Committed> {
    let mut before = Vec::new();
    let deadline = Instant::now() + LONG;
    while reader.transactions.consistency() != Consistency::Consistent {
        assert!(Instant::now() < deadline, "never consistent");
        let Advanced { committed, record } = reader.advance();
        before.extend(committed);
        if record.is_some()
            && let Some(mut connection) = open.take()
        {
            thread::sleep(Duration::from_millis(50));
            assert_ne!(reader.transactions.consistency(), Consistency::Consistent);
            connection.execute("COMMIT").unwrap();
        }
    }
    before
}

/// Checkpoints while the writers write: a page changed twice between a
/// redo point and its checkpoint record has an image only the first
/// time, so Straum must keep it past the record. Then the writers stop.
fn checkpoint_then_stop(db: &mut Db, stop: &AtomicBool, writers: Vec<thread::JoinHandle<()>>) {
    for _ in 0..3 {
        thread::sleep(Duration::from_millis(100));
        db.run("CHECKPOINT");
    }
    thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::Relaxed);
    for writer in writers {
        writer.join().unwrap();
    }
}

/// What a load in a repeatable-read snapshot reads: the snapshot, and the
/// trips it sees.
struct Load {
    snapshot: TransactionSnapshot,
    rows: i64,
}

/// Counts the trips in a repeatable-read snapshot.
fn load(connection: &mut Connection, db: &Db) -> Load {
    connection
        .execute("BEGIN ISOLATION LEVEL REPEATABLE READ")
        .unwrap();
    let snapshot = connection.rows("SELECT pg_current_snapshot()").unwrap();
    let snapshot = TransactionSnapshot::parse(snapshot[0][0].as_deref().unwrap()).unwrap();
    let rows = connection
        .rows(&db.sql("SELECT count(*) FROM $S.trips"))
        .unwrap();
    let rows = rows[0][0].as_deref().unwrap().parse().unwrap();
    connection.execute("COMMIT").unwrap();
    Load { snapshot, rows }
}

/// The physical stream read record by record into its pages and held
/// transactions.
struct WalReader {
    stream: Physical,
    heap: Heap,
    transactions: Transactions,
}

/// What reading one record gave: the transaction it commits whole, if it
/// does, and the record, if one came.
struct Advanced {
    committed: Option<Committed>,
    record: Option<crate::wal::Record>,
}

impl WalReader {
    /// Reads the next record into the pages and the held transactions.
    fn advance(&mut self) -> Advanced {
        let Some(record) = self.stream.next_record().unwrap() else {
            return Advanced {
                committed: None,
                record: None,
            };
        };
        let mut changes = self.heap.apply(&record).unwrap();
        self.transactions.hold(record.xid, record.lsn, &mut changes);
        let committed = match xact::event(&record, self.stream.major()).unwrap() {
            Some(event) => self
                .transactions
                .event(event, record.lsn, record.next())
                .unwrap(),
            None => None,
        };
        Advanced {
            committed,
            record: Some(record),
        }
    }
}

/// The commits a snapshot doesn't see: the rows they add, less those
/// they delete, and how many there are.
struct Unseen {
    rows: i64,
    commits: u32,
}

/// The commits of `committed` that `snapshot` doesn't see.
fn unseen_in(committed: &[Committed], snapshot: &TransactionSnapshot) -> Unseen {
    let mut unseen = Unseen {
        rows: 0,
        commits: 0,
    };
    for transaction in committed {
        if !snapshot.sees(transaction.xid) {
            unseen.rows += net_rows(transaction);
            unseen.commits += 1;
        }
    }
    unseen
}

/// Reads to the switch record at or past `last`, counting the commits
/// `snapshot` doesn't see.
fn unseen_through(reader: &mut WalReader, snapshot: &TransactionSnapshot, last: Lsn) -> Unseen {
    let mut unseen = Unseen {
        rows: 0,
        commits: 0,
    };
    let deadline = Instant::now() + LONG;
    loop {
        assert!(Instant::now() < deadline, "no switch record past {last}");
        let Advanced { committed, record } = reader.advance();
        if let Some(committed) = committed
            && !snapshot.sees(committed.xid)
        {
            unseen.rows += net_rows(&committed);
            unseen.commits += 1;
        }
        if let Some(record) = record
            && record.is_switch()
            && record.lsn >= last.0
        {
            return unseen;
        }
    }
}

/// A writer thread inserting and deleting trips in `db` until `stop`,
/// each transaction with a savepoint rolled back.
fn insert_and_delete_until(stop: &Arc<AtomicBool>, db: &Db, index: u64) -> thread::JoinHandle<()> {
    let stop = Arc::clone(stop);
    let dsn = db.dsn.clone();
    let schema = db.name.clone();
    thread::spawn(move || {
        let mut connection = Connection::connect(&dsn, ConnectionKind::Sql).unwrap();
        let mut id = index * 1_000_000;
        while !stop.load(Ordering::Relaxed) {
            id += 1;
            connection
                .execute(&format!(
                    "BEGIN; INSERT INTO {schema}.trips (id) VALUES ({id}), ({}); SAVEPOINT a;
                 INSERT INTO {schema}.trips (id) VALUES ({}); ROLLBACK TO a;
                 DELETE FROM {schema}.trips WHERE id = {}; COMMIT",
                    id + 500_000,
                    id + 700_000,
                    id - 1
                ))
                .unwrap();
        }
    })
}

/// The rows `committed` adds, less those it deletes.
fn net_rows(committed: &Committed) -> i64 {
    let mut rows = 0;
    for change in &committed.changes {
        rows += match change {
            HeapChange::Insert { .. } => 1,
            HeapChange::Update { .. } => 0,
            HeapChange::Delete { .. } => -1,
        };
    }
    rows
}
