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

use super::heap::Change;
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
    pub(crate) changes: Vec<Change>,
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
    changes: Vec<(u64, Change)>,
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

    pub(crate) fn hold(&mut self, xid: u32, lsn: u64, changes: Vec<Change>) {
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
            Event::Commit(ending) => {
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
                if held.iter().any(|h| h.over) {
                    return Err(TooBig {
                        xid: ending.xid,
                        lsn,
                    });
                }
                // pgoutput sends nothing for a transaction that changed
                // no published table. One that invalidated a table may
                // have changed how its rows read, so it still comes.
                if held.iter().all(|h| h.changes.is_empty()) && ending.invalidated.is_empty() {
                    return Ok(None);
                }
                // Each xid's changes are in WAL order; together they
                // merge by LSN, as the reorder buffer replays them.
                let mut changes: Vec<(u64, Change)> =
                    held.into_iter().flat_map(|h| h.changes).collect();
                changes.sort_by_key(|(lsn, _)| *lsn);
                Ok(Some(Committed {
                    xid: ending.xid,
                    lsn,
                    end,
                    time: ending.time,
                    changes: changes.into_iter().map(|(_, c)| c).collect(),
                    invalidated: ending.invalidated,
                }))
            }
        }
    }

    fn running(&mut self, running: &Running) {
        if let Start::Unknown { ended } = &self.start {
            // The record lists what ran when PostgreSQL looked, and goes
            // into the WAL after: a transaction may end in between, its
            // ending before the record that names it (`snapbuild.c`).
            let waiting = (running.xids.iter())
                .filter(|xid| !ended.contains(xid))
                .copied()
                .collect();
            self.start = Start::From {
                next_xid: running.next_xid,
                running: waiting,
            };
        }
        // Every xid before the oldest has ended. One with no ending in
        // the WAL died in a crash.
        let oldest = running.oldest_xid;
        let mut freed = 0;
        self.held.retain(|xid, held| {
            let keep = follows(*xid, oldest);
            if !keep {
                freed += held.bytes;
            }
            keep
        });
        self.bytes -= freed;
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
    pub(crate) fn unseen_dropped(&self, snapshot: &Snapshot) -> Vec<u32> {
        let mut xids: Vec<u32> = (self.dropped.iter().flatten())
            .copied()
            .filter(|xid| !snapshot.sees(*xid))
            .collect();
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
        let xids = std::iter::once(ending.xid).chain(ending.subxacts.iter().copied());
        let held: Vec<Held> = xids.filter_map(|xid| self.held.remove(&xid)).collect();
        self.bytes -= held.iter().map(|h| h.bytes).sum::<usize>();
        held
    }

    #[cfg(test)]
    fn held_changes(&self) -> usize {
        let counted: usize = (self.held.values().flat_map(|h| &h.changes))
            .map(|(_, c)| c.footprint())
            .sum();
        assert_eq!(self.bytes, counted, "bytes as counted");
        self.held.values().map(|h| h.changes.len()).sum()
    }

    #[cfg(test)]
    fn holds(&self, xid: u32) -> bool {
        self.held.contains_key(&xid)
    }
}

/// `xid` is `from` or later, as xids wrap: `TransactionIdFollowsOrEquals`.
pub(crate) fn follows(xid: u32, from: u32) -> bool {
    i32::from_ne_bytes(xid.wrapping_sub(from).to_ne_bytes()) >= 0
}

/// What a load's snapshot sees: `pg_current_snapshot()`, as
/// `xmin:xmax:xip,...` of 64-bit xids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    xmax: u64,
    running: HashSet<u64>,
}

impl Snapshot {
    pub(crate) fn parse(text: &str) -> Option<Snapshot> {
        let mut parts = text.split(':');
        let xmin: u64 = parts.next()?.parse().ok()?;
        let xmax: u64 = parts.next()?.parse().ok()?;
        let running = match parts.next()? {
            "" => HashSet::new(),
            list => list
                .split(',')
                .map(|x| x.parse().ok())
                .collect::<Option<_>>()?,
        };
        (parts.next().is_none() && xmin <= xmax).then_some(Snapshot { xmax, running })
    }

