//! The write loop: `Writer` keeps an `App`'s store in step with the
//! `Source`. It loads every table, then applies the commits that the
//! source has queued, all under one lock of the store. It takes the lock
//! only to apply what it has read, never while it waits on PostgreSQL, so
//! asks never wait on the database. Its readings of the server's WAL go
//! behind the progress's lock, so they never wait on an ask. The store is
//! advanced to the snapshot's time before the load, and to each commit's
//! time before its changes, so a width kept only for a time drops what
//! falls behind it.

use std::fmt;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use straum_domain::{Identifier, RowError, Spec, Store};

use crate::app::{App, Following, Progress, State};
use crate::lag::Lag;
use crate::loading::{Load, Loading, loaded, loads, planned};
use crate::rate::Applying;
use crate::source::{Change, Commit, Drift, Lsn, Opened, Source, SourceError, Wal};

const STARTED: &str = "the writer applies commits only to a started stream";

const FAILED: &str = "a writer that failed takes no more steps: start a new one";

/// How long after a reading of the server's WAL ends the writer reads it
/// again. The lag can look up to this much longer than it is.
const WAL_EVERY: Duration = Duration::from_secs(1);

const BYTES_PER_MIB: u32 = 1 << 20;

/// Reads the source and keeps an [`App`] in step with it: the loop that
/// does the I/O. One writer per app, and one stream per writer: a writer
/// that failed takes no more steps.
// Generic over the source: the binary runs it on PostgreSQL, the tests on
// an in-memory fake.
pub struct Writer<S> {
    source: S,
    app: App,
    phase: Phase,
    /// When the server's WAL was last read: the time before the reading.
    wal_read_at: Instant,
    /// When the next reading of the WAL falls due: `WAL_EVERY` after the
    /// last reading ended, so a slow reading leaves time to apply. A batch
    /// ends later than it by the time that the writer waited for the lock.
    wal_due_at: Instant,
    /// Where the start says what it is doing, line by line.
    report: Report,
}

/// Where a writer is in its one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Nothing loaded yet: the next step loads every table.
    Loading,
    /// Every table loaded: the next step applies the next commit.
    Following,
    /// A step failed. Straum starts again in a new process, from a new
    /// stream, so this writer never loads again.
    Failed,
}

/// Takes one line saying what the writer is doing: the binary logs it.
pub type Report = fn(&str);

/// Says nothing.
fn report_nothing(_line: &str) {}

/// What one [`Writer::step`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Loaded every table from a new stream that starts here.
    Started(Lsn),
    /// Applied the commits queued, the last of which ends here.
    Applied(Lsn),
}

impl Progress {
    fn following(&mut self) -> &mut Following {
        self.following.as_mut().expect(STARTED)
    }
}

impl State {
    /// Applies the commit to every fact and dim, or names the first table
    /// it no longer fits, and why.
    fn apply(&mut self, commit: &Commit) -> Result<(), Error> {
        let Commit {
            lsn,
            changes,
            committed,
        } = commit;
        // Before the changes, so a row older than the horizon makes no
        // narrow cell that the next advance would drop.
        self.store.advance(unix_seconds(*committed));
        for change in changes {
            let table = change.table();
            let spec = self.store.spec();
            // The publication may carry tables the spec doesn't name.
            if spec.fact(table.as_str()).is_none() && spec.dim(table.as_str()).is_none() {
                continue;
            }
            if let Err(why) = apply_change(&mut self.store, change) {
                // The earlier changes of this commit are in the store.
                // Clear the stream before the write guard drops, so an ask
                // waiting on it cannot read them.
                self.applied = None;
                return Err(Error::Unfit {
                    table: table.clone(),
                    why,
                });
            }
        }
        self.applied = Some(*lsn);
        Ok(())
    }
}

