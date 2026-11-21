//! Row changes held by transaction until it ends, the step between heap
//! changes and commits: record, heap change, transaction, commit. A
//! transaction is named by its xid, its transaction id; a savepoint gets a
//! subtransaction with an xid of its own, which commits with its top one.
//! Changes wait here as logical decoding's reorder buffer holds them
//! (`reorderbuffer.c`), and come out merged by LSN when the commit record
//! comes. This file also finds the point from which every commit is whole
//! in the WAL Straum reads, as `snapbuild.c` finds it.

use std::collections::HashSet;

use straum_domain::FastMap;

use super::TransactionId;
use super::heap::HeapChange;
use super::xact::{Ending, Event, Invalidated, Running};

/// The bits of a 64-bit xid that a 32-bit one keeps; the bits above count
/// how often the 32-bit xids wrapped, the epoch.
const LOW_32_BITS: u64 = 0xFFFF_FFFF;

/// A transaction that committed, with every change it made to the
/// tracked tables, in WAL order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Committed {
    pub(crate) xid: TransactionId,
    /// The commit record's start, which names it, and its end.
    pub(crate) lsn: u64,
    pub(crate) end: u64,
    /// Microseconds since 2000-01-01 UTC.
    pub(crate) time: i64,
    pub(crate) changes: Vec<HeapChange>,
    /// Tables whose layout may have changed with the commit.
    pub(crate) invalidated: Vec<Invalidated>,
}

/// How far the stream is from where every later commit is whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Consistency {
    /// No `RUNNING_XACTS` read yet.
    Unknown,
    /// These transactions ran at the first one and haven't ended.
    Waiting(Vec<TransactionId>),
    Consistent,
}

/// A transaction past the limit commits: its changes weren't all kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TooBig {
    pub(crate) xid: TransactionId,
    pub(crate) lsn: u64,
}

/// The changes of every open transaction, and how far the stream is from
/// where every commit is whole.
pub(crate) struct Transactions {
    /// By xid, top-level or sub, with the LSN of each change's record.
    held: FastMap<TransactionId, Held>,
    /// The held changes' footprints, summed.
    bytes: usize,
    /// The most changes, rows, one xid keeps; past it, only that it had
    /// more. A savepoint's xid is known as the top one's only at the
    /// commit, so the decoder counts the whole transaction's rows then.
    limit: usize,
    start: Start,
    /// The xids of commits dropped as not whole, until the load's
    /// snapshot: it must see each. `None` once it did.
    dropped: Option<HashSet<TransactionId>>,
}

/// What one xid holds: its changes with the LSN of each one's record.
#[derive(Default)]
struct Held {
    changes: Vec<HeldChange>,
    /// The changes' footprints, summed.
    bytes: usize,
    /// It made more changes than the limit; none of them is kept.
    over: bool,
}

/// A held change and the LSN of the record that made it.
struct HeldChange {
    lsn: u64,
    change: HeapChange,
}

// Straum starts at a redo point, so a transaction running there may have
// changed rows before it. The first `RUNNING_XACTS` names what runs; once
// all of those end, every later transaction began after it, whole.
// Commits of transactions before that point are dropped: the load, taken
// after it, must see them. PostgreSQL logs a commit before it ends the
// transaction for snapshots, and waits for synchronous standbys in
// between, so the dropped xids are kept for the load to check.

/// What the stream knows of where every commit is whole.
enum Start {
    /// No `RUNNING_XACTS` yet; the xids that ended so far.
    Unknown { ended: HashSet<TransactionId> },
    /// The first one: transactions from `next_xid` on are whole, since
    /// PostgreSQL holds `XidGenLock` until the record is in the WAL
    /// (`LogStandbySnapshot`), so no later xid wrote before it. Those it
    /// names that haven't ended yet keep Straum from being consistent.
    From {
        next_xid: TransactionId,
        running: HashSet<TransactionId>,
    },
    /// A later one's oldest running xid was `next_xid` or past it: every
    /// transaction since began after the start, so every commit is whole.
    /// Comparing with `next_xid` would wrap 2^31 xids on.
    Whole,
}

