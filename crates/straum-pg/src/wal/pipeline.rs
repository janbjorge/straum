//! The physical stream's path from WAL records to commits: the rows of
//! the tables' pages, the changes each transaction holds, then the
//! messages the decoder both streams share. Reads no socket and no
//! catalog: a table's layout, read again after DDL, comes from the caller.
//!
//! Commits wait until the load's snapshot is known, and only those it
//! doesn't see go on: the load has the rest.

use std::collections::HashMap;

use straum_app::{Commit, Lsn};

use super::Record;
use super::heap::{Change, Heap};
use super::messages::{Publisher, Table};
use super::transactions::{Committed, Consistency, Snapshot, TooBig, Transactions, unseen};
use super::xact;
use crate::catalog::Checked;
use crate::decode::Decoder;
use crate::error::Error;

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
    snapshot: Option<Snapshot>,
}

impl Pipeline {
    /// `tables` are the layouts of `checked`, the spec's tables; `labels`
    /// their enums' labels by OID.
    pub(crate) fn new(
        checked: Vec<Checked>,
        tables: Vec<Table>,
        labels: HashMap<u32, String>,
        major: u32,
        limit: usize,
        max_bytes: usize,
    ) -> Pipeline {
        let publisher = Publisher::new(tables, labels);
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
    pub(crate) fn bytes(&self) -> usize {
        self.heap.bytes() + self.transactions.bytes() + self.held_bytes
    }

    /// Reads one record. `layouts` reads the tables' layouts again, after
    /// a commit that may have changed them.
    pub(crate) fn record(
        &mut self,
        record: &Record,
        layouts: impl FnMut() -> Result<Vec<Table>, Error>,
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

    fn read(
        &mut self,
        record: &Record,
        mut layouts: impl FnMut() -> Result<Vec<Table>, Error>,
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
                self.held_bytes += committed
                    .changes
                    .iter()
                    .map(Change::footprint)
                    .sum::<usize>();
                self.held.push(committed);
                Ok(Vec::new())
            }
            Some(snapshot) if snapshot.sees(committed.xid) => Ok(Vec::new()),
            Some(_) => Ok(self
                .publisher
                .publish(&committed, &mut self.decoder)?
                .into_iter()
                .collect()),
        }
    }

    fn too_big(&self, too_big: TooBig) -> Error {
        Error::Transaction {
            lsn: Lsn(too_big.lsn),
            limit: self.limit,
        }
    }

    /// The xids of commits read as before the start that `snapshot`
    /// doesn't see: PostgreSQL logged them and hadn't ended them yet, as
    /// while it waits for a synchronous standby. Such a snapshot can't be
    /// the load's; a later one can.
    pub(crate) fn unseen_dropped(&self, snapshot: &Snapshot) -> Vec<u32> {
        self.transactions.unseen_dropped(snapshot)
    }

    /// The load's snapshot, which sees every commit read as before the
    /// start: the commits held that it doesn't see, and every later one,
    /// go on; the others are in the load.
    pub(crate) fn snapshot(&mut self, snapshot: Snapshot) -> Result<Vec<Commit>, Error> {
        if let Some(too_big) = self.too_big.iter().find(|t| !snapshot.sees(t.xid)) {
            return Err(self.too_big(*too_big));
        }
        self.too_big = Vec::new();
        self.transactions.loaded();
        let mut commits = Vec::new();
        self.held_bytes = 0;
        for committed in unseen(&mut self.held, &snapshot) {
            commits.extend(self.publisher.publish(&committed, &mut self.decoder)?);
        }
        self.snapshot = Some(snapshot);
        Ok(commits)
    }

