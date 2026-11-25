//! Tests of the pipeline: records built as PostgreSQL writes them, read
//! through it with a load snapshot and table layouts, and the commits that
//! come out. They also hold it to its limits and to the allocations a row
//! may cost.

use std::cell::RefCell;
use std::collections::HashMap;

use straum_app::{Change, Drift};
use straum_domain::{ColumnType, Datum, Identifier};

use super::*;
use crate::pgoutput::{Relation, RelationColumn};
use crate::value::INT4;
use crate::wal::messages::Layout;
use crate::wal::test_support::{BlockSpec, DATABASE, FILE, Spec, TEST_LSN, assemble};
use crate::wal::test_support::{allocations, decode};
use crate::wal::tuple::{Attribute, Kind};
use crate::wal::xact::tests::{
    commit_with_subxacts, enum_commit, invalidating_commit, running_main, statistic_commit,
};
use crate::wal::xact::{STANDBY_RUNNING_XACTS, XACT_COMMIT, XACT_HAS_INFO};
use crate::wal::{RESOURCE_MANAGER_HEAP, RESOURCE_MANAGER_STANDBY, RESOURCE_MANAGER_TRANSACTION};

use crate::wal::heap::{HEAP_INSERT, INIT_PAGE};
use crate::wal::record::RECORD_ALIGNMENT;
use crate::wal::test_support::{ROW_HEADER, ROW_PADDING};
/// The commit time of every test commit, in microseconds since 2000.
const COMMIT_TIME: i64 = 7;
/// `pg_class.oid` of the table read, its layout's and its check's.
const TABLE_OID: u32 = 77;

/// `public.t (id int4)`, read by the spec as `t`.
fn table() -> Layout {
    Layout {
        file: FILE,
        relation: Relation {
            id: TABLE_OID,
            schema: "public".to_owned(),
            name: "t".to_owned(),
            columns: vec![RelationColumn {
                name: "id".to_owned(),
                type_oid: INT4,
                typmod: -1,
            }],
        },
        attributes: vec![Attribute {
            length: 4,
            align: 4,
            kind: Kind::Read(ColumnType::Int),
        }],
        columns: vec![0],
        missing: vec![Datum::Null],
    }
}

/// A fact named `name` reading `public.t`.
fn reading_t(name: &str) -> Checked {
    Checked {
        oid: TABLE_OID,
        ..Checked::public(name, "t", &["id"], vec![ColumnType::Int])
    }
}

/// The table's layout, and no enum labels.
fn layouts() -> Layouts {
    Layouts {
        tables: vec![table()],
        labels: HashMap::new(),
    }
}

fn pipeline(max_bytes: usize) -> Pipeline {
    Pipeline::new(vec![reading_t("t")], layouts(), 18, 2, max_bytes, 1 << 30)
}

/// Records at increasing LSNs, as the stream reads them.
struct Wal {
    lsn: u64,
    block: u32,
}

impl Wal {
    fn record(
        &mut self,
        resource_manager: u8,
        info: u8,
        xid: u32,
        main: Vec<u8>,
        blocks: Vec<BlockSpec>,
    ) -> Record {
        let spec = Spec {
            xid,
            resource_manager,
            info,
            toplevel_xid: None,
            origin: false,
            blocks,
            main,
        };
        let bytes = assemble(&spec);
        let lsn = self.lsn;
        let end = lsn + bytes.len() as u64;
        self.lsn = end.next_multiple_of(RECORD_ALIGNMENT);
        decode(lsn, end, bytes).unwrap()
    }