impl Transactions {
    /// Nothing held, the start not yet known; `limit` is the most changes
    /// one xid keeps.
    pub(crate) fn new(limit: usize) -> Transactions {
        Transactions {
            held: FastMap::default(),
            bytes: 0,
            limit,
            start: Start::Unknown {
                ended: HashSet::new(),
            },
            dropped: Some(HashSet::new()),
        }
    }

    /// Holds `changes`, made by `xid` in the record at `lsn`, until it
    /// ends, leaving the list empty for the next record.
    pub(crate) fn hold(&mut self, xid: TransactionId, lsn: u64, changes: &mut Vec<HeapChange>) {
        if changes.is_empty() {
            return;
        }
        let held = self.held.entry(xid).or_default();
        for change in changes.drain(..) {
            if held.changes.len() >= self.limit {
                held.over = true;
                held.changes = Vec::new();
                self.bytes -= held.bytes;
                held.bytes = 0;
            }
            if !held.over {
                let bytes = change.footprint();
                held.bytes += bytes;
                self.bytes += bytes;
                held.changes.push(HeldChange { lsn, change });
            }
        }
    }

    /// The bytes the held changes take.
    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    /// How far the stream is from where every transaction that commits
    /// is whole.
    pub(crate) fn consistency(&self) -> Consistency {
        match &self.start {
            Start::Unknown { .. } => Consistency::Unknown,
            Start::Whole => Consistency::Consistent,
            Start::From { running, .. } if running.is_empty() => Consistency::Consistent,
            Start::From { running, .. } => {
                let mut xids: Vec<TransactionId> = running.iter().copied().collect();
                xids.sort_unstable();
                Consistency::Waiting(xids)
            }
        }
    }

    /// The commit `event` is, if it is one Straum reads whole.
    pub(crate) fn event(
        &mut self,
        event: Event,
        lsn: u64,
        end: u64,
    ) -> Result<Option<Committed>, TooBig> {
        match event {
            Event::Running(running) => {
                self.running(&running);
                Ok(None)
            }
            Event::Abort(ending) => {
                self.ended(&ending);
                Ok(None)
            }
            Event::Commit(ending) => self.commit(ending, lsn, end),
        }
    }

    /// Ends `ending`'s transaction as committed: what it changed, if the
    /// commit is whole and changed or invalidated a table.
    fn commit(&mut self, ending: Ending, lsn: u64, end: u64) -> Result<Option<Committed>, TooBig> {
        let whole = match &self.start {
            Start::From { next_xid, .. } => follows(ending.xid, *next_xid),
            Start::Whole => true,
            Start::Unknown { .. } => false,
        };
        let held = self.ended(&ending);
        if !whole {
            if let Some(dropped) = &mut self.dropped {
                dropped.insert(ending.xid);
            }
            return Ok(None);
        }
        let mut over = false;
        let mut no_changes = true;
        for part in &held {
            over |= part.over;
            no_changes &= part.changes.is_empty();
        }
        if over {
            return Err(TooBig {
                xid: ending.xid,
                lsn,
            });
        }
        // pgoutput sends nothing for a transaction that changed no
        // published table. One that invalidated a table may have changed
        // how its rows read, so it still comes.
        if no_changes && ending.invalidated.is_empty() {
            return Ok(None);
        }
        Ok(Some(Committed {
            xid: ending.xid,
            lsn,
            end,
            time: ending.time,
            changes: merge_by_lsn(held),
            invalidated: ending.invalidated,
        }))
    }

