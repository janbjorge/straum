//! Changes held by transaction until it ends, as logical decoding's
//! reorder buffer holds them (`reorderbuffer.c`), and the point from
//! which every commit's changes are all in the WAL Straum reads, as
//! `snapbuild.c` finds it.
//!
//! Straum starts at a redo point, so a transaction running there may have
//! changed rows before it. The first `RUNNING_XACTS` names what runs;
//! once all of those end, every later transaction began after it, whole.
//! Commits of transactions before that point are dropped: the load,
//! taken after it, must see them. PostgreSQL logs a commit before it
//! ends the transaction for snapshots, and waits for synchronous standbys
//! in between, so the dropped xids are kept for the load to check.

use std::collections::HashSet;

use straum_domain::FastMap;

use super::heap::HeapChange;
use super::xact::{Ending, Event, Invalidated, Running};

/// A transaction that committed, with every change it made to the
/// tracked tables, in WAL order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Committed {
    pub(crate) xid: u32,
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
    Waiting(Vec<u32>),
    Consistent,
}

/// A transaction past the limit commits: its changes weren't all kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TooBig {
    pub(crate) xid: u32,
    pub(crate) lsn: u64,
}

pub(crate) struct Transactions {
    /// By xid, top-level or sub, with the LSN of each change's record.
    held: FastMap<u32, Held>,
    /// The held changes' footprints, summed.
    bytes: usize,
    /// The most changes one xid keeps; past it, only that it had more.
    limit: usize,
    start: Start,
    /// The xids of commits dropped as not whole, until the load's
    /// snapshot: it must see each. `None` once it did.
    dropped: Option<HashSet<u32>>,
}

#[derive(Default)]
struct Held {
    changes: Vec<(u64, HeapChange)>,
    bytes: usize,
    over: bool,
}

enum Start {
    /// No `RUNNING_XACTS` yet; the xids that ended so far.
    Unknown { ended: HashSet<u32> },
    /// The first one: transactions from `next_xid` on are whole, since
    /// PostgreSQL holds `XidGenLock` until the record is in the WAL
    /// (`LogStandbySnapshot`), so no later xid wrote before it. Those it
    /// names that haven't ended yet keep Straum from being consistent.
    From {
        next_xid: u32,
        running: HashSet<u32>,
    },
}

impl Transactions {
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