    /// A row `id` inserted on a page of its own.
    fn insert(&mut self, xid: u32, id: i32) -> Record {
        // `xl_heap_header`: one column, no flags, then the row.
        let mut data = 1u16.to_le_bytes().to_vec();
        data.extend(0u16.to_le_bytes());
        data.push(ROW_HEADER);
        data.extend([0; ROW_PADDING]);
        data.extend(id.to_le_bytes());
        let block = BlockSpec {
            id: 0,
            fork: 0,
            will_init: true,
            file: FILE,
            number: self.block,
            image: None,
            data,
        };
        self.block += 1;
        // `xl_heap_insert`: slot 1, no flags.
        let mut main = 1u16.to_le_bytes().to_vec();
        main.push(0);
        let info = HEAP_INSERT | INIT_PAGE;
        self.record(RESOURCE_MANAGER_HEAP, info, xid, main, vec![block])
    }

    fn commit(&mut self, xid: u32) -> Record {
        let main = COMMIT_TIME.to_le_bytes().to_vec();
        self.record(
            RESOURCE_MANAGER_TRANSACTION,
            XACT_COMMIT,
            xid,
            main,
            Vec::new(),
        )
    }

    fn running(&mut self, next: u32, xids: &[u32]) -> Record {
        let oldest = xids.iter().copied().min().unwrap_or(next);
        let count = u32::try_from(xids.len()).unwrap();
        let main = running_main(count, next, oldest, xids);
        let info = STANDBY_RUNNING_XACTS;
        self.record(RESOURCE_MANAGER_STANDBY, info, 0, main, Vec::new())
    }
}

fn no_layouts(_: TransactionId) -> Result<Layouts, Error> {
    unreachable!("no commit here invalidates a table")
}

fn failing_layouts(_: TransactionId) -> Result<Layouts, Error> {
    Err(Error::Stopped)
}

/// No tables any more, noting in `asked` the xid they must show.
fn no_tables_for(asked: &RefCell<Vec<TransactionId>>, xid: TransactionId) -> Layouts {
    asked.borrow_mut().push(xid);
    Layouts::default()
}

/// The table as it was, noting in `asked` the xid it must show.
fn the_table_for(asked: &RefCell<Vec<TransactionId>>, xid: TransactionId) -> Layouts {
    asked.borrow_mut().push(xid);
    layouts()
}

/// The commits `pipeline` releases as it takes the snapshot `text`.
fn see_all(pipeline: &mut Pipeline, text: &str) -> Result<Vec<Commit>, Error> {
    let snapshot = TransactionSnapshot::parse(text).unwrap();
    let (_, commits) = pipeline.see(snapshot, Lsn(u64::MAX))?;
    Ok(commits)
}

fn ids(commits: &[Commit]) -> Vec<i64> {
    let mut ids = Vec::new();
    for commit in commits {
        for change in &commit.changes {
            let Change::Insert { row, .. } = change else {
                unreachable!()
            };
            let [Datum::Int(id)] = row[..] else {
                unreachable!()
            };
            ids.push(id);
        }
    }
    ids
}

fn read_one(pipeline: &mut Pipeline, record: &Record) -> Vec<Commit> {
    pipeline.record(record, no_layouts).unwrap()
}

fn read_all(pipeline: &mut Pipeline, records: &[Record]) -> Result<Vec<Commit>, Error> {
    let mut commits = Vec::new();
    for record in records {
        commits.extend(pipeline.record(record, no_layouts)?);
    }
    Ok(commits)
}

/// A commit of `xid` with one relcache invalidation, of table 77.
fn ddl(wal: &mut Wal, xid: u32) -> Record {
    let main = invalidating_commit(COMMIT_TIME, 5, 77);
    let info = XACT_COMMIT | XACT_HAS_INFO;
    wal.record(RESOURCE_MANAGER_TRANSACTION, info, xid, main, Vec::new())
}

/// Three inserts by `xid`, past the limit of 2, and its commit.
fn big_transaction(wal: &mut Wal, xid: u32) -> Vec<Record> {
    let mut records = Vec::new();
    for id in 1..=3 {
        records.push(wal.insert(xid, id));
    }
    records.push(wal.commit(xid));
    records
}

fn assert_too_big(result: Result<Vec<Commit>, Error>) {
    let error = result.unwrap_err().to_string();
    assert!(error.contains("changes more than 2 rows"), "{error}");
}

