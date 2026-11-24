//! What the stream's thread reports, worked out apart from the socket.
//! The thread hands `next_report` what it has queued and read and what
//! the writer has applied; it sends the server the position that comes
//! back, and keeps the applied position for the lag. The server may then
//! recycle everything read, since the slot is never streamed again.

use straum_app::Lsn;

/// What the stream's thread knows when it reports.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Progress {
    /// The last commit the writer has applied.
    pub(crate) applied: Lsn,
    /// The last commit queued.
    pub(crate) sent: Lsn,
    /// How far the stream is read outside a transaction: the last
    /// keepalive between transactions on the logical stream, the WAL
    /// replayed on the physical one.
    pub(crate) idle: Lsn,
    /// The stream holds part of a transaction it hasn't queued.
    pub(crate) in_transaction: bool,
    /// The tables are loading: the writer applies nothing, and a
    /// physical stream may hold commits read before the load's snapshot
    /// that it hasn't queued.
    pub(crate) loading: bool,
}

/// What a report told the server, and what it knew applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Reported {
    /// The position sent. A physical slot moves its restart point to
    /// whatever is reported, back too, so no report goes below the last.
    pub(crate) position: Lsn,
    /// Every commit up to here is applied: the lag counts from it.
    pub(crate) applied: Lsn,
}

impl Reported {
    /// Nothing told yet, everything before `start` read and applied.
    pub(crate) fn at(start: Lsn) -> Reported {
        Reported {
            position: start,
            applied: start,
        }
    }
}