    /// Reads a `RUNNING_XACTS`: the first sets the start, every one lets
    /// go of what transactions a crash ended.
    fn running(&mut self, running: &Running) {
        let first = matches!(self.start, Start::Unknown { .. });
        if let Start::Unknown { ended } = &self.start {
            // The record lists what ran when PostgreSQL looked, and goes
            // into the WAL after: a transaction may end in between, its
            // ending before the record that names it (`snapbuild.c`).
            let mut waiting = HashSet::new();
            for xid in &running.xids {
                if !ended.contains(xid) {
                    waiting.insert(*xid);
                }
            }
            self.start = Start::From {
                next_xid: running.next_xid,
                running: waiting,
            };
        }
        // Every xid before the oldest has ended. One with no ending in
        // the WAL died in a crash.
        let oldest = running.oldest_xid;
        let mut ended = Vec::new();
        for (xid, held) in &self.held {
            if !follows(*xid, oldest) {
                ended.push(*xid);
                self.bytes -= held.bytes;
            }
        }
        for xid in ended {
            self.held.remove(&xid);
        }
        if let Start::From {
            running: waiting, ..
        } = &mut self.start
        {
            waiting.retain(|xid| follows(*xid, oldest));
        }
        // The dropped xids stay, though none before the oldest runs: the
        // load's snapshot may be older than this record, taken while one
        // still ran. Only commits before the start add to them.
        if let Start::From { next_xid, .. } = &self.start
            && !first
            && follows(oldest, *next_xid)
        {
            self.start = Start::Whole;
        }
    }

    /// The commits dropped as not whole that `snapshot` doesn't see: it
    /// was taken before PostgreSQL ended them, and can't be the load's.
    pub(crate) fn unseen_dropped(&self, snapshot: &TransactionSnapshot) -> Vec<TransactionId> {
        let mut xids = Vec::new();
        if let Some(dropped) = &self.dropped {
            for xid in dropped {
                if !snapshot.sees(*xid) {
                    xids.push(*xid);
                }
            }
        }
        xids.sort_unstable();
        xids
    }

    /// The load's snapshot saw every commit dropped, and every later
    /// commit is whole: no more to keep.
    pub(crate) fn loaded(&mut self) {
        self.dropped = None;
    }

    /// Drops what `ending` ends, and gives back what was held for it.
    fn ended(&mut self, ending: &Ending) -> Vec<Held> {
        match &mut self.start {
            Start::Unknown { ended } => {
                ended.insert(ending.xid);
            }
            Start::From { running, .. } => {
                running.remove(&ending.xid);
            }
            Start::Whole => {}
        }
        let mut held = Vec::new();
        self.take_held(ending.xid, &mut held);
        for subxact in &ending.subxacts {
            self.take_held(*subxact, &mut held);
        }
        held
    }

    /// Moves what `xid` held into `held`.
    fn take_held(&mut self, xid: TransactionId, held: &mut Vec<Held>) {
        if let Some(part) = self.held.remove(&xid) {
            self.bytes -= part.bytes;
            held.push(part);
        }
    }

    #[cfg(test)]
    fn held_changes(&self) -> usize {
        let mut counted = 0;
        let mut changes = 0;
        for held in self.held.values() {
            changes += held.changes.len();
            for held_change in &held.changes {
                counted += held_change.change.footprint();
            }
        }
        assert_eq!(self.bytes, counted, "bytes as counted");
        changes
    }

    #[cfg(test)]
    fn holds(&self, xid: TransactionId) -> bool {
        self.held.contains_key(&xid)
    }
}

/// Each xid's changes, in WAL order, merged by LSN as the reorder buffer
/// replays them.
fn merge_by_lsn(mut held: Vec<Held>) -> Vec<HeapChange> {
    // One xid, no subtransactions: its changes came in WAL order, and the
    // sort's copies were the cost of most commits.
    if let [part] = held.as_mut_slice() {
        let mut changes = Vec::with_capacity(part.changes.len());
        for held_change in std::mem::take(&mut part.changes) {
            changes.push(held_change.change);
        }
        return changes;
    }
    let mut changes = Vec::new();
    for part in held {
        changes.extend(part.changes);
    }
    changes.sort_by_key(lsn_of);
    let mut merged = Vec::with_capacity(changes.len());
    for held_change in changes {
        merged.push(held_change.change);
    }
    merged
}

fn lsn_of(held_change: &HeldChange) -> u64 {
    held_change.lsn
}

/// `xid` is `from` or later, as xids wrap: `TransactionIdFollowsOrEquals`.
pub(crate) fn follows(xid: TransactionId, from: TransactionId) -> bool {
    i32::from_ne_bytes(xid.wrapping_sub(from).to_ne_bytes()) >= 0
}

