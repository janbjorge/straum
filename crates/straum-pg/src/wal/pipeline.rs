//! The physical stream's path from WAL records to commits, run in order
//! for each record: `heap/` applies it to the tables' pages and gives row
//! changes, `transactions.rs` holds them by transaction, `xact.rs` says
//! when one ends, and `messages.rs` turns a commit into the messages the
//! decoder both streams share. Reads no socket and no catalog: a table's
//! layout, read again after DDL, comes from the caller, as of a snapshot
//! that sees the DDL's transaction. Commits wait until
//! the load's snapshot is known, and only those it doesn't see go on: the
//! load has the rest.

use std::mem;

use straum_app::{Commit, Lsn};
use straum_domain::FastSet;

use super::heap::Heap;
use super::messages::{Layouts, Publisher};
use super::transactions::{Committed, Consistency, TooBig, TransactionSnapshot, Transactions};
use super::xact;
use super::{Record, RelationFile, TransactionId};
use crate::catalog::Checked;
use crate::decode::Decoder;
use crate::error::Error;

/// What the pipeline made of the load's snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sight {
    /// Every commit up to this LSN is in the load; later ones stream.
    Through(Lsn),
    /// The snapshot doesn't see these transactions, whose commits the
    /// stream read as before its start: take another.
    Blind(Vec<TransactionId>),
}

/// The pages, held transactions, publisher and decoder of one stream.
pub(crate) struct Pipeline {
    major: u32,
    limit: usize,
    /// The most bytes of rows held: pages, open transactions' changes and
    /// commits waiting for the snapshot.
    max_bytes: usize,
    /// The held commits' changes, summed.
    held_bytes: usize,
    heap: Heap,
    transactions: Transactions,
    publisher: Publisher,
    decoder: Decoder,
    /// Commits read before the load's snapshot is known.
    held: Vec<Committed>,
    /// Commits past the limit read before it: only those the load
    /// doesn't see stop the stream.
    too_big: Vec<TooBig>,
    /// The last commit read before the load's snapshot is known, held or
    /// not, without its changes: it carries a table made unfit by then.
    last_before_snapshot: Option<Committed>,
    snapshot: Option<TransactionSnapshot>,
    /// Where the next record starts: every commit before it is read.
    read_to: u64,
}

impl Pipeline {
    /// `layouts` are those of `checked`, the spec's tables.
    pub(crate) fn new(
        checked: Vec<Checked>,
        layouts: Layouts,
        major: u32,
        limit: usize,
        max_bytes: usize,
    ) -> Pipeline {
        let publisher = Publisher::new(layouts);
        Pipeline {
            major,
            limit,
            max_bytes,
            held_bytes: 0,
            heap: Heap::new(publisher.files(), major),
            transactions: Transactions::new(limit),
            publisher,
            decoder: Decoder::new(checked, limit),
            held: Vec::new(),
            too_big: Vec::new(),
            last_before_snapshot: None,
            snapshot: None,
            read_to: 0,
        }
    }

    /// The relation files of the tables it reads.
    pub(crate) fn files(&self) -> FastSet<RelationFile> {
        self.publisher.files().into_iter().collect()
    }

    /// Every transaction that commits from here on is whole in the WAL
    /// read so far: the load's snapshot may be taken.
    pub(crate) fn consistency(&self) -> Consistency {
        self.transactions.consistency()
    }

    /// The bytes of rows held.
    // Leaves out the reader's buffer, `Vec` slack and the maps in `Heap`
    // and `Transactions`, which never shrink; `--max-page-memory`'s docs
    // say so.
    pub(crate) fn bytes(&self) -> usize {
        self.heap.bytes() + self.transactions.bytes() + self.held_bytes
    }

    /// Reads one record. `layouts` reads the tables' layouts again, after
    /// a commit that may have changed them.
    // `impl FnMut`: the caller passes a closure over its own catalog
    // connection, which this file must not know. It gets the xid of the
    // DDL whose commit the layouts must show.
    pub(crate) fn record(
        &mut self,
        record: &Record,
        layouts: impl FnMut(TransactionId) -> Result<Layouts, Error>,
    ) -> Result<Vec<Commit>, Error> {
        let commits = self.read(record, layouts)?;
        let held = self.bytes();
        if held > self.max_bytes {
            return Err(Error::Memory {
                held,
                limit: self.max_bytes,
            });
        }
        Ok(commits)
    }