    /// The snapshot sees `xid`'s commit: it ended before the snapshot. A
    /// 32-bit xid is read as the 64-bit one nearest `xmax`.
    pub(crate) fn sees(&self, xid: u32) -> bool {
        let low = u32::try_from(self.xmax & 0xFFFF_FFFF).unwrap_or(0);
        let distance = i64::from(i32::from_ne_bytes(xid.wrapping_sub(low).to_ne_bytes()));
        let full = self.xmax.checked_add_signed(distance);
        full.is_some_and(|full| full < self.xmax && !self.running.contains(&full))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::RelationFile;
    use crate::wal::heap::Tuple;

    const FILE: RelationFile = RelationFile {
        tablespace: 1663,
        database: 5,
        relation: 16_384,
    };

    /// A splitmix generator, so failures replay.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn change(n: u64) -> Change {
        Change::Insert {
            file: FILE,
            new: Tuple {
                infomask2: 1,
                infomask: 0,
                hoff: 24,
                body: n.to_le_bytes().to_vec(),
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
            let mut next_xid = (u32::MAX - 30).wrapping_add(u32::try_from(rng.below(60)).unwrap());
            let mut scripts: Vec<Script> = Vec::new();
            for _ in 0..1 + rng.below(10) {
                let top = next_xid;
                let count = rng.below(4);
                let subxacts = (1..=count)
                    .map(|i| top.wrapping_add(u32::try_from(i).unwrap()))
                    .collect();
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
            let mut transactions = Transactions::new(1_000);
            let mut lsn = 0x100;
            let mut counter = 0;
            let mut started = 0;
            let mut open: Vec<usize> = Vec::new();
            let mut first_whole: Option<usize> = None;
            let mut expected = Vec::new();
            let mut emitted = Vec::new();
            // Commits dropped as not whole that a snapshot may still miss.
            let mut dropped: Vec<u32> = Vec::new();
            let blind = Snapshot::parse("0:0:").unwrap();
            loop {
                lsn += 8;
                let roll = rng.below(10);
                let next = scripts.get(started).map_or(next_xid, |s| s.top);
                if roll == 0 || (started == scripts.len() && open.is_empty()) {
                    let names: Vec<u32> = open.iter().map(|i| scripts[*i].top).collect();
                    let oldest = names.first().copied().unwrap_or(next);
                    let event = running(next, oldest, names);
                    assert_eq!(transactions.event(event, lsn, lsn + 4), Ok(None));
                    dropped.retain(|xid| follows(*xid, oldest));
                    dropped.sort_unstable();
                    assert_eq!(transactions.unseen_dropped(&blind), dropped, "seed {seed}");
                    first_whole.get_or_insert(started);
                    if started == scripts.len() && open.is_empty() {
                        break;
                    }
                    continue;
                }
                if started < scripts.len() && (roll < 3 || open.is_empty()) {
                    open.push(started);
                    started += 1;
                    continue;
                }
                let pick = usize::try_from(rng.below(u64::try_from(open.len()).unwrap())).unwrap();
                let index = open[pick];
                let script = &mut scripts[index];
                match rng.below(6) {
                    0..=2 => {
                        counter += 1;
                        let xid = script.open_sub.unwrap_or(script.top);
                        transactions.hold(xid, lsn, vec![change(counter)]);
                        if script.open_sub.is_some() {
                            script.pending.push(counter);
                        } else {
                            script.kept.push(counter);
                        }
                    }
                    3 if script.open_sub.is_none() && script.opened < script.subxacts.len() => {
                        script.open_sub = Some(script.subxacts[script.opened]);
                        script.opened += 1;
                    }
                    3 => {}
                    4 if script.open_sub.is_some() => {
                        let sub = script.open_sub.take().unwrap();
                        if rng.below(2) == 0 {
                            script.kept_subs.push(sub);
                            script.kept.append(&mut script.pending);
                        } else {
                            script.pending.clear();
                            let event = Event::Abort(ending(sub, Vec::new()));
                            assert_eq!(transactions.event(event, lsn, lsn + 4), Ok(None));
                            assert!(!transactions.holds(sub), "seed {seed}");
                        }
                    }
                    _ => {
                        // COMMIT releases an open savepoint.
                        if let Some(sub) = script.open_sub.take() {
                            script.kept_subs.push(sub);
                            script.kept.append(&mut script.pending);
                        }
                        open.remove(pick);
                        let subs = script.kept_subs.clone();
                        let outcome = rng.below(5);
                        let event = match outcome {
                            0 => continue,
                            1 => Event::Abort(ending(script.top, subs)),
                            _ => Event::Commit(ending(script.top, subs)),
                        };
                        let whole = first_whole.is_some_and(|first| index >= first);
                        if outcome > 1 && whole && !script.kept.is_empty() {
                            expected.push((script.top, script.kept.clone()));
                        }
                        if outcome > 1 && !whole {
                            dropped.push(script.top);
                        }
                        let got = transactions.event(event, lsn, lsn + 4).unwrap();
                        // The byte count stays true at every step.
                        transactions.held_changes();
                        dropped.sort_unstable();
                        assert_eq!(transactions.unseen_dropped(&blind), dropped, "seed {seed}");
                        let ended = std::iter::once(script.top).chain(script.kept_subs.clone());
                        assert!(
                            ended.into_iter().all(|x| !transactions.holds(x)),
                            "seed {seed}"
                        );
                        if let Some(committed) = got {
                            assert_eq!((committed.lsn, committed.end), (lsn, lsn + 4));
                            let numbers: Vec<u64> = (committed.changes.into_iter())
                                .map(|c| match c {
                                    Change::Insert { new, .. } => {
                                        u64::from_le_bytes(new.body.try_into().unwrap())
                                    }
                                    _ => unreachable!(),
                                })
                                .collect();
                            assert!(!numbers.is_empty(), "seed {seed}: empty commit");
                            emitted.push((committed.xid, numbers));
                        }
                    }
                }
            }
            assert_eq!(emitted, expected, "seed {seed}");
            assert!(
                transactions.consistency() == Consistency::Consistent,
                "seed {seed}"
            );
            assert_eq!(transactions.held_changes(), 0, "seed {seed}");
        }
    }

    #[test]
    fn stops_at_the_limit() {
        let mut transactions = Transactions::new(2);
        let commit = |xid: u32, subxacts: Vec<u32>| Event::Commit(ending(xid, subxacts));
        assert_eq!(transactions.event(running(10, 10, vec![]), 8, 12), Ok(None));
        assert!(transactions.consistency() == Consistency::Consistent);
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
        let commit = |xid: u32| Event::Commit(ending(xid, vec![]));
        // Before any RUNNING_XACTS nothing is known whole.
        assert_eq!(transactions.event(commit(5), 16, 20), Ok(None));
        assert!(transactions.consistency() != Consistency::Consistent);
        transactions.hold(6, 24, vec![change(2)]);
        assert_eq!(transactions.consistency(), Consistency::Unknown);
        assert_eq!(
            transactions.event(running(8, 6, vec![7, 6]), 32, 36),
            Ok(None)
        );
        assert_eq!(transactions.consistency(), Consistency::Waiting(vec![6, 7]));
        transactions.hold(8, 40, vec![change(3)]);
        assert_eq!(transactions.event(commit(6), 48, 52), Ok(None));
        assert!(transactions.consistency() != Consistency::Consistent);
        // 7 dies in a crash: a later RUNNING_XACTS has it gone.
        assert_eq!(transactions.event(running(9, 8, vec![8]), 56, 60), Ok(None));
        assert!(transactions.consistency() == Consistency::Consistent);
        let got = transactions.event(commit(8), 64, 68).unwrap().unwrap();
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
        assert!(transactions.consistency() != Consistency::Consistent);
        let abort = Event::Abort(ending(8, vec![]));
        assert_eq!(transactions.event(abort, 32, 36), Ok(None));
        assert!(transactions.consistency() == Consistency::Consistent);
        assert_eq!(transactions.held_changes(), 0);
    }

    /// Commits dropped as not whole wait for the load's snapshot to see
    /// them, until PostgreSQL says none older runs.
    #[test]
    fn keeps_what_it_dropped_for_the_load_to_see() {
        let mut transactions = Transactions::new(10);
        let commit = |xid: u32| Event::Commit(ending(xid, vec![]));
        let blind = Snapshot::parse("0:0:").unwrap();
        assert_eq!(transactions.event(commit(5), 8, 12), Ok(None));
        assert_eq!(
            transactions.event(running(8, 6, vec![6, 7]), 16, 20),
            Ok(None)
        );
        assert_eq!(transactions.event(commit(6), 24, 28), Ok(None));
        // An abort, and a whole commit, aren't the load's to see.
        let abort = Event::Abort(ending(7, vec![]));
        assert_eq!(transactions.event(abort, 32, 36), Ok(None));
        transactions.hold(8, 40, vec![change(1)]);
        assert!(transactions.event(commit(8), 48, 52).unwrap().is_some());
        // 5 ended before 6, the oldest running then: every snapshot sees it.
        assert_eq!(transactions.unseen_dropped(&blind), [6]);
        let before = Snapshot::parse("6:9:6").unwrap();
        assert_eq!(transactions.unseen_dropped(&before), [6]);
        let after = Snapshot::parse("9:9:").unwrap();
        assert_eq!(transactions.unseen_dropped(&after), Vec::<u32>::new());
        assert_eq!(transactions.event(running(9, 7, vec![]), 56, 60), Ok(None));
        assert_eq!(transactions.unseen_dropped(&blind), Vec::<u32>::new());
        assert_eq!(transactions.event(commit(6), 64, 68), Ok(None));
        assert_eq!(transactions.unseen_dropped(&blind), [6]);
        transactions.loaded();
        assert_eq!(transactions.event(commit(6), 72, 76), Ok(None));
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
        let snapshot = Snapshot::parse("4294967290:4294967300:4294967295,4294967298").unwrap();
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
        let empty = Snapshot::parse("750:750:").unwrap();
        assert!(empty.sees(749) && !empty.sees(750) && !empty.sees(751));
        let early = Snapshot::parse("3:5:").unwrap();
        assert!(early.sees(4) && !early.sees(u32::MAX));
        for bad in [
            "", "1", "1:2", "2:1:", "1:2:x", "1:2:3:4", "a:2:", "1:b:", "1:2:3,",
        ] {
            assert_eq!(Snapshot::parse(bad), None, "{bad}");
        }
    }
}