/// `at` in Unix seconds, rounded down, so a time before 1970 falls in the
/// second it is in; a time past `i64` stops at its end.
fn unix_seconds(at: SystemTime) -> i64 {
    match at.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        Err(before) => {
            let before = before.duration();
            let whole = i64::try_from(before.as_secs()).unwrap_or(i64::MAX);
            if before.subsec_nanos() == 0 {
                -whole
            } else {
                -whole - 1
            }
        }
    }
}

/// Applies one change, or says why it no longer fits. Facts and dims need
/// the whole old row: a fact to take it out of its cell, a dim because a
/// key change is the old row out and the new one in.
fn apply_change(store: &mut Store, change: &Change) -> Result<(), Unfitting> {
    match change {
        Change::Insert { table, row } => store.add_row(table.as_str(), row),
        Change::Update { table, old, new } => store.update_row(table.as_str(), old, new),
        Change::Delete { table, old } => store.remove_row(table.as_str(), old),
        Change::Truncate { table } => {
            store.clear(table.as_str());
            return Ok(());
        }
        Change::Unfit { drift, .. } => return Err(Unfitting::Drifted(*drift)),
    }
    .map_err(Unfitting::Row)
}

impl<S: Source> Writer<S> {
    /// A writer that has loaded nothing yet; its first step loads.
    pub fn new(app: App, source: S) -> Writer<S> {
        Writer {
            source,
            app,
            phase: Phase::Loading,
            wal_read_at: Instant::now(),
            wal_due_at: Instant::now(),
            report: report_nothing,
        }
    }

    /// The writer, saying through `report` what each start does: the
    /// stream it opened and each table as it loads.
    #[must_use]
    pub fn reporting(self, report: Report) -> Writer<S> {
        Writer { report, ..self }
    }

    /// The app this writer keeps in step, for asks.
    pub fn app(&self) -> &App {
        &self.app
    }

    /// Loads every table when nothing is loaded yet, else applies the next
    /// commit, waiting for one and reading the server's WAL meanwhile. Under
    /// the same lock, it applies each commit queued after it, until the
    /// queue is empty or a reading of the WAL is due. The wait for the lock
    /// does not count, so a long ask delays the reading and not the batch.
    /// After an error Straum is not ready and the writer is done: Straum
    /// starts again with a new app and writer, from a new stream.
    ///
    /// # Panics
    ///
    /// On a step after one that failed.
    pub fn step(&mut self) -> Result<Step, Error> {
        let step = match self.phase {
            Phase::Loading => self.start().map(Step::Started),
            Phase::Following => self.apply_next().map(Step::Applied),
            Phase::Failed => panic!("{FAILED}"),
        };
        if step.is_err() {
            self.phase = Phase::Failed;
            // No state of a failed run is answered from.
            self.app.write().applied = None;
            let mut progress = self.app.progress();
            progress.following = None;
            progress.loading = None;
        } else {
            self.phase = Phase::Following;
        }
        step
    }

    /// Loads every dim and fact into a new store outside the lock, then
    /// swaps it in.
    fn start(&mut self) -> Result<Lsn, Error> {
        let spec = Spec::clone(&self.app.spec);
        // WAL the server writes from here on waits for the load.
        let opened = Instant::now();
        let Opened { start, at } = self.source.open(&spec)?;
        let loads = loads(&spec);
        let mut estimates = Vec::new();
        for load in &loads {
            estimates.push(self.source.estimate(&load.names)?);
        }
        let plan = planned(&loads, &estimates);
        (self.report)(&format!("opened a stream at {start}; {plan}"));
        let mut store = Store::new(spec.clone());
        // Before the load, so no row older than the horizon makes a
        // narrow cell, and the load holds only what the store keeps.
        store.advance(unix_seconds(at));
        for (index, (load, estimate)) in loads.iter().zip(estimates).enumerate() {
            let loading = Loading {
                table: load.table.clone(),
                position: index + 1,
                tables: loads.len(),
                estimate,
                rows_read: 0,
                held_bytes: 0,
                max_stream_memory: self.app.limits.max_stream_memory,
            };
            self.load(load, loading, &mut store)?;
        }
        let wal = self.read_wal()?;
        let mut lag = Lag::new(opened);
        lag.read(&wal, self.wal_read_at);
        let mut state = self.app.write();
        state.store = store;
        state.applied = Some(start);
        let mut progress = self.app.progress();
        progress.loading = None;
        progress.following = Some(Following {
            applied: start,
            committed: None,
            lag,
            wal,
            applying: Applying::starting(self.wal_read_at),
        });
        Ok(start)
    }