/// What a load's snapshot sees: `pg_current_snapshot()`, as
/// `xmin:xmax:xip,...` of 64-bit xids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TransactionSnapshot {
    xmax: u64,
    running: HashSet<u64>,
}

impl TransactionSnapshot {
    /// Reads `pg_current_snapshot()`'s text; `None` if it isn't one.
    pub(crate) fn parse(text: &str) -> Option<TransactionSnapshot> {
        let mut parts = text.split(':');
        let xmin: u64 = parts.next()?.parse().ok()?;
        let xmax: u64 = parts.next()?.parse().ok()?;
        let mut running = HashSet::new();
        let list = parts.next()?;
        if !list.is_empty() {
            for xid in list.split(',') {
                running.insert(xid.parse().ok()?);
            }
        }
        if parts.next().is_some() || xmin > xmax {
            return None;
        }
        Some(TransactionSnapshot { xmax, running })
    }

    /// A snapshot that sees no commit: what the load's comes to once
    /// every transaction running began after it.
    pub(crate) fn seeing_nothing() -> TransactionSnapshot {
        TransactionSnapshot {
            xmax: 0,
            running: HashSet::new(),
        }
    }

    /// The snapshot sees `xid`'s commit: it ended before the snapshot. A
    /// 32-bit xid is read as the 64-bit one nearest `xmax`, so past 2^31
    /// xids on it reads as before; [`TransactionSnapshot::precedes`] says
    /// when to stop asking.
    pub(crate) fn sees(&self, xid: TransactionId) -> bool {
        let Some(full) = self.widen(xid) else {
            return false;
        };
        full < self.xmax && !self.running.contains(&full)
    }

    /// `oldest`, the oldest xid still running, began after the snapshot:
    /// it sees none of the commits to come.
    pub(crate) fn precedes(&self, oldest: TransactionId) -> bool {
        match self.widen(oldest) {
            Some(full) => full >= self.xmax,
            None => false,
        }
    }

    /// `xid` as the 64-bit xid nearest `xmax`.
    fn widen(&self, xid: TransactionId) -> Option<u64> {
        let low = u32::try_from(self.xmax & LOW_32_BITS).unwrap_or(0);
        let distance = i64::from(i32::from_ne_bytes(xid.wrapping_sub(low).to_ne_bytes()));
        self.xmax.checked_add_signed(distance)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::heap::HeapTuple;
    use crate::wal::test_support::{FILE, ROW_HEADER, Rng};

    fn change(number: u64) -> HeapChange {
        HeapChange::Insert {
            file: FILE,
            new: HeapTuple {
                infomask2: 1,
                infomask: 0,
                hoff: ROW_HEADER,
                body: number.to_le_bytes().into(),
            },
        }
    }

    fn ending(xid: TransactionId, subxacts: Vec<TransactionId>) -> Ending {
        Ending {
            xid,
            subxacts,
            time: i64::from(xid),
            invalidated: Vec::new(),
            enums_changed: Vec::new(),
        }
    }

    fn running(
        next_xid: TransactionId,
        oldest_xid: TransactionId,
        xids: Vec<TransactionId>,
    ) -> Event {
        Event::Running(Running {
            next_xid,
            oldest_xid,
            xids,
        })
    }

    /// A transaction of the generated workload, by its index.
    struct Script {
        top: TransactionId,
        subxacts: Vec<TransactionId>,
        /// The subtransaction open now, and how many were opened.
        open_sub: Option<TransactionId>,
        opened: usize,
        kept_subs: Vec<TransactionId>,
        /// What a reference keeps if the transaction commits.
        kept: Vec<u64>,
        /// Changes of the open subtransaction, kept if it is released.
        pending: Vec<u64>,
    }

    fn commit(xid: TransactionId, subxacts: Vec<TransactionId>) -> Event {
        Event::Commit(ending(xid, subxacts))
    }

    /// A transaction and the changes a reference keeps of it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Kept {
        xid: TransactionId,
        changes: Vec<u64>,
    }