    // `impl FnMut`, as for `record`: the caller's closure reads the catalog.
    fn read(
        &mut self,
        record: &Record,
        mut layouts: impl FnMut(TransactionId) -> Result<Layouts, Error>,
    ) -> Result<Vec<Commit>, Error> {
        self.read_to = record.next();
        let mut changes = self.heap.apply(record)?;
        self.transactions.hold(record.xid, record.lsn, &mut changes);
        self.heap.reuse(changes);
        let Some(event) = xact::event(record, self.major)? else {
            return Ok(Vec::new());
        };
        // DDL changes how the rows after it lie on the pages, whether or
        // not its commit goes on: one before the start, or that the load
        // saw, too.
        let mut carrier = None;
        if let xact::Event::Commit(ending) = &event {
            let stale = self.publisher.stale(&ending.invalidated);
            // An enum's new label invalidates no table; rows to come may
            // hold it. A label renamed makes the tables that read its
            // enum unfit at this commit, which stops Straum.
            let relabel = self.publisher.reads_enums_in(&ending.enums_changed);
            if !stale.is_empty() || relabel {
                let fresh = layouts(ending.xid)?;
                self.publisher.relayout(&stale, fresh);
            }
            carrier = Some(Committed {
                xid: ending.xid,
                lsn: record.lsn,
                end: record.next(),
                time: ending.time,
                changes: Vec::new(),
                invalidated: Vec::new(),
            });
        }
        // Once every transaction running began after the load's snapshot,
        // it sees none to come; asking it 2^31 xids on would wrap.
        if let xact::Event::Running(running) = &event
            && let Some(snapshot) = &self.snapshot
            && snapshot.precedes(running.oldest_xid)
        {
            self.snapshot = Some(TransactionSnapshot::seeing_nothing());
        }
        let mut commits = self.commits(event, record)?;
        let Some(carrier) = carrier else {
            return Ok(commits);
        };
        if self.snapshot.is_none() {
            self.last_before_snapshot = Some(carrier);
            return Ok(commits);
        }
        // A table made unfit by a commit that doesn't go on, as one the
        // load saw, must still stop the stream now: changes to its new
        // file pass by unread, and may never bring a commit of their own.
        if commits.is_empty() && self.publisher.unfit_pending() {
            commits.extend(self.publisher.publish(&carrier, &mut self.decoder)?);
        }
        Ok(commits)
    }

    /// The commits `event` makes: none until the load's snapshot is
    /// known, then those it doesn't see.
    fn commits(&mut self, event: xact::Event, record: &Record) -> Result<Vec<Commit>, Error> {
        let committed = match self.transactions.event(event, record.lsn, record.next()) {
            Ok(Some(committed)) => committed,
            Ok(None) => return Ok(Vec::new()),
            Err(too_big) => {
                return match &self.snapshot {
                    None => {
                        self.too_big.push(too_big);
                        Ok(Vec::new())
                    }
                    Some(snapshot) if snapshot.sees(too_big.xid) => Ok(Vec::new()),
                    Some(_) => Err(self.too_big(too_big)),
                };
            }
        };
        match &self.snapshot {
            None => {
                for change in &committed.changes {
                    self.held_bytes += change.footprint();
                }
                self.held.push(committed);
                Ok(Vec::new())
            }
            Some(snapshot) if snapshot.sees(committed.xid) => Ok(Vec::new()),
            Some(_) => {
                let mut commits = Vec::new();
                if let Some(commit) = self.publisher.publish(&committed, &mut self.decoder)? {
                    commits.push(commit);
                }
                Ok(commits)
            }
        }
    }

    /// The stream's error for a transaction past the limit.
    fn too_big(&self, too_big: TooBig) -> Error {
        Error::Transaction {
            lsn: Lsn(too_big.lsn),
            limit: self.limit,
        }
    }