    /// Loads one dim, or the views of one fact table together, showing
    /// how far it is in the status meanwhile, then saying how long it took
    /// and how many rows it read.
    fn load(&mut self, load: &Load, loading: Loading, store: &mut Store) -> Result<(), Error> {
        let counts = &self.app.counts;
        counts.rows.store(0, Ordering::Relaxed);
        let bytes = loading.estimate.bytes;
        self.app.progress().loading = Some(loading);
        let clock = Instant::now();
        self.source.load(&load.names, store, counts)?;
        let seconds = clock.elapsed().as_secs_f64();
        let rows = counts.rows.load(Ordering::Relaxed);
        (self.report)(&loaded(load, rows, bytes, seconds));
        Ok(())
    }

    fn apply_next(&mut self) -> Result<Lsn, Error> {
        loop {
            if Instant::now() >= self.wal_due_at {
                self.take_wal_reading()?;
            }
            let wait = self.wal_due_at.saturating_duration_since(Instant::now());
            if let Some(mut commit) = self.source.next_commit(wait)? {
                // One wait for the lock applies every commit queued, so a
                // long ask delays a batch and not each commit.
                let asked = Instant::now();
                let mut state = self.app.write();
                // The wait for the lock is the time of an ask, not of the batch.
                let due_at = self.wal_due_at + asked.elapsed();
                loop {
                    state.apply(&commit)?;
                    let mut progress = self.app.progress();
                    let following = progress.following();
                    following.applied = commit.lsn;
                    following.committed = Some(commit.committed);
                    following.applying.count(commit.changes.len());
                    drop(progress);
                    // The reading of the WAL never runs under the lock.
                    if Instant::now() >= due_at {
                        return Ok(commit.lsn);
                    }
                    match self.source.next_commit(Duration::ZERO)? {
                        Some(next) => commit = next,
                        None => return Ok(commit.lsn),
                    }
                }
            }
        }
    }

    /// Reads the server's WAL into the lag, without the store's lock.
    fn take_wal_reading(&mut self) -> Result<(), Error> {
        let wal = self.read_wal()?;
        let mut progress = self.app.progress();
        let following = progress.following();
        following.lag.read(&wal, self.wal_read_at);
        following.wal = wal;
        following.applying.close(self.wal_read_at);
        Ok(())
    }

    /// Reads the server's WAL, and fails once the slot retains more of it
    /// than [`Limits::max_retained_wal`](crate::Limits::max_retained_wal).
    fn read_wal(&mut self) -> Result<Wal, Error> {
        // Taken before the read, so the reading holds all WAL written by then.
        let at = Instant::now();
        let wal = self.source.wal()?;
        self.wal_read_at = at;
        self.wal_due_at = Instant::now() + WAL_EVERY;
        let retained = wal.retained();
        let limit = self.app.limits.max_retained_wal;
        if retained > limit {
            return Err(Error::Retained { retained, limit });
        }
        Ok(wal)
    }
}

/// Why a [`Writer::step`] failed; the writer takes no more steps.
#[derive(Debug)]
pub enum Error {
    Source(SourceError),
    /// A change no longer fits the table: a row that doesn't fit, a change
    /// without the old row, a dim row Straum never held, an altered table.
    Unfit {
        table: Identifier,
        why: Unfitting,
    },
    /// The server holds more WAL for Straum's slot than
    /// [`Limits::max_retained_wal`](crate::Limits::max_retained_wal), in
    /// bytes.
    Retained {
        retained: u64,
        limit: u64,
    },
}