    /// The generated workload as it runs, and what a reference expects.
    struct Workload {
        seed: u64,
        scripts: Vec<Script>,
        /// The xid after the last script's.
        next_xid: TransactionId,
        transactions: Transactions,
        lsn: u64,
        /// The last change number handed out.
        counter: u64,
        /// The scripts started so far, in order.
        started: usize,
        /// The scripts started and not yet ended, by index.
        open: Vec<usize>,
        /// The first script started after the first `RUNNING_XACTS`.
        first_whole: Option<usize>,
        expected: Vec<Kept>,
        emitted: Vec<Kept>,
        /// Commits dropped as not whole that a snapshot may still miss.
        dropped: Vec<TransactionId>,
    }

    impl Workload {
        fn new(seed: u64, rng: &mut Rng) -> Workload {
            let mut next_xid = (u32::MAX - 30).wrapping_add(u32::try_from(rng.below(60)).unwrap());
            let mut scripts: Vec<Script> = Vec::new();
            for _ in 0..=rng.below(10) {
                let top = next_xid;
                let count = rng.below(4);
                let mut subxacts = Vec::new();
                for index in 1..=count {
                    subxacts.push(top.wrapping_add(u32::try_from(index).unwrap()));
                }
                next_xid = top.wrapping_add(u32::try_from(count).unwrap() + 1);
                scripts.push(Script {
                    top,
                    subxacts,
                    open_sub: None,
                    opened: 0,
                    kept_subs: Vec::new(),
                    kept: Vec::new(),
                    pending: Vec::new(),
                });
            }
            Workload {
                seed,
                scripts,
                next_xid,
                transactions: Transactions::new(1_000),
                lsn: 0x100,
                counter: 0,
                started: 0,
                open: Vec::new(),
                first_whole: None,
                expected: Vec::new(),
                emitted: Vec::new(),
                dropped: Vec::new(),
            }
        }

        /// One step of the workload: `false` once every script ended.
        fn step(&mut self, rng: &mut Rng) -> bool {
            self.lsn += 8;
            let roll = rng.below(10);
            let all_ended = self.started == self.scripts.len() && self.open.is_empty();
            if roll == 0 || all_ended {
                self.log_running();
                return !all_ended;
            }
            if self.started < self.scripts.len() && (roll < 3 || self.open.is_empty()) {
                self.open.push(self.started);
                self.started += 1;
                return true;
            }
            let pick = usize::try_from(rng.below(u64::try_from(self.open.len()).unwrap())).unwrap();
            let index = self.open[pick];
            match rng.below(6) {
                0..=2 => self.change(index),
                3 => self.open_savepoint(index),
                4 if self.scripts[index].open_sub.is_some() => self.end_savepoint(rng, index),
                _ => self.end(rng, pick),
            }
            true
        }

        fn log_running(&mut self) {
            let seed = self.seed;
            let next = match self.scripts.get(self.started) {
                Some(script) => script.top,
                None => self.next_xid,
            };
            let mut names = Vec::new();
            for index in &self.open {
                names.push(self.scripts[*index].top);
            }
            let oldest = names.first().copied().unwrap_or(next);
            let event = running(next, oldest, names);
            let lsn = self.lsn;
            assert_eq!(self.transactions.event(event, lsn, lsn + 4), Ok(None));
            self.dropped.sort_unstable();
            let unseen = self
                .transactions
                .unseen_dropped(&TransactionSnapshot::seeing_nothing());
            assert_eq!(unseen, self.dropped, "seed {seed}");
            self.first_whole.get_or_insert(self.started);
        }

        fn change(&mut self, index: usize) {
            self.counter += 1;
            let script = &mut self.scripts[index];
            let xid = script.open_sub.unwrap_or(script.top);
            self.transactions
                .hold(xid, self.lsn, &mut vec![change(self.counter)]);
            if script.open_sub.is_some() {
                script.pending.push(self.counter);
            } else {
                script.kept.push(self.counter);
            }
        }

        fn open_savepoint(&mut self, index: usize) {
            let script = &mut self.scripts[index];
            if script.open_sub.is_none() && script.opened < script.subxacts.len() {
                script.open_sub = Some(script.subxacts[script.opened]);
                script.opened += 1;
            }
        }