    /// Takes the load's snapshot, taken with the server's WAL at `at`:
    /// the LSN every commit up to which is in the load, and the commits
    /// held that it didn't see, which go on, as every later one does. A
    /// snapshot blind to a commit read as before the start isn't taken:
    /// PostgreSQL logged it and hadn't ended it yet, as while it waits
    /// for a synchronous standby. A later snapshot sees it.
    pub(crate) fn see(
        &mut self,
        snapshot: TransactionSnapshot,
        at: Lsn,
    ) -> Result<(Sight, Vec<Commit>), Error> {
        let blind = self.transactions.unseen_dropped(&snapshot);
        if !blind.is_empty() {
            return Ok((Sight::Blind(blind), Vec::new()));
        }
        for too_big in &self.too_big {
            if !snapshot.sees(too_big.xid) {
                return Err(self.too_big(*too_big));
            }
        }
        // Every commit before the first held one it doesn't see is in
        // the load. A commit not read yet may be before `at` and unseen
        // too, and it streams: the load is only known through what was
        // read.
        let mut seen = at.min(Lsn(self.read_to));
        for committed in &self.held {
            if !snapshot.sees(committed.xid) {
                seen = seen.min(Lsn(committed.end.saturating_sub(1)));
            }
        }
        self.too_big = Vec::new();
        self.transactions.loaded();
        self.held_bytes = 0;
        let mut commits = Vec::new();
        for committed in mem::take(&mut self.held) {
            if !snapshot.sees(committed.xid) {
                commits.extend(self.publisher.publish(&committed, &mut self.decoder)?);
            }
        }
        // A table made unfit before the start, by DDL the load saw, stops
        // the stream at once, as after it, though no commit was held, as
        // before the first `RUNNING_XACTS`.
        let last = self.last_before_snapshot.take();
        if commits.is_empty()
            && self.publisher.unfit_pending()
            && let Some(last) = last
        {
            commits.extend(self.publisher.publish(&last, &mut self.decoder)?);
        }
        self.snapshot = Some(snapshot);
        Ok((Sight::Through(seen), commits))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;

    use straum_app::{Change, Drift};
    use straum_domain::{ColumnType, Datum, Identifier};

    use super::*;
    use crate::pgoutput::{Relation, RelationColumn};
    use crate::value::INT4;
    use crate::wal::messages::Layout;
    use crate::wal::test_support::{BlockSpec, DATABASE, FILE, Spec, assemble};
    use crate::wal::test_support::{allocations, decode};
    use crate::wal::tuple::{Attribute, Kind};
    use crate::wal::xact::tests::{
        enum_commit, invalidating_commit, running_main, statistic_commit,
    };
    use crate::wal::xact::{STANDBY_RUNNING_XACTS, XACT_COMMIT, XACT_HAS_INFO};
    use crate::wal::{
        RESOURCE_MANAGER_HEAP, RESOURCE_MANAGER_STANDBY, RESOURCE_MANAGER_TRANSACTION,
    };

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

    fn pipeline(max_bytes: usize) -> Pipeline {
        let checked = Checked {
            oid: TABLE_OID,
            ..Checked::public("t", "t", &["id"], vec![ColumnType::Int])
        };
        Pipeline::new(
            vec![checked],
            Layouts {
                tables: vec![table()],
                labels: HashMap::new(),
            },
            18,
            2,
            max_bytes,
        )
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
        Layouts {
            tables: vec![table()],
            labels: HashMap::new(),
        }
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
            lsn: 0x100_0028,
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
        let mut pipeline = Pipeline::new(checked, layouts, 18, 2 * ROWS, 1 << 30);
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
            lsn: 0x100_0028,
            block: 0,
        };
        let checked = Checked {
            oid: TABLE_OID,
            ..Checked::public("t", "t", &["id"], vec![ColumnType::Int])
        };
        let layouts = Layouts {
            tables: vec![table()],
            labels: HashMap::new(),
        };
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
            lsn: 0x100_0028,
            block: 0,
        };
        let mut pipeline = warmed(&mut wal, Vec::new(), Layouts::default());
        let records = transaction(&mut wal, 101);
        let (made, rows) = counted(&mut pipeline, &records);
        assert_eq!(rows, 0);
        assert_eq!(made, 0, "allocations for another table's rows");
    }

    /// A transaction past the limit, and rows past the bytes allowed, stop
    /// the stream.
    #[test]
    fn stops_at_its_limits() {
        let mut wal = Wal {
            lsn: 0x100_0028,
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
    }

    /// A commit that invalidates a table reads its layout again, once.
    #[test]
    fn reads_layouts_again_after_ddl() {
        let mut pipeline = pipeline(1 << 20);
        let mut wal = Wal {
            lsn: 0x100_0028,
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
        let mut pipeline = Pipeline::new(vec![checked], layouts, 18, 2, 1 << 20);
        let mut wal = Wal {
            lsn: 0x100_0028,
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
            lsn: 0x100_0028,
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
            lsn: 0x100_0028,
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
            lsn: 0x100_0028,
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
            lsn: 0x100_0028,
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
            lsn: 0x100_0028,
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
            lsn: 0x100_0028,
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
            lsn: 0x100_0028,
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
            lsn: 0x100_0028,
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
}