/// Why a change no longer fits its table.
#[derive(Debug)]
pub enum Unfitting {
    /// The store turned the row away.
    Row(RowError),
    /// The table no longer reads as Straum holds it.
    Drifted(Drift),
}

impl fmt::Display for Unfitting {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unfitting::Row(error) => error.fmt(formatter),
            Unfitting::Drifted(drift) => drift.fmt(formatter),
        }
    }
}

impl From<SourceError> for Error {
    fn from(error: SourceError) -> Error {
        Error::Source(error)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Source(error) => write!(formatter, "{error}"),
            Error::Unfit { table, why } => {
                write!(formatter, "a change no longer fits {table}: {why}")
            }
            Error::Retained { retained, limit } => write!(
                formatter,
                "PostgreSQL holds {} of WAL for Straum's slot, past the limit of {}",
                format_mebibytes(*retained),
                format_mebibytes(*limit)
            ),
        }
    }
}

impl std::error::Error for Error {}

/// Bytes as mebibytes, for messages.
#[expect(
    clippy::cast_precision_loss,
    reason = "f64 is exact up to 2^53 bytes, and the message shows one decimal"
)]
pub fn format_mebibytes(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / f64::from(BYTES_PER_MIB))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::mem;
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, SystemTime};

    use straum_domain::{Datum, Identifier, Spec, Store, Value};

    use super::{Change, Commit, Drift, Lsn, Step, WAL_EVERY, Wal, Writer, unix_seconds};
    use crate::app::AnswerError;
    use crate::app::tests::{WAITING, app, count, follow, id};
    use crate::source::{Estimate, LoadCounts, Opened, Source, SourceError};

    /// A time rounds down to its second on either side of 1970, so a
    /// horizon worked out from it lands where the server's clock puts it.
    #[test]
    fn a_time_is_its_unix_second_rounded_down() {
        let epoch = SystemTime::UNIX_EPOCH;
        let half = Duration::from_millis(500);
        assert_eq!(unix_seconds(epoch), 0);
        assert_eq!(unix_seconds(epoch + Duration::from_secs(90) + half), 90);
        assert_eq!(unix_seconds(epoch - Duration::from_secs(90)), -90);
        assert_eq!(unix_seconds(epoch - Duration::from_secs(90) - half), -91);
    }

    #[test]
    fn a_failed_apply_hides_the_commit_before_the_lock_drops() {
        let app = app();
        follow(&app);
        let mut state = app.write();
        let applied = state.apply(&Commit {
            lsn: Lsn(2),
            committed: SystemTime::UNIX_EPOCH,
            changes: vec![
                Change::Insert {
                    table: id("trips"),
                    row: vec![Datum::Int(0)],
                },
                Change::Unfit {
                    table: id("trips"),
                    drift: Drift::Altered,
                },
            ],
        });
        assert!(applied.is_err());
        assert!(state.applied.is_none());
        // The insert landed. Nothing can answer it: the stream is already clear.
        assert!(state.store.cells("trips").unwrap() > 0);
        drop(state);
        assert!(matches!(app.ask(&count()), Err(AnswerError::NotReady)));
    }

    #[test]
    fn an_unfit_table_says_why() {
        for (drift, words) in [
            (Drift::Altered, "the table was altered or rewritten"),
            (
                Drift::Renamed,
                "a value of a column Straum reads was renamed",
            ),
            (
                Drift::OutOfLine,
                "a value of a column Straum reads is stored out of line",
            ),
        ] {
            let app = app();
            follow(&app);
            let error = app
                .write()
                .apply(&Commit {
                    lsn: Lsn(2),
                    committed: SystemTime::UNIX_EPOCH,
                    changes: vec![Change::Unfit {
                        table: id("trips"),
                        drift,
                    }],
                })
                .unwrap_err()
                .to_string();
            assert!(
                error.starts_with("a change no longer fits trips: "),
                "{error}"
            );
            assert!(error.contains(words), "{error}");
            if drift == Drift::OutOfLine {
                assert!(!error.contains("altered"), "{error}");
            }
        }
    }

    #[test]
    fn a_commit_that_fits_stays_readable() {
        let app = app();
        follow(&app);
        {
            let mut state = app.write();
            state
                .apply(&Commit {
                    lsn: Lsn(2),
                    committed: SystemTime::UNIX_EPOCH,
                    changes: vec![Change::Insert {
                        table: id("trips"),
                        row: vec![Datum::Int(0)],
                    }],
                })
                .unwrap();
            assert_eq!(state.applied, Some(Lsn(2)));
        }
        let answered = app.ask(&count()).unwrap();
        assert_eq!(answered.applied, Lsn(2));
        assert_eq!(answered.answer.rows[0].values[0], Value::Int(1));
    }

    /// A source whose server stands still: the WAL it reads never moves,
    /// and each commit it sends is empty.
    struct StandingStill;

    impl Source for StandingStill {
        fn open(&mut self, _spec: &Spec) -> Result<Opened, SourceError> {
            Ok(Opened {
                start: Lsn(1),
                at: SystemTime::UNIX_EPOCH,
            })
        }

        fn estimate(&mut self, _tables: &[Identifier]) -> Result<Estimate, SourceError> {
            Ok(Estimate {
                rows: Some(0),
                bytes: 0,
            })
        }

        fn load(
            &mut self,
            _tables: &[Identifier],
            _store: &mut Store,
            _counts: &LoadCounts,
        ) -> Result<(), SourceError> {
            Ok(())
        }

        fn next_commit(&mut self, _wait: Duration) -> Result<Option<Commit>, SourceError> {
            Ok(Some(Commit {
                lsn: Lsn(2),
                committed: SystemTime::UNIX_EPOCH,
                changes: Vec::new(),
            }))
        }

        fn wal(&mut self) -> Result<Wal, SourceError> {
            Ok(Wal {
                written: Lsn(1),
                applied: Lsn(1),
                restart: Lsn(1),
            })
        }
    }

    /// A source whose server never answers.
    struct Unreachable;

    impl Source for Unreachable {
        fn open(&mut self, _spec: &Spec) -> Result<Opened, SourceError> {
            Err("connection refused".into())
        }

        fn estimate(&mut self, _tables: &[Identifier]) -> Result<Estimate, SourceError> {
            Err("connection refused".into())
        }

        fn load(
            &mut self,
            _tables: &[Identifier],
            _store: &mut Store,
            _counts: &LoadCounts,
        ) -> Result<(), SourceError> {
            Err("connection refused".into())
        }

        fn next_commit(&mut self, _wait: Duration) -> Result<Option<Commit>, SourceError> {
            Err("connection refused".into())
        }

        fn wal(&mut self) -> Result<Wal, SourceError> {
            Err("connection refused".into())
        }
    }

    /// A source with a queue of commits, each of one insert into `trips`.
    /// It counts the readings of the WAL, and keeps the wait of each ask
    /// for a commit. An ask that finds the queue empty returns at once.
    struct Queued {
        commits: VecDeque<Commit>,
        /// Asks that find no commit before the queue hands one out.
        empty: usize,
        /// How long the next ask with a wait takes before its commit.
        pause: Duration,
        asked: Vec<Duration>,
        readings: usize,
        /// How long each reading of the WAL takes.
        reading: Duration,
    }

    impl Queued {
        /// `count` commits at LSN 2 and on, each one second after the last.
        fn of(count: u64) -> Queued {
            let commits = (2..count + 2)
                .map(|lsn| Commit {
                    lsn: Lsn(lsn),
                    committed: SystemTime::UNIX_EPOCH + Duration::from_secs(lsn),
                    changes: vec![Change::Insert {
                        table: id("trips"),
                        row: vec![Datum::Int(0)],
                    }],
                })
                .collect();
            Queued {
                commits,
                empty: 0,
                pause: Duration::ZERO,
                asked: Vec::new(),
                readings: 0,
                reading: Duration::ZERO,
            }
        }
    }

    impl Source for Queued {
        fn open(&mut self, spec: &Spec) -> Result<Opened, SourceError> {
            StandingStill.open(spec)
        }

        fn estimate(&mut self, tables: &[Identifier]) -> Result<Estimate, SourceError> {
            StandingStill.estimate(tables)
        }

        fn load(
            &mut self,
            tables: &[Identifier],
            store: &mut Store,
            counts: &LoadCounts,
        ) -> Result<(), SourceError> {
            StandingStill.load(tables, store, counts)
        }

        fn next_commit(&mut self, wait: Duration) -> Result<Option<Commit>, SourceError> {
            self.asked.push(wait);
            if self.empty > 0 {
                self.empty -= 1;
                return Ok(None);
            }
            if !wait.is_zero() {
                thread::sleep(mem::take(&mut self.pause));
            }
            Ok(self.commits.pop_front())
        }

        fn wal(&mut self) -> Result<Wal, SourceError> {
            self.readings += 1;
            thread::sleep(self.reading);
            StandingStill.wal()
        }
    }

    /// The writer applies every queued commit after one wait for the
    /// store's lock. Thus a long ask delays a batch, and not each commit.
    #[test]
    fn one_wait_for_the_lock_applies_every_queued_commit() {
        const QUEUED: u64 = 50;
        let app = app();
        let mut writer = Writer::new(app.clone(), Queued::of(QUEUED));
        assert_eq!(writer.step().unwrap(), Step::Started(Lsn(1)));
        let (held, holding) = mpsc::channel();
        let asker = {
            let app = app.clone();
            thread::spawn(move || {
                let guard = app.read();
                held.send(()).unwrap();
                thread::sleep(WAITING / 5);
                drop(guard);
            })
        };
        holding.recv().unwrap();
        let last = Lsn(QUEUED + 1);
        assert_eq!(writer.step().unwrap(), Step::Applied(last));
        asker.join().unwrap();

        let status = app.status();
        assert_eq!(status.applied, Some(last));
        assert_eq!(
            status.committed,
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(last.0))
        );
        assert_eq!(app.ask(&count()).unwrap().applied, last);
        // The interval began at the last reading. Closed one second after
        // that, its counts are its rates.
        let mut progress = app.progress();
        let applying = &mut progress.following.as_mut().unwrap().applying;
        applying.close(writer.wal_read_at + Duration::from_secs(1));
        let rate = applying.rate.unwrap();
        assert_eq!(rate.commits_per_second, QUEUED);
        assert_eq!(rate.changes_per_second, QUEUED);
    }

    /// With no commit queued, the writer waits for one up to the next
    /// reading of the WAL. It asks with a zero wait only after a commit.
    #[test]
    fn an_empty_queue_is_waited_for() {
        let mut source = Queued::of(1);
        source.empty = 1;
        let mut writer = Writer::new(app(), source);
        writer.step().unwrap();
        assert_eq!(writer.step().unwrap(), Step::Applied(Lsn(2)));
        let asked = &writer.source.asked;
        assert!(asked.len() <= 3, "{asked:?}");
        for wait in &asked[..2] {
            assert!(*wait > WAL_EVERY / 2, "{asked:?}");
        }
    }

    /// A reading of the WAL that falls due ends a batch: the reading
    /// never runs under the store's lock, so a long batch delays it at
    /// most one commit.
    #[test]
    fn a_due_reading_of_the_wal_ends_a_batch() {
        let mut source = Queued::of(3);
        source.pause = WAL_EVERY;
        let mut writer = Writer::new(app(), source);
        writer.step().unwrap();
        let readings = writer.source.readings;
        assert_eq!(writer.step().unwrap(), Step::Applied(Lsn(2)));
        assert_eq!(writer.source.readings, readings);
        assert_eq!(writer.step().unwrap(), Step::Applied(Lsn(4)));
        assert_eq!(writer.source.readings, readings + 1);
    }

    /// An ask that holds the store's lock past the due time of the WAL
    /// reading does not cut the batch. The wait for the lock is not time
    /// that the writer applies in. Thus a long ask delays a batch, and
    /// not each commit (ADR-0002).
    #[test]
    fn a_long_ask_delays_a_batch_and_not_each_commit() {
        const QUEUED: u64 = 50;
        let app = app();
        let mut writer = Writer::new(app.clone(), Queued::of(QUEUED));
        writer.step().unwrap();
        let (held, holding) = mpsc::channel();
        let asker = {
            let app = app.clone();
            thread::spawn(move || {
                let guard = app.read();
                held.send(()).unwrap();
                thread::sleep(WAL_EVERY + WAL_EVERY / 5);
                drop(guard);
            })
        };
        holding.recv().unwrap();
        assert_eq!(writer.step().unwrap(), Step::Applied(Lsn(QUEUED + 1)));
        asker.join().unwrap();
    }

    /// The next reading of the WAL falls due a second after the last one
    /// ends, not after it starts. Thus a reading slower than a second
    /// leaves time to apply a batch, and not only one commit.
    #[test]
    fn a_slow_reading_of_the_wal_leaves_time_for_a_batch() {
        const QUEUED: u64 = 5;
        let mut source = Queued::of(QUEUED);
        source.reading = WAL_EVERY + WAL_EVERY / 5;
        let mut writer = Writer::new(app(), source);
        writer.step().unwrap();
        let readings = writer.source.readings;
        assert_eq!(writer.step().unwrap(), Step::Applied(Lsn(QUEUED + 1)));
        assert_eq!(writer.source.readings, readings);
    }

    /// A writer that steps clean steps on; one that failed is done, and
    /// leaves its app not ready.
    #[test]
    fn a_failed_writer_leaves_its_app_not_ready() {
        let mut writer = Writer::new(app(), StandingStill);
        assert_eq!(writer.step().unwrap(), Step::Started(Lsn(1)));
        assert_eq!(writer.step().unwrap(), Step::Applied(Lsn(2)));
        assert_eq!(writer.step().unwrap(), Step::Applied(Lsn(2)));

        let mut failed = Writer::new(app(), Unreachable);
        assert_eq!(failed.step().unwrap_err().to_string(), "connection refused");
        assert!(!failed.app().status().ready);
        assert!(matches!(
            failed.app().ask(&count()),
            Err(AnswerError::NotReady)
        ));
    }

    #[test]
    #[should_panic(expected = "a writer that failed takes no more steps")]
    fn a_failed_writer_takes_no_more_steps() {
        let mut writer = Writer::new(app(), Unreachable);
        writer.step().unwrap_err();
        let _ = writer.step();
    }

    /// The writer's reading of the WAL and the status don't wait on the
    /// store's lock: a long ask holds it, and a writer that waited for it
    /// would hold back every ask after.
    #[test]
    fn a_reading_of_the_wal_and_the_status_wait_on_no_ask() {
        let app = app();
        let mut writer = Writer::new(app.clone(), StandingStill);
        assert_eq!(writer.step().unwrap(), Step::Started(Lsn(1)));
        let held = app.write();
        let (done, finished) = mpsc::channel();
        let reader = thread::spawn(move || {
            writer.take_wal_reading().unwrap();
            done.send(writer.app().status().ready).unwrap();
            writer
        });
        assert_eq!(finished.recv_timeout(WAITING), Ok(true));
        drop(held);
        let mut writer = reader.join().unwrap();
        assert_eq!(writer.step().unwrap(), Step::Applied(Lsn(2)));
        assert_eq!(app.status().applied, Some(Lsn(2)));
    }
}