        /// Releases the open savepoint, or rolls it back.
        fn end_savepoint(&mut self, rng: &mut Rng, index: usize) {
            let seed = self.seed;
            let script = &mut self.scripts[index];
            let Some(sub) = script.open_sub.take() else {
                return;
            };
            if rng.below(2) == 0 {
                script.kept_subs.push(sub);
                script.kept.append(&mut script.pending);
                return;
            }
            script.pending.clear();
            let event = Event::Abort(ending(sub, Vec::new()));
            let lsn = self.lsn;
            assert_eq!(self.transactions.event(event, lsn, lsn + 4), Ok(None));
            assert!(!self.transactions.holds(sub), "seed {seed}");
        }

        /// Commits, aborts or crashes the script open at `pick`.
        fn end(&mut self, rng: &mut Rng, pick: usize) {
            let (seed, lsn) = (self.seed, self.lsn);
            let index = self.open.remove(pick);
            let script = &mut self.scripts[index];
            // COMMIT releases an open savepoint.
            if let Some(sub) = script.open_sub.take() {
                script.kept_subs.push(sub);
                script.kept.append(&mut script.pending);
            }
            let subs = script.kept_subs.clone();
            let outcome = rng.below(5);
            let event = match outcome {
                0 => return,
                1 => Event::Abort(ending(script.top, subs)),
                _ => Event::Commit(ending(script.top, subs)),
            };
            let whole = match self.first_whole {
                Some(first) => index >= first,
                None => false,
            };
            if outcome > 1 && whole && !script.kept.is_empty() {
                self.expected.push(Kept {
                    xid: script.top,
                    changes: script.kept.clone(),
                });
            }
            if outcome > 1 && !whole {
                self.dropped.push(script.top);
            }
            let got = self.transactions.event(event, lsn, lsn + 4).unwrap();
            // The byte count stays true at every step.
            self.transactions.held_changes();
            self.dropped.sort_unstable();
            let unseen = self
                .transactions
                .unseen_dropped(&TransactionSnapshot::seeing_nothing());
            assert_eq!(unseen, self.dropped, "seed {seed}");
            assert!(!self.transactions.holds(script.top), "seed {seed}");
            for sub in &script.kept_subs {
                assert!(!self.transactions.holds(*sub), "seed {seed}");
            }
            if let Some(committed) = got {
                assert_eq!((committed.lsn, committed.end), (lsn, lsn + 4));
                let changes = numbers(committed.changes);
                assert!(!changes.is_empty(), "seed {seed}: empty commit");
                self.emitted.push(Kept {
                    xid: committed.xid,
                    changes,
                });
            }
        }

        fn finish(&self) {
            let seed = self.seed;
            assert_eq!(self.emitted, self.expected, "seed {seed}");
            assert_eq!(
                self.transactions.consistency(),
                Consistency::Consistent,
                "seed {seed}"
            );
            assert_eq!(self.transactions.held_changes(), 0, "seed {seed}");
        }
    }

    /// The numbers `change` made `changes` of.
    fn numbers(changes: Vec<HeapChange>) -> Vec<u64> {
        let mut numbers = Vec::new();
        for change in changes {
            let HeapChange::Insert { new, .. } = change else {
                unreachable!()
            };
            numbers.push(u64::from_le_bytes(new.body[..].try_into().unwrap()));
        }
        numbers
    }

    /// Random interleavings of transactions that change rows in the top
    /// and in subtransactions, release or roll back savepoints, commit,
    /// abort or vanish in a crash, with `RUNNING_XACTS` between: what
    /// commits is each committed transaction that started after the
    /// first `RUNNING_XACTS`, with its changes and its released
    /// subtransactions', in WAL order. Nothing stays held.
    #[test]
    fn commits_what_a_reference_commits() {
        for seed in 0..500 {
            let mut rng = Rng(seed);
            let mut workload = Workload::new(seed, &mut rng);
            while workload.step(&mut rng) {}
            workload.finish();
        }
    }