/// Commits wait for the load's snapshot, and only those it doesn't
/// see go on; later commits go at once unless the snapshot saw them.
#[test]
fn holds_commits_until_the_snapshot() {
    let mut pipeline = pipeline(1 << 20);
    let mut wal = Wal {
        lsn: TEST_LSN,
        block: 0,
    };
    assert_eq!(pipeline.consistency(), Consistency::Unknown);
    let records = [
        wal.running(100, &[]),
        wal.insert(100, 1),
        wal.insert(101, 2),
    ];
    for record in records {
        assert_eq!(read_one(&mut pipeline, &record), []);
    }
    assert_eq!(pipeline.consistency(), Consistency::Consistent);
    let before = pipeline.bytes();
    for record in [wal.commit(101), wal.commit(100)] {
        assert_eq!(read_one(&mut pipeline, &record), []);
    }
    // The changes moved from open transactions to held commits.
    assert_eq!(pipeline.bytes(), before);
    // The snapshot saw 101, not 100.
    let snapshot = TransactionSnapshot::parse("100:102:100").unwrap();
    let (sight, released) = pipeline.see(snapshot, Lsn(u64::MAX)).unwrap();
    assert_eq!(ids(&released), [1]);
    // The load has every commit before the first it didn't see.
    assert_eq!(sight, Sight::Through(Lsn(released[0].lsn.0 - 1)));
    assert!(pipeline.bytes() < before, "released commits don't count");
    // 101 seen; 102 and 103 not.
    let later = [
        wal.insert(102, 3),
        wal.commit(102),
        wal.insert(101, 9),
        wal.commit(101),
    ];
    let commits = read_all(&mut pipeline, &later).unwrap();
    assert_eq!(ids(&commits), [3]);
}

/// Rows in a transaction the allocation tests read.
const ROWS: usize = 1000;
/// What reading one row of a spec table allocates: the page's slot
/// list, the row's bytes and the decoded row. The lists the publisher,
/// heap and transactions keep from row to row allocate nothing once
/// grown (`physical_wal_bench`).
const ROW_ALLOCATIONS: usize = 3;
/// What a transaction allocates beyond its rows: its commit's lists,
/// and a share of the page map's growth.
const TRANSACTION_ALLOCATIONS: usize = 50;

/// `ROWS` inserts by `xid`, each on a page of its own, and its commit.
fn transaction(wal: &mut Wal, xid: u32) -> Vec<Record> {
    let mut records = Vec::new();
    for id in 0..ROWS {
        records.push(wal.insert(xid, i32::try_from(id).unwrap()));
    }
    records.push(wal.commit(xid));
    records
}

/// A pipeline of `layouts`, consistent and past its snapshot, that a
/// transaction of `ROWS` has run through, so its lists have grown.
fn warmed(wal: &mut Wal, checked: Vec<Checked>, layouts: Layouts) -> Pipeline {
    let mut pipeline = Pipeline::new(checked, layouts, 18, 2 * ROWS, 1 << 30, 1 << 30);
    read_one(&mut pipeline, &wal.running(100, &[]));
    see_all(&mut pipeline, "100:100:").unwrap();
    read_all(&mut pipeline, &transaction(wal, 100)).unwrap();
    pipeline
}

/// The allocations reading `records` makes, and the rows it commits.
fn counted(pipeline: &mut Pipeline, records: &[Record]) -> (usize, usize) {
    let before = allocations();
    let commits = read_all(pipeline, records).unwrap();
    let made = allocations() - before;
    (made, ids(&commits).len())
}

