//! The physical stream's path from WAL records to commits, run in order
//! for each record: `heap/` applies it to the tables' pages and gives row
//! changes, `transactions.rs` holds them by transaction, `xact.rs` says
//! when one ends, and `messages.rs` turns a commit into the messages the
//! decoder both streams share. Reads no socket and no catalog: a table's
//! layout, read again after DDL, comes from the caller. Commits wait until
//! the load's snapshot is known, and only those it doesn't see go on: the
//! load has the rest.

use std::mem;

use straum_app::{Commit, Lsn};

use super::heap::Heap;
use super::messages::{Layouts, Publisher};
use super::transactions::{Committed, Consistency, TooBig, TransactionSnapshot, Transactions};
use super::xact;
use super::{Record, TransactionId};
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
    snapshot: Option<TransactionSnapshot>,
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
            snapshot: None,
        }
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
    // connection, which this file must not know.
    pub(crate) fn record(
        &mut self,
        record: &Record,
        layouts: impl FnMut() -> Result<Layouts, Error>,
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
        mut layouts: impl FnMut() -> Result<Layouts, Error>,
    ) -> Result<Vec<Commit>, Error> {
        let changes = self.heap.apply(record)?;
        self.transactions.hold(record.xid, record.lsn, changes);
        let Some(event) = xact::event(record, self.major)? else {
            return Ok(Vec::new());
        };
        // DDL changes how the rows after it lie on the pages, whether or
        // not its commit goes on: one before the start, or that the load
        // saw, too.
        if let xact::Event::Commit(ending) = &event {
            let stale = self.publisher.stale(&ending.invalidated);
            if !stale.is_empty() {
                let fresh = layouts()?;
                self.publisher.relayout(&stale, fresh);
            }
        }
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
        // the load.
        let mut seen = at;
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
        self.snapshot = Some(snapshot);
        Ok((Sight::Through(seen), commits))
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::collections::HashMap;

    use straum_app::Change;
    use straum_domain::{ColumnType, Datum, Identifier};

    use super::*;
    use crate::pgoutput::{Relation, RelationColumn};
    use crate::value::INT4;
    use crate::wal::messages::Layout;
    use crate::wal::test_support::decode;
    use crate::wal::test_support::{BlockSpec, FILE, Spec, assemble};
    use crate::wal::tuple::{Attribute, Kind};
    use crate::wal::xact::tests::{invalidating_commit, running_main};
    use crate::wal::xact::{STANDBY_RUNNING_XACTS, XACT_COMMIT, XACT_HAS_INFO};
    use crate::wal::{
        RESOURCE_MANAGER_HEAP, RESOURCE_MANAGER_STANDBY, RESOURCE_MANAGER_TRANSACTION,
    };

    use crate::wal::heap::{HEAP_INSERT, INIT_PAGE};
    use crate::wal::record::RECORD_ALIGNMENT;
    use crate::wal::test_support::{ROW_HEADER, ROW_PADDING};
    /// The commit time of every test commit, in microseconds since 2000.
    const COMMIT_TIME: i64 = 7;

    /// `public.t (id int4)`, read by the spec as `t`.
    fn table() -> Layout {
        Layout {
            file: FILE,
            relation: Relation {
                id: 77,
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
        let checked = Checked::public("t", "t", &["id"], vec![ColumnType::Int]);
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

    fn no_layouts() -> Result<Layouts, Error> {
        unreachable!("no commit here invalidates a table")
    }

    fn failing_layouts() -> Result<Layouts, Error> {
        Err(Error::Stopped)
    }

    /// No tables any more, counting the call in `calls`.
    fn count_no_tables(calls: &Cell<u32>) -> Layouts {
        calls.set(calls.get() + 1);
        Layouts::default()
    }

    /// The table as it was, counting the call in `calls`.
    fn count_the_table(calls: &Cell<u32>) -> Layouts {
        calls.set(calls.get() + 1);
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
        let calls = Cell::new(0);
        // The table is gone now: its tables are unfit.
        let commits = (pipeline.record(&ddl, || Ok(count_no_tables(&calls)))).unwrap();
        assert_eq!(calls.get(), 1);
        let unfit = Change::Unfit {
            table: Identifier::new("t").unwrap(),
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

    /// DDL lays out the rows after it, whether or not its commit goes on:
    /// one before the start and one the load saw read the layout again.
    #[test]
    fn ddl_not_published_still_lays_out_again() {
        let mut pipeline = pipeline(1 << 20);
        let mut wal = Wal {
            lsn: 0x100_0028,
            block: 0,
        };
        let calls = Cell::new(0);
        // Before any RUNNING_XACTS: not whole, yet laid out again.
        let early = ddl(&mut wal, 50);
        let commits = pipeline.record(&early, || Ok(count_the_table(&calls)));
        assert_eq!(commits.unwrap(), []);
        assert_eq!(calls.get(), 1);
        pipeline.record(&wal.running(100, &[]), no_layouts).unwrap();
        see_all(&mut pipeline, "100:102:").unwrap();
        // Seen by the load: dropped, yet laid out again.
        let seen = ddl(&mut wal, 101);
        let commits = pipeline.record(&seen, || Ok(count_the_table(&calls)));
        assert_eq!(commits.unwrap(), []);
        assert_eq!(calls.get(), 2);
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
}