/// The report after `last`, from `progress`. It names everything read:
/// the commits queued, held in memory until the writer takes them, and
/// what the stream read past them. The slot is never streamed again, so
/// the server needs none of it. What is applied is the last commit
/// applied, or with every queued commit applied and no transaction open,
/// everything read, once the tables are loaded. Neither position falls.
pub(crate) fn next_report(last: Reported, progress: Progress) -> Reported {
    let Progress {
        applied,
        sent,
        idle,
        in_transaction,
        loading,
    } = progress;
    let done = if applied >= sent && !in_transaction && !loading {
        applied.max(idle)
    } else {
        applied
    };
    Reported {
        position: last.position.max(sent.max(idle)),
        applied: last.applied.max(done),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::mem;

    /// splitmix64, as `straum-app`'s lag tests use: a unit test can't
    /// reach another crate's test code.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, bound: u64) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut bits = self.0;
            bits = (bits ^ (bits >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            bits = (bits ^ (bits >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            (bits ^ (bits >> 31)) % bound
        }
    }

    /// Which stream the reference plays.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Kind {
        Logical,
        Physical,
    }

    /// One thing that happens to a stream between reports.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Event {
        Begin,
        Commit,
        Keepalive,
        /// The physical stream takes the load's snapshot and queues the
        /// commits it held.
        See,
        /// The writer asks for its first commit.
        Loaded,
        Apply,
        Report,
    }

    /// The reference: what a stream's thread has read and queued, what
    /// the writer has applied, and the last report, kept by hand.
    #[derive(Debug)]
    struct Model {
        kind: Kind,
        /// The server's position.
        server: Lsn,
        /// What a report may name: commits queued, and the keepalives
        /// between transactions on the logical stream or all WAL read on
        /// the physical one.
        read: Lsn,
        /// What the thread hands the rule beside the commits: the last
        /// keepalive between transactions, or the WAL read.
        idle: Lsn,
        sent: Lsn,
        /// A transaction has begun and not committed.
        open: bool,
        /// Commits queued and not applied yet, oldest first.
        waiting: VecDeque<Lsn>,
        /// A physical stream's commits read before the load's snapshot is
        /// known, not queued yet.
        held: VecDeque<Lsn>,
        /// The physical stream has the load's snapshot; a logical one
        /// never waits for it.
        seen: bool,
        loading: bool,
        applied: Lsn,
        reported: Reported,
    }

    impl Model {
        fn new(kind: Kind) -> Model {
            let start = Lsn(0x100);
            Model {
                kind,
                server: start,
                read: start,
                idle: start,
                sent: start,
                open: false,
                waiting: VecDeque::new(),
                held: VecDeque::new(),
                seen: kind == Kind::Logical,
                loading: true,
                applied: start,
                reported: Reported::at(start),
            }
        }

        fn step(&mut self, random: &mut Rng) -> Event {
            let event = match random.below(7) {
                0 => Event::Begin,
                1 => Event::Commit,
                2 => Event::Keepalive,
                3 => Event::See,
                4 => Event::Loaded,
                5 => Event::Apply,
                _ => Event::Report,
            };
            match event {
                Event::Begin if !self.open => {
                    self.open = true;
                    self.advance(random);
                }
                Event::Commit => {
                    self.advance(random);
                    self.open = false;
                    self.read = self.read.max(self.server);
                    if self.seen {
                        self.sent = self.server;
                        self.waiting.push_back(self.server);
                    } else {
                        self.held.push_back(self.server);
                    }
                }
                Event::Keepalive => self.keepalive(random),
                Event::See if !self.seen => {
                    self.seen = true;
                    for commit in mem::take(&mut self.held) {
                        self.sent = commit;
                        self.waiting.push_back(commit);
                    }
                }
                Event::Loaded if self.seen => self.loading = false,
                // Each once, in its order; nothing applies while loading.
                Event::Begin | Event::See | Event::Loaded => {}
                Event::Apply if self.loading => {}
                Event::Apply => {
                    if let Some(commit) = self.waiting.pop_front() {
                        self.applied = commit;
                    }
                }
                Event::Report => self.reported = next_report(self.reported, self.progress()),
            }
            event
        }

        /// The server writes on; the physical stream reads all of it.
        fn advance(&mut self, random: &mut Rng) {
            self.server = Lsn(self.server.0 + 1 + random.below(16));
            if self.kind == Kind::Physical {
                self.idle = self.server;
                self.read = self.server;
            }
        }

        /// A keepalive, the server idle or not. The logical stream counts
        /// only one between transactions.
        fn keepalive(&mut self, random: &mut Rng) {
            if random.below(2) == 0 {
                self.advance(random);
            }
            if self.kind == Kind::Logical && !self.open {
                self.idle = self.idle.max(self.server);
                self.read = self.read.max(self.server);
            }
        }

        fn in_transaction(&self) -> bool {
            self.kind == Kind::Logical && self.open
        }

        fn progress(&self) -> Progress {
            Progress {
                applied: self.applied,
                sent: self.sent,
                idle: self.idle,
                in_transaction: self.in_transaction(),
                loading: self.loading,
            }
        }

        /// What holds after every event, `before` the last report until it.
        fn check(&self, before: Reported, event: Event, context: &str) {
            let now = self.reported;
            assert!(now.position >= before.position, "report fell: {context}");
            assert!(now.applied >= before.applied, "applied fell: {context}");
            assert!(now.position <= self.read, "past what is read: {context}");
            assert!(now.applied <= now.position, "applied past: {context}");
            for first in [self.waiting.front(), self.held.front()]
                .into_iter()
                .flatten()
            {
                assert!(now.applied < *first, "unapplied counted: {context}");
            }
            if event != Event::Report {
                return;
            }
            assert!(now.position >= self.read, "behind what is read: {context}");
            assert!(now.applied >= self.applied, "behind applied: {context}");
            if self.waiting.is_empty() && !self.in_transaction() && !self.loading {
                assert!(now.applied >= self.idle, "not caught up: {context}");
            }
        }
    }

    /// The report names everything read, never less than before, and the
    /// lag counts from what is applied, against a reference kept by hand.
    #[test]
    fn reports_what_is_read_and_counts_the_lag_from_what_is_applied() {
        for seed in 0..500 {
            for kind in [Kind::Logical, Kind::Physical] {
                let mut random = Rng(seed);
                let mut model = Model::new(kind);
                for step in 0..200 {
                    let before = model.reported;
                    let event = model.step(&mut random);
                    let context = format!("{kind:?} seed {seed} step {step} {event:?}: {model:?}");
                    model.check(before, event, &context);
                }
            }
        }
    }
}