#[test]
fn reads_a_row_within_its_allocations() {
    let mut wal = Wal {
        lsn: TEST_LSN,
        block: 0,
    };
    let checked = Checked {
        oid: TABLE_OID,
        ..Checked::public("t", "t", &["id"], vec![ColumnType::Int])
    };
    let layouts = layouts();
    let mut pipeline = warmed(&mut wal, vec![checked], layouts);
    let records = transaction(&mut wal, 101);
    let (made, rows) = counted(&mut pipeline, &records);
    assert_eq!(rows, ROWS);
    let most = ROWS * ROW_ALLOCATIONS + TRANSACTION_ALLOCATIONS;
    assert!(
        made <= most,
        "{made} allocations for {ROWS} rows, at most {most}"
    );
}

#[test]
fn passes_other_tables_rows_without_allocating() {
    let mut wal = Wal {
        lsn: TEST_LSN,
        block: 0,
    };
    let mut pipeline = warmed(&mut wal, Vec::new(), Layouts::default());
    let records = transaction(&mut wal, 101);
    let (made, rows) = counted(&mut pipeline, &records);
    assert_eq!(rows, 0);
    assert_eq!(made, 0, "allocations for another table's rows");
}

/// A transaction past the limit of 2 rows, and rows past the bytes
/// allowed, stop the stream.
#[test]
fn stops_at_its_limits() {
    let mut wal = Wal {
        lsn: TEST_LSN,
        block: 0,
    };
    let mut big = pipeline(1 << 20);
    see_all(&mut big, "1:1:").unwrap();
    let records = [
        wal.running(100, &[]),
        wal.insert(100, 1),
        wal.insert(100, 2),
        wal.insert(100, 3),
    ];
    for record in records {
        big.record(&record, no_layouts).unwrap();
    }
    let commit = wal.commit(100);
    let error = big.record(&commit, no_layouts).unwrap_err().to_string();
    assert!(
        error.contains("changes more than 2 rows Straum reads"),
        "{error}"
    );

    let mut small = pipeline(100);
    let error = small.record(&wal.insert(200, 1), no_layouts).unwrap_err();
    assert!(matches!(error, Error::Memory { limit: 100, .. }), "{error}");

    // A row counts once however many facts read it; a transaction's
    // rows count together over savepoints, though no xid has 3.
    let mut twice = pipeline(1 << 20);
    twice.decoder = Decoder::new(vec![reading_t("t"), reading_t("u")], 2, 1 << 30);
    see_all(&mut twice, "1:1:").unwrap();
    let records = [
        wal.running(300, &[]),
        wal.insert(300, 1),
        wal.insert(301, 2),
        commit_after_savepoints(&mut wal, 300, &[301]),
    ];
    assert_eq!(read_all(&mut twice, &records).unwrap()[0].changes.len(), 4);
    let records = [
        wal.insert(302, 3),
        wal.insert(303, 4),
        wal.insert(304, 5),
        commit_after_savepoints(&mut wal, 302, &[303, 304]),
    ];
    assert_too_big(read_all(&mut twice, &records));
}

/// The commit of `xid` and its released savepoints' `subxacts`.
fn commit_after_savepoints(wal: &mut Wal, xid: u32, subxacts: &[u32]) -> Record {
    let main = commit_with_subxacts(COMMIT_TIME, subxacts);
    let info = XACT_COMMIT | XACT_HAS_INFO;
    wal.record(RESOURCE_MANAGER_TRANSACTION, info, xid, main, Vec::new())
}

/// A commit that invalidates a table reads its layout again, once.
#[test]
fn reads_layouts_again_after_ddl() {
    let mut pipeline = pipeline(1 << 20);
    let mut wal = Wal {
        lsn: TEST_LSN,
        block: 0,
    };
    see_all(&mut pipeline, "1:1:").unwrap();
    pipeline.record(&wal.running(100, &[]), no_layouts).unwrap();
    let ddl = ddl(&mut wal, 100);
    let asked = RefCell::new(Vec::new());
    // The table is gone now: its tables are unfit. The layouts must
    // show the DDL's own transaction.
    let commits = (pipeline.record(&ddl, |xid| Ok(no_tables_for(&asked, xid)))).unwrap();
    assert_eq!(*asked.borrow(), [100]);
    let unfit = Change::Unfit {
        table: Identifier::new("t").unwrap(),
        drift: Drift::Altered,
    };
    let mut changes = Vec::new();
    for commit in &commits {
        changes.extend(&commit.changes);
    }
    assert_eq!(changes, [&unfit]);
    let again = wal.record(
        RESOURCE_MANAGER_TRANSACTION,
        XACT_COMMIT | XACT_HAS_INFO,
        101,
        ddl.main_data().to_vec(),
        Vec::new(),
    );
    assert!(matches!(
        pipeline.record(&again, failing_layouts),
        Err(Error::Stopped)
    ));
}

