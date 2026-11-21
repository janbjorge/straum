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
mod tests;