    pub(crate) fn hold(&mut self, xid: u32, lsn: u64, changes: Vec<HeapChange>) {
        if changes.is_empty() {
            return;
        }
        let held = self.held.entry(xid).or_default();
        for change in changes {
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
                held.changes.push((lsn, change));
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
            Start::From { running, .. } if running.is_empty() => Consistency::Consistent,
            Start::From { running, .. } => {
                let mut xids: Vec<u32> = running.iter().copied().collect();
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

    fn commit(&mut self, ending: Ending, lsn: u64, end: u64) -> Result<Option<Committed>, TooBig> {
        let whole = match &self.start {
            Start::From { next_xid, .. } => follows(ending.xid, *next_xid),
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

    fn running(&mut self, running: &Running) {
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
        // PostgreSQL found none before the oldest still running, so any
        // later snapshot sees them.
        if let Some(dropped) = &mut self.dropped {
            dropped.retain(|xid| follows(*xid, oldest));
        }
    }

    /// The commits dropped as not whole that `snapshot` doesn't see: it
    /// was taken before PostgreSQL ended them, and can't be the load's.
    pub(crate) fn unseen_dropped(&self, snapshot: &TransactionSnapshot) -> Vec<u32> {
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
        }
        let mut held = Vec::new();
        self.take_held(ending.xid, &mut held);
        for subxact in &ending.subxacts {
            self.take_held(*subxact, &mut held);
        }
        held
    }

    /// Moves what `xid` held into `held`.
    fn take_held(&mut self, xid: u32, held: &mut Vec<Held>) {
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
            for (_, change) in &held.changes {
                counted += change.footprint();
            }
        }
        assert_eq!(self.bytes, counted, "bytes as counted");
        changes
    }

    #[cfg(test)]
    fn holds(&self, xid: u32) -> bool {
        self.held.contains_key(&xid)
    }
}

/// Each xid's changes, in WAL order, merged by LSN as the reorder buffer
/// replays them.
fn merge_by_lsn(held: Vec<Held>) -> Vec<HeapChange> {
    let mut changes = Vec::new();
    for part in held {
        changes.extend(part.changes);
    }
    changes.sort_by_key(lsn_of);
    let mut merged = Vec::with_capacity(changes.len());
    for (_, change) in changes {
        merged.push(change);
    }
    merged
}

fn lsn_of(change: &(u64, HeapChange)) -> u64 {
    change.0
}

/// `xid` is `from` or later, as xids wrap: `TransactionIdFollowsOrEquals`.
pub(crate) fn follows(xid: u32, from: u32) -> bool {
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

    /// The snapshot sees `xid`'s commit: it ended before the snapshot. A
    /// 32-bit xid is read as the 64-bit one nearest `xmax`.
    pub(crate) fn sees(&self, xid: u32) -> bool {
        let low = u32::try_from(self.xmax & 0xFFFF_FFFF).unwrap_or(0);
        let distance = i64::from(i32::from_ne_bytes(xid.wrapping_sub(low).to_ne_bytes()));
        let Some(full) = self.xmax.checked_add_signed(distance) else {
            return false;
        };
        full < self.xmax && !self.running.contains(&full)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::heap::HeapTuple;
    use crate::wal::test_support::{FILE, Rng};

    fn change(number: u64) -> HeapChange {
        HeapChange::Insert {
            file: FILE,
            new: HeapTuple {
                infomask2: 1,
                infomask: 0,
                hoff: 24,
                body: number.to_le_bytes().into(),
            },
        }
    }

    fn ending(xid: u32, subxacts: Vec<u32>) -> Ending {
        Ending {
            xid,
            subxacts,
            time: i64::from(xid),
            invalidated: Vec::new(),
        }
    }

    fn running(next_xid: u32, oldest_xid: u32, xids: Vec<u32>) -> Event {
        Event::Running(Running {
            next_xid,
            oldest_xid,
            xids,
        })
    }

    /// A transaction of the generated workload, by its index.
    struct Script {
        top: u32,
        subxacts: Vec<u32>,
        /// The subtransaction open now, and how many were opened.
        open_sub: Option<u32>,
        opened: usize,
        kept_subs: Vec<u32>,
        /// What a reference keeps if the transaction commits.
        kept: Vec<u64>,
        /// Changes of the open subtransaction, kept if it is released.
        pending: Vec<u64>,
    }

    fn commit(xid: u32, subxacts: Vec<u32>) -> Event {
        Event::Commit(ending(xid, subxacts))
    }

    /// A snapshot that sees nothing.
    fn blind() -> TransactionSnapshot {
        TransactionSnapshot::parse("0:0:").unwrap()
    }

    /// A transaction and the changes a reference keeps of it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Kept {
        xid: u32,
        changes: Vec<u64>,
    }

    /// The generated workload as it runs, and what a reference expects.
    struct Workload {
        seed: u64,
        scripts: Vec<Script>,
        /// The xid after the last script's.
        next_xid: u32,
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
        dropped: Vec<u32>,
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
            self.dropped.retain(|xid| follows(*xid, oldest));
            self.dropped.sort_unstable();
            let unseen = self.transactions.unseen_dropped(&blind());
            assert_eq!(unseen, self.dropped, "seed {seed}");
            self.first_whole.get_or_insert(self.started);
        }

        fn change(&mut self, index: usize) {
            self.counter += 1;
            let script = &mut self.scripts[index];
            let xid = script.open_sub.unwrap_or(script.top);
            self.transactions
                .hold(xid, self.lsn, vec![change(self.counter)]);
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
            let unseen = self.transactions.unseen_dropped(&blind());
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
        transactions.hold(10, 16, vec![change(1), change(2)]);
        transactions.hold(11, 24, vec![change(3), change(4)]);
        let got = transactions
            .event(commit(10, vec![11]), 32, 40)
            .unwrap()
            .unwrap();
        assert_eq!(got.changes, [change(1), change(2), change(3), change(4)]);
        assert_eq!(got.time, 10);
        transactions.hold(12, 48, vec![change(5), change(6), change(7)]);
        transactions.hold(12, 56, vec![change(8)]);
        assert_eq!(transactions.held_changes(), 0);
        assert_eq!(
            transactions.event(commit(12, vec![]), 64, 72),
            Err(TooBig { xid: 12, lsn: 64 })
        );
        assert!(!transactions.holds(12));
        // Past the limit but aborted, or not whole: nothing to say.
        transactions.hold(13, 80, vec![change(9), change(10), change(11)]);
        let abort = Event::Abort(ending(13, vec![]));
        assert_eq!(transactions.event(abort, 88, 96), Ok(None));
        transactions.hold(9, 104, vec![change(12), change(13), change(14)]);
        assert_eq!(transactions.event(commit(9, vec![]), 112, 120), Ok(None));
    }

    #[test]
    fn waits_for_the_transactions_running_at_the_start() {
        let mut transactions = Transactions::new(10);
        transactions.hold(5, 8, vec![change(1)]);
        // Before any RUNNING_XACTS nothing is known whole.
        assert_eq!(transactions.event(commit(5, vec![]), 16, 20), Ok(None));
        assert_ne!(transactions.consistency(), Consistency::Consistent);
        transactions.hold(6, 24, vec![change(2)]);
        assert_eq!(transactions.consistency(), Consistency::Unknown);
        assert_eq!(
            transactions.event(running(8, 6, vec![7, 6]), 32, 36),
            Ok(None)
        );
        assert_eq!(transactions.consistency(), Consistency::Waiting(vec![6, 7]));
        transactions.hold(8, 40, vec![change(3)]);
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
        transactions.hold(7, 8, vec![change(1)]);
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
    /// them, until PostgreSQL says none older runs.
    #[test]
    fn keeps_what_it_dropped_for_the_load_to_see() {
        let mut transactions = Transactions::new(10);
        let blind = blind();
        assert_eq!(transactions.event(commit(5, vec![]), 8, 12), Ok(None));
        assert_eq!(
            transactions.event(running(8, 6, vec![6, 7]), 16, 20),
            Ok(None)
        );
        assert_eq!(transactions.event(commit(6, vec![]), 24, 28), Ok(None));
        // An abort, and a whole commit, aren't the load's to see.
        let abort = Event::Abort(ending(7, vec![]));
        assert_eq!(transactions.event(abort, 32, 36), Ok(None));
        transactions.hold(8, 40, vec![change(1)]);
        assert!(
            transactions
                .event(commit(8, vec![]), 48, 52)
                .unwrap()
                .is_some()
        );
        // 5 ended before 6, the oldest running then: every snapshot sees it.
        assert_eq!(transactions.unseen_dropped(&blind), [6]);
        let before = TransactionSnapshot::parse("6:9:6").unwrap();
        assert_eq!(transactions.unseen_dropped(&before), [6]);
        let after = TransactionSnapshot::parse("9:9:").unwrap();
        assert_eq!(transactions.unseen_dropped(&after), Vec::<u32>::new());
        assert_eq!(transactions.event(running(9, 7, vec![]), 56, 60), Ok(None));
        assert_eq!(transactions.unseen_dropped(&blind), Vec::<u32>::new());
        assert_eq!(transactions.event(commit(6, vec![]), 64, 68), Ok(None));
        assert_eq!(transactions.unseen_dropped(&blind), [6]);
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
}