/// An enum changed in the database of a table read of an enum, as by
/// a label added, reads the layouts again; one changed in another
/// database does not, nor another catalog changed in the same one.
#[test]
fn reads_layouts_again_only_after_an_enum_change_in_its_database() {
    let mut layout = table();
    layout.attributes[0].kind = Kind::Enum(0);
    let checked = Checked {
        oid: TABLE_OID,
        ..Checked::public("t", "t", &["id"], vec![ColumnType::Text])
    };
    let layouts = Layouts {
        tables: vec![layout],
        labels: HashMap::new(),
    };
    let mut pipeline = Pipeline::new(vec![checked], layouts, 18, 2, 1 << 20, 1 << 30);
    let mut wal = Wal {
        lsn: TEST_LSN,
        block: 0,
    };
    see_all(&mut pipeline, "1:1:").unwrap();
    pipeline.record(&wal.running(100, &[]), no_layouts).unwrap();
    let asked = RefCell::new(Vec::new());
    let commits = [
        (100, enum_commit(COMMIT_TIME, DATABASE + 1)),
        (101, statistic_commit(COMMIT_TIME, DATABASE)),
        (102, enum_commit(COMMIT_TIME, DATABASE)),
    ];
    for (xid, main) in commits {
        let info = XACT_COMMIT | XACT_HAS_INFO;
        let commit = wal.record(RESOURCE_MANAGER_TRANSACTION, info, xid, main, Vec::new());
        let read = pipeline.record(&commit, |xid| Ok(the_table_for(&asked, xid)));
        read.unwrap();
    }
    assert_eq!(*asked.borrow(), [102]);
}

/// DDL lays out the rows after it, whether or not its commit goes on:
/// one before the start and one the load saw read the layout again.
#[test]
fn ddl_not_published_still_lays_out_again() {
    let mut pipeline = pipeline(1 << 20);
    let mut wal = Wal {
        lsn: TEST_LSN,
        block: 0,
    };
    let asked = RefCell::new(Vec::new());
    // Before any RUNNING_XACTS: not whole, yet laid out again.
    let early = ddl(&mut wal, 50);
    let commits = pipeline.record(&early, |xid| Ok(the_table_for(&asked, xid)));
    assert_eq!(commits.unwrap(), []);
    assert_eq!(*asked.borrow(), [50]);
    pipeline.record(&wal.running(100, &[]), no_layouts).unwrap();
    see_all(&mut pipeline, "100:102:").unwrap();
    // Seen by the load: dropped, yet laid out again.
    let seen = ddl(&mut wal, 101);
    let commits = pipeline.record(&seen, |xid| Ok(the_table_for(&asked, xid)));
    assert_eq!(commits.unwrap(), []);
    assert_eq!(*asked.borrow(), [50, 101]);
}

/// The changes of a commit, with every `Unfit` among them.
fn unfit_tables(commits: &[Commit]) -> Vec<&Change> {
    let mut unfit = Vec::new();
    for commit in commits {
        for change in &commit.changes {
            if let Change::Unfit { .. } = change {
                unfit.push(change);
            }
        }
    }
    unfit
}