    #[test]
    fn stops_at_the_limit() {
        let mut transactions = Transactions::new(2);
        assert_eq!(transactions.event(running(10, 10, vec![]), 8, 12), Ok(None));
        assert_eq!(transactions.consistency(), Consistency::Consistent);
        transactions.hold(10, 16, &mut vec![change(1), change(2)]);
        transactions.hold(11, 24, &mut vec![change(3), change(4)]);
        let got = transactions
            .event(commit(10, vec![11]), 32, 40)
            .unwrap()
            .unwrap();
        assert_eq!(got.changes, [change(1), change(2), change(3), change(4)]);
        assert_eq!(got.time, 10);
        transactions.hold(12, 48, &mut vec![change(5), change(6), change(7)]);
        transactions.hold(12, 56, &mut vec![change(8)]);
        assert_eq!(transactions.held_changes(), 0);
        assert_eq!(
            transactions.event(commit(12, vec![]), 64, 72),
            Err(TooBig { xid: 12, lsn: 64 })
        );
        assert!(!transactions.holds(12));
        // Past the limit but aborted, or not whole: nothing to say.
        transactions.hold(13, 80, &mut vec![change(9), change(10), change(11)]);
        let abort = Event::Abort(ending(13, vec![]));
        assert_eq!(transactions.event(abort, 88, 96), Ok(None));
        transactions.hold(9, 104, &mut vec![change(12), change(13), change(14)]);
        assert_eq!(transactions.event(commit(9, vec![]), 112, 120), Ok(None));
    }

    #[test]
    fn waits_for_the_transactions_running_at_the_start() {
        let mut transactions = Transactions::new(10);
        transactions.hold(5, 8, &mut vec![change(1)]);
        // Before any RUNNING_XACTS nothing is known whole.
        assert_eq!(transactions.event(commit(5, vec![]), 16, 20), Ok(None));
        assert_ne!(transactions.consistency(), Consistency::Consistent);
        transactions.hold(6, 24, &mut vec![change(2)]);
        assert_eq!(transactions.consistency(), Consistency::Unknown);
        assert_eq!(
            transactions.event(running(8, 6, vec![7, 6]), 32, 36),
            Ok(None)
        );
        assert_eq!(transactions.consistency(), Consistency::Waiting(vec![6, 7]));
        transactions.hold(8, 40, &mut vec![change(3)]);
        assert_eq!(transactions.event(commit(6, vec![]), 48, 52), Ok(None));
        assert_ne!(transactions.consistency(), Consistency::Consistent);
        // 7 dies in a crash: a later RUNNING_XACTS has it gone.
        assert_eq!(transactions.event(running(9, 8, vec![8]), 56, 60), Ok(None));
        assert_eq!(transactions.consistency(), Consistency::Consistent);
        let got = transactions
            .event(commit(8, vec![]), 64, 68)
            .unwrap()
            .unwrap();
        assert_eq!((got.xid, got.changes), (8, vec![change(3)]));
        assert_eq!(transactions.held_changes(), 0);
    }

    /// A transaction can end between PostgreSQL listing it as running and
    /// the `RUNNING_XACTS` going into the WAL.
    #[test]
    fn knows_what_ended_before_the_record_that_names_it() {
        let mut transactions = Transactions::new(10);
        transactions.hold(7, 8, &mut vec![change(1)]);
        let commit = Event::Commit(ending(7, vec![]));
        assert_eq!(transactions.event(commit, 16, 20), Ok(None));
        assert_eq!(
            transactions.event(running(9, 7, vec![7, 8]), 24, 28),
            Ok(None)
        );
        assert_ne!(transactions.consistency(), Consistency::Consistent);
        let abort = Event::Abort(ending(8, vec![]));
        assert_eq!(transactions.event(abort, 32, 36), Ok(None));
        assert_eq!(transactions.consistency(), Consistency::Consistent);
        assert_eq!(transactions.held_changes(), 0);
    }