    /// The first commit the snapshot doesn't see, of those held: every
    /// commit before it is in the load.
    pub(crate) fn first_unseen(&self, snapshot: &Snapshot) -> Option<Lsn> {
        (self.held.iter())
            .filter(|c| !snapshot.sees(c.xid))
            .map(|c| Lsn(c.end))
            .min()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use straum_app::Change as Applied;
    use straum_domain::{ColumnType, Datum, Identifier};

    use super::*;
    use crate::pgoutput::{Column, Relation};
    use crate::wal::tests::{BlockSpec, Spec, assemble};
    use crate::wal::tuple::{Attribute, Kind};
    use crate::wal::{RelationFile, decode};

    const FILE: RelationFile = RelationFile {
        tablespace: 1663,
        database: 5,
        relation: 16_384,
    };

    /// `public.t (id int4)`, read by the spec as `t`.
    fn table() -> Table {
        Table {
            file: FILE,
            relation: Relation {
                id: 77,
                schema: "public".to_owned(),
                name: "t".to_owned(),
                columns: vec![Column {
                    name: "id".to_owned(),
                    type_oid: 23,
                    typmod: -1,
                }],
            },
            attributes: vec![Attribute {
                length: 4,
                align: 4,
                kind: Kind::Int,
            }],
            columns: vec![0],
            missing: vec![None],
        }
    }

    fn pipeline(max_bytes: usize) -> Pipeline {
        let checked = Checked::public("t", "t", &["id"], vec![ColumnType::Int]);
        Pipeline::new(
            vec![checked],
            vec![table()],
            HashMap::new(),
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
            self.lsn = end.next_multiple_of(8);
            decode(lsn, end, bytes).unwrap()
        }

        /// A row `id` inserted on a page of its own.
        fn insert(&mut self, xid: u32, id: i32) -> Record {
            let mut data = vec![1, 0, 0, 0, 24];
            data.push(0);
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
            self.record(10, 0x80, xid, vec![1, 0, 0], vec![block])
        }

        fn commit(&mut self, xid: u32) -> Record {
            self.record(1, 0x00, xid, 7i64.to_le_bytes().to_vec(), Vec::new())
        }

        fn running(&mut self, next: u32, xids: &[u32]) -> Record {
            let mut main = Vec::new();
            let oldest = xids.iter().copied().min().unwrap_or(next);
            for word in [
                u32::try_from(xids.len()).unwrap(),
                0,
                0,
                next,
                oldest,
                next - 1,
            ] {
                main.extend(word.to_le_bytes());
            }
            for xid in xids {
                main.extend(xid.to_le_bytes());
            }
            self.record(8, 0x10, 0, main, Vec::new())
        }
    }

    fn no_layouts() -> Result<Vec<Table>, Error> {
        unreachable!("no commit here invalidates a table")
    }

    fn ids(commits: &[Commit]) -> Vec<i64> {
        commits
            .iter()
            .flat_map(|c| &c.changes)
            .map(|c| match c {
                Applied::Insert { row, .. } => match row[..] {
                    [Datum::Int(id)] => id,
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            })
            .collect()
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
        let read =
            |pipeline: &mut Pipeline, record: Record| pipeline.record(&record, no_layouts).unwrap();
        assert_eq!(pipeline.consistency(), Consistency::Unknown);
        let records = [
            wal.running(100, &[]),
            wal.insert(100, 1),
            wal.insert(101, 2),
        ];
        for record in records {
            assert_eq!(read(&mut pipeline, record), []);
        }
        assert_eq!(pipeline.consistency(), Consistency::Consistent);
        let before = pipeline.bytes();
        for record in [wal.commit(101), wal.commit(100)] {
            assert_eq!(read(&mut pipeline, record), []);
        }
        // The changes moved from open transactions to held commits.
        assert_eq!(pipeline.bytes(), before);
        // The snapshot saw 101, not 100.
        let snapshot = Snapshot::parse("100:102:100").unwrap();
        let first = pipeline.first_unseen(&snapshot).unwrap();
        let released = pipeline.snapshot(snapshot).unwrap();
        assert_eq!(ids(&released), [1]);
        assert_eq!(first, released[0].lsn);
        assert!(pipeline.bytes() < before, "released commits don't count");
        // 101 seen; 102 and 103 not.
        let later = [
            wal.insert(102, 3),
            wal.commit(102),
            wal.insert(101, 9),
            wal.commit(101),
        ];
        let commits: Vec<Commit> = later
            .into_iter()
            .flat_map(|r| read(&mut pipeline, r))
            .collect();
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
        big.snapshot(Snapshot::parse("1:1:").unwrap()).unwrap();
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
        let err = big.record(&commit, no_layouts).unwrap_err().to_string();
        assert!(
            err.contains("changes more than 2 rows Straum reads"),
            "{err}"
        );

        let mut small = pipeline(100);
        let err = small.record(&wal.insert(200, 1), no_layouts).unwrap_err();
        assert!(matches!(err, Error::Memory { limit: 100, .. }), "{err}");
    }

    /// A commit that invalidates a table reads its layout again, once.
    #[test]
    fn reads_layouts_again_after_ddl() {
        let mut pipeline = pipeline(1 << 20);
        let mut wal = Wal {
            lsn: 0x100_0028,
            block: 0,
        };
        pipeline.snapshot(Snapshot::parse("1:1:").unwrap()).unwrap();
        pipeline.record(&wal.running(100, &[]), no_layouts).unwrap();
        // A commit with one relcache invalidation of table 77.
        let mut main = 7i64.to_le_bytes().to_vec();
        main.extend((1u32 << 3).to_le_bytes());
        main.extend(1u32.to_le_bytes());
        main.extend([0xFE, 0, 0, 0]);
        for word in [5u32, 77, 0] {
            main.extend(word.to_le_bytes());
        }
        let ddl = wal.record(1, 0x80, 100, main, Vec::new());
        let calls = Cell::new(0);
        let layouts = || {
            calls.set(calls.get() + 1);
            Ok(Vec::new())
        };
        // The table is gone now: its tables are unfit.
        let commits = pipeline.record(&ddl, layouts).unwrap();
        assert_eq!(calls.get(), 1);
        let unfit = Applied::Unfit {
            table: Identifier::new("t").unwrap(),
        };
        assert_eq!(
            commits.iter().flat_map(|c| &c.changes).collect::<Vec<_>>(),
            [&unfit]
        );
        let failing = || Err(Error::Stopped);
        let again = wal.record(1, 0x80, 101, ddl.main_data().to_vec(), Vec::new());
        assert!(matches!(
            pipeline.record(&again, failing),
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
        let layouts = || {
            calls.set(calls.get() + 1);
            Ok(vec![table()])
        };
        let ddl = |wal: &mut Wal, xid: u32| {
            let mut main = 7i64.to_le_bytes().to_vec();
            main.extend((1u32 << 3).to_le_bytes());
            main.extend(1u32.to_le_bytes());
            main.extend([0xFE, 0, 0, 0]);
            for word in [5u32, 77, 0] {
                main.extend(word.to_le_bytes());
            }
            wal.record(1, 0x80, xid, main, Vec::new())
        };
        // Before any RUNNING_XACTS: not whole, yet laid out again.
        let early = ddl(&mut wal, 50);
        assert_eq!(pipeline.record(&early, layouts).unwrap(), []);
        assert_eq!(calls.get(), 1);
        pipeline.record(&wal.running(100, &[]), no_layouts).unwrap();
        pipeline
            .snapshot(Snapshot::parse("100:102:").unwrap())
            .unwrap();
        // Seen by the load: dropped, yet laid out again.
        let seen = ddl(&mut wal, 101);
        assert_eq!(pipeline.record(&seen, layouts).unwrap(), []);
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
        let big = |wal: &mut Wal, xid: u32| {
            let mut records: Vec<Record> = (1..=3).map(|id| wal.insert(xid, id)).collect();
            records.push(wal.commit(xid));
            records
        };
        let read = |pipeline: &mut Pipeline, records: Vec<Record>| {
            (records.iter())
                .map(|r| pipeline.record(r, no_layouts))
                .collect::<Result<Vec<_>, Error>>()
                .map(|commits| commits.concat())
        };
        let too_big = |result: Result<Vec<Commit>, Error>| {
            let err = result.unwrap_err().to_string();
            assert!(err.contains("changes more than 2 rows"), "{err}");
        };
        let mut before = pipeline(1 << 20);
        let mut records = vec![wal.running(100, &[])];
        records.extend(big(&mut wal, 100));
        records.extend(big(&mut wal, 101));
        assert_eq!(read(&mut before, records.clone()).unwrap(), []);
        // Seen, both go quietly; 101 unseen stops the stream.
        let mut seen = pipeline(1 << 20);
        read(&mut seen, records).unwrap();
        let released = seen.snapshot(Snapshot::parse("102:102:").unwrap());
        assert_eq!(released.unwrap(), []);
        too_big(before.snapshot(Snapshot::parse("101:102:101").unwrap()));
        // After the snapshot: 102 it saw, 103 it didn't.
        let mut after = pipeline(1 << 20);
        after.record(&wal.running(102, &[]), no_layouts).unwrap();
        after
            .snapshot(Snapshot::parse("103:103:").unwrap())
            .unwrap();
        assert_eq!(read(&mut after, big(&mut wal, 102)).unwrap(), []);
        too_big(read(&mut after, big(&mut wal, 103)));
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
        let blind = Snapshot::parse("99:101:99").unwrap();
        assert_eq!(pipeline.unseen_dropped(&blind), [99]);
        let fine = Snapshot::parse("101:101:").unwrap();
        assert_eq!(pipeline.unseen_dropped(&fine), Vec::<u32>::new());
        assert_eq!(ids(&pipeline.snapshot(fine).unwrap()), Vec::<i64>::new());
        assert_eq!(pipeline.unseen_dropped(&blind), Vec::<u32>::new());
    }
}