/// A table rewritten by DDL the load saw stops the stream with that
/// DDL's commit, or with the snapshot if the commit came before it:
/// changes to its new file pass by unread and bring no commit.
#[test]
fn a_rewrite_the_load_saw_is_unfit_at_once() {
    let unfit = Change::Unfit {
        table: Identifier::new("t").unwrap(),
        drift: Drift::Altered,
    };
    let asked = RefCell::new(Vec::new());
    let mut wal = Wal {
        lsn: TEST_LSN,
        block: 0,
    };
    let mut after = pipeline(1 << 20);
    after.record(&wal.running(100, &[]), no_layouts).unwrap();
    see_all(&mut after, "100:102:").unwrap();
    let seen = ddl(&mut wal, 101);
    let commits = after.record(&seen, |xid| Ok(no_tables_for(&asked, xid)));
    assert_eq!(unfit_tables(&commits.unwrap()), [&unfit]);

    let mut before = pipeline(1 << 20);
    before.record(&wal.running(200, &[]), no_layouts).unwrap();
    let held = ddl(&mut wal, 200);
    let commits = before.record(&held, |xid| Ok(no_tables_for(&asked, xid)));
    assert_eq!(commits.unwrap(), []);
    let released = see_all(&mut before, "201:201:").unwrap();
    assert_eq!(unfit_tables(&released), [&unfit]);
}

/// DDL that makes a table unfit before the first `RUNNING_XACTS` is
/// held by no commit, as its transaction may not be whole: the
/// snapshot still stops the stream, with no later commit.
#[test]
fn a_table_made_unfit_before_any_running_transactions_is_unfit_at_once() {
    let unfit = Change::Unfit {
        table: Identifier::new("t").unwrap(),
        drift: Drift::Altered,
    };
    let asked = RefCell::new(Vec::new());
    let mut wal = Wal {
        lsn: TEST_LSN,
        block: 0,
    };
    let mut pipeline = pipeline(1 << 20);
    let early = ddl(&mut wal, 50);
    let commits = pipeline.record(&early, |xid| Ok(no_tables_for(&asked, xid)));
    assert_eq!(commits.unwrap(), []);
    pipeline.record(&wal.running(100, &[]), no_layouts).unwrap();
    let released = see_all(&mut pipeline, "100:100:").unwrap();
    assert_eq!(unfit_tables(&released), [&unfit]);
}

/// A stream that stays up 2^31 xids past its start still publishes:
/// once every transaction running began after the start and after
/// the load's snapshot, neither is asked about an xid again.
#[test]
fn publishes_two_to_the_31_xids_on() {
    let mut pipeline = pipeline(1 << 20);
    let mut wal = Wal {
        lsn: TEST_LSN,
        block: 0,
    };
    pipeline.record(&wal.running(100, &[]), no_layouts).unwrap();
    see_all(&mut pipeline, "100:100:").unwrap();
    pipeline.record(&wal.running(101, &[]), no_layouts).unwrap();
    let far = 100 + (1 << 31);
    let mut commits = Vec::new();
    for (id, xid) in [(1, far - 1), (2, far), (3, far + 1)] {
        let records = [wal.insert(xid, id), wal.commit(xid)];
        commits.extend(read_all(&mut pipeline, &records).unwrap());
    }
    assert_eq!(ids(&commits), [1, 2, 3]);
}

/// A transaction past the limit stops the stream only if the load
/// didn't see it, whether it commits before the snapshot or after.
#[test]
fn a_transaction_past_the_limit_the_load_saw_goes_quietly() {
    let mut wal = Wal {
        lsn: TEST_LSN,
        block: 0,
    };
    let mut before = pipeline(1 << 20);
    let mut records = vec![wal.running(100, &[])];
    records.extend(big_transaction(&mut wal, 100));
    records.extend(big_transaction(&mut wal, 101));
    assert_eq!(read_all(&mut before, &records).unwrap(), []);
    // Seen, both go quietly; 101 unseen stops the stream.
    let mut seen = pipeline(1 << 20);
    read_all(&mut seen, &records).unwrap();
    let released = see_all(&mut seen, "102:102:");
    assert_eq!(released.unwrap(), []);
    assert_too_big(see_all(&mut before, "101:102:101"));
    // After the snapshot: 102 it saw, 103 it didn't.
    let mut after = pipeline(1 << 20);
    after.record(&wal.running(102, &[]), no_layouts).unwrap();
    see_all(&mut after, "103:103:").unwrap();
    let seen_by_load = big_transaction(&mut wal, 102);
    assert_eq!(read_all(&mut after, &seen_by_load).unwrap(), []);
    assert_too_big(read_all(&mut after, &big_transaction(&mut wal, 103)));
}