    /// Commits dropped as not whole wait for the load's snapshot to see
    /// them, though PostgreSQL says none older runs: the snapshot may be
    /// older than that.
    #[test]
    fn keeps_what_it_dropped_for_the_load_to_see() {
        let mut transactions = Transactions::new(10);
        let blind = TransactionSnapshot::seeing_nothing();
        assert_eq!(transactions.event(commit(5, vec![]), 8, 12), Ok(None));
        assert_eq!(
            transactions.event(running(8, 6, vec![6, 7]), 16, 20),
            Ok(None)
        );
        assert_eq!(transactions.event(commit(6, vec![]), 24, 28), Ok(None));
        // An abort, and a whole commit, aren't the load's to see.
        let abort = Event::Abort(ending(7, vec![]));
        assert_eq!(transactions.event(abort, 32, 36), Ok(None));
        transactions.hold(8, 40, &mut vec![change(1)]);
        assert!(
            transactions
                .event(commit(8, vec![]), 48, 52)
                .unwrap()
                .is_some()
        );
        assert_eq!(transactions.unseen_dropped(&blind), [5, 6]);
        let before = TransactionSnapshot::parse("6:9:6").unwrap();
        assert_eq!(transactions.unseen_dropped(&before), [6]);
        let after = TransactionSnapshot::parse("9:9:").unwrap();
        assert_eq!(transactions.unseen_dropped(&after), Vec::<u32>::new());
        // None before 7 runs now, yet a snapshot taken earlier missed 6.
        assert_eq!(transactions.event(running(9, 7, vec![]), 56, 60), Ok(None));
        assert_eq!(transactions.unseen_dropped(&before), [6]);
        assert_eq!(transactions.unseen_dropped(&after), Vec::<u32>::new());
        transactions.loaded();
        assert_eq!(transactions.event(commit(6, vec![]), 72, 76), Ok(None));
        assert_eq!(transactions.unseen_dropped(&blind), Vec::<u32>::new());
    }

    #[test]
    fn brings_commits_that_only_invalidate() {
        let mut transactions = Transactions::new(10);
        assert_eq!(transactions.event(running(5, 5, vec![]), 8, 12), Ok(None));
        let invalidated = vec![Invalidated {
            database: 5,
            relation: 16_384,
        }];
        let ddl = Ending {
            invalidated: invalidated.clone(),
            ..ending(5, vec![])
        };
        let got = transactions
            .event(Event::Commit(ddl), 16, 24)
            .unwrap()
            .unwrap();
        assert_eq!((got.changes, got.invalidated), (Vec::new(), invalidated));
        let plain = Event::Commit(ending(6, vec![]));
        assert_eq!(transactions.event(plain, 32, 40), Ok(None));
    }

    #[test]
    fn snapshots_see_what_ended_before_them() {
        let snapshot =
            TransactionSnapshot::parse("4294967290:4294967300:4294967295,4294967298").unwrap();
        // 4294967296 is xid 0 of epoch 1; the 32-bit xids around it wrap.
        for (xid, seen) in [
            (4_294_967_289, true),
            (4_294_967_294, true),
            (4_294_967_295, false),
            (0, true),
            (1, true),
            (2, false),
            (3, true),
            (4, false),
            (5, false),
            (1_000, false),
        ] {
            assert_eq!(snapshot.sees(xid), seen, "{xid}");
        }
        let empty = TransactionSnapshot::parse("750:750:").unwrap();
        assert!(empty.sees(749) && !empty.sees(750) && !empty.sees(751));
        let early = TransactionSnapshot::parse("3:5:").unwrap();
        assert!(early.sees(4) && !early.sees(u32::MAX));
        for bad in [
            "", "1", "1:2", "2:1:", "1:2:x", "1:2:3:4", "a:2:", "1:b:", "1:2:3,",
        ] {
            assert_eq!(TransactionSnapshot::parse(bad), None, "{bad}");
        }
    }

    /// The load's snapshot reads an xid as the 64-bit one nearest its
    /// `xmax`, and precedes every xid at or past it.
    #[test]
    fn a_snapshot_precedes_what_began_after_it() {
        let snapshot = TransactionSnapshot::parse("100:105:102").unwrap();
        assert!(snapshot.sees(101) && !snapshot.sees(102) && !snapshot.sees(105));
        assert!(snapshot.precedes(105) && snapshot.precedes(106));
        assert!(!snapshot.precedes(104) && !snapshot.precedes(102));
        let nothing = TransactionSnapshot::seeing_nothing();
        for xid in [0, 1, 105, 105 + (1 << 31), u32::MAX] {
            assert!(!nothing.sees(xid), "{xid}");
        }
    }
}