/// A commit read as before the start must be in the load: a snapshot
/// that doesn't see it is named, one that does is taken.
#[test]
fn a_snapshot_must_see_what_was_dropped() {
    let mut pipeline = pipeline(1 << 20);
    let mut wal = Wal {
        lsn: TEST_LSN,
        block: 0,
    };
    for record in [
        wal.insert(99, 1),
        wal.running(100, &[99]),
        wal.insert(100, 2),
        wal.commit(99),
        wal.commit(100),
    ] {
        assert_eq!(pipeline.record(&record, no_layouts).unwrap(), []);
    }
    assert_eq!(pipeline.consistency(), Consistency::Consistent);
    let blind = TransactionSnapshot::parse("99:101:99").unwrap();
    assert_eq!(
        pipeline.see(blind, Lsn(7)).unwrap(),
        (Sight::Blind(vec![99]), Vec::new())
    );
    let fine = TransactionSnapshot::parse("101:101:").unwrap();
    assert_eq!(
        pipeline.see(fine, Lsn(7)).unwrap(),
        (Sight::Through(Lsn(7)), Vec::new())
    );
}

/// The load's snapshot can be older than the last `RUNNING_XACTS`
/// read: one that says no xid before the dropped commit runs doesn't
/// make a snapshot taken while it still ran see it.
#[test]
fn a_snapshot_older_than_the_last_running_xacts_must_see_what_was_dropped() {
    let mut pipeline = pipeline(1 << 20);
    let mut wal = Wal {
        lsn: TEST_LSN,
        block: 0,
    };
    for record in [
        wal.insert(99, 1),
        wal.running(100, &[99]),
        wal.commit(99),
        wal.running(101, &[]),
    ] {
        assert_eq!(pipeline.record(&record, no_layouts).unwrap(), []);
    }
    let blind = TransactionSnapshot::parse("99:101:99").unwrap();
    assert_eq!(
        pipeline.see(blind, Lsn(7)).unwrap(),
        (Sight::Blind(vec![99]), Vec::new())
    );
    let fine = TransactionSnapshot::parse("101:101:").unwrap();
    assert_eq!(
        pipeline.see(fine, Lsn(7)).unwrap(),
        (Sight::Through(Lsn(7)), Vec::new())
    );
}

/// A snapshot taken with the server's WAL past what the stream read
/// is in the load only up to what was read: a commit after that the
/// snapshot doesn't see streams later, and the start is before it.
#[test]
fn the_load_is_known_only_through_what_was_read() {
    let mut pipeline = pipeline(1 << 20);
    let mut wal = Wal {
        lsn: TEST_LSN,
        block: 0,
    };
    for record in [wal.running(100, &[]), wal.insert(100, 1)] {
        assert_eq!(read_one(&mut pipeline, &record), []);
    }
    let read_to = wal.lsn;
    // The server had written 100's commit, which the snapshot doesn't
    // see yet and the stream hasn't read.
    let snapshot = TransactionSnapshot::parse("100:101:100").unwrap();
    let (sight, released) = pipeline.see(snapshot, Lsn(0x200_0000)).unwrap();
    assert_eq!(
        (sight, released),
        (Sight::Through(Lsn(read_to)), Vec::new())
    );
    let streamed = read_one(&mut pipeline, &wal.commit(100));
    assert_eq!(ids(&streamed), [1]);
    assert!(streamed[0].lsn > Lsn(read_to));
}
