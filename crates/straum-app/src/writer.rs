//! The write loop: `Writer` keeps an `App`'s store in step with the
//! `Source`. It loads every table, then applies one commit at a time; it
//! takes the store's lock only to apply what it has read, never while it
//! waits on PostgreSQL, so asks never wait on the database. Its readings
//! of the server's WAL go behind the progress's lock, so they never wait
//! on an ask.

use std::time::{Duration, Instant};
use std::{fmt, slice};

use straum_domain::{Identifier, RowError, Spec, Store};

use crate::app::{App, Following, Progress, State};
use crate::lag::Lag;
use crate::source::{Change, Commit, Drift, Lsn, Source, SourceError, Wal};

const STARTED: &str = "the writer applies commits only to a started stream";

const FAILED: &str = "a writer that failed takes no more steps: start a new one";

/// How often the writer reads the server's WAL while it waits for commits.
/// The lag can look up to this much longer than it is.
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
    /// When the server's WAL was last read.
    wal_read_at: Instant,
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
    /// Applied the commit that ends here.
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
        let Commit { lsn, changes, .. } = commit;
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
    /// commit, waiting for one and reading the server's WAL meanwhile.
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
            self.app.progress().following = None;
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
        let start = self.source.open(&spec)?;
        let tables = spec.dims().len() + spec.facts().len();
        (self.report)(&format!(
            "opened a stream at {start}; loading {tables} tables"
        ));
        let mut store = Store::new(spec.clone());
        // Dims first, so a fact's rows find theirs when asked.
        for dim in spec.dims() {
            self.load(slice::from_ref(&dim.name), "dim", &mut store)?;
        }
        for views in spec.facts_by_table() {
            self.load(&views, "view", &mut store)?;
        }
        let wal = self.read_wal()?;
        let mut lag = Lag::new(opened);
        lag.read(&wal, self.wal_read_at);
        let mut state = self.app.write();
        state.store = store;
        state.applied = Some(start);
        let mut progress = self.app.progress();
        progress.following = Some(Following {
            applied: start,
            committed: None,
            lag,
            wal,
        });
        Ok(start)
    }

    /// Loads one dim, or the views of one fact table together, saying
    /// when it is done and how long it took.
    fn load(&mut self, names: &[Identifier], kind: &str, store: &mut Store) -> Result<(), Error> {
        let clock = Instant::now();
        self.source.load(names, store)?;
        let seconds = clock.elapsed().as_secs_f64();
        (self.report)(&format!("loaded {} in {seconds:.1} s", named(kind, names)));
        Ok(())
    }

    fn apply_next(&mut self) -> Result<Lsn, Error> {
        loop {
            if self.wal_read_at.elapsed() >= WAL_EVERY {
                self.take_wal_reading()?;
            }
            let wait = WAL_EVERY.saturating_sub(self.wal_read_at.elapsed());
            if let Some(commit) = self.source.next_commit(wait)? {
                let mut state = self.app.write();
                state.apply(&commit)?;
                let mut progress = self.app.progress();
                let following = progress.following();
                following.applied = commit.lsn;
                following.committed = Some(commit.committed);
                return Ok(commit.lsn);
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
        Ok(())
    }

    /// Reads the server's WAL, and fails once the slot retains more of it
    /// than [`Limits::max_retained_wal`](crate::Limits::max_retained_wal).
    fn read_wal(&mut self) -> Result<Wal, Error> {
        // Taken before the read, so the reading holds all WAL written by then.
        let at = Instant::now();
        let wal = self.source.wal()?;
        self.wal_read_at = at;
        let retained = wal.retained();
        let limit = self.app.limits.max_retained_wal;
        if retained > limit {
            return Err(Error::Retained { retained, limit });
        }
        Ok(wal)
    }
}

/// `view trips` for one name, `views trips, dropoffs` for several.
fn named(kind: &str, names: &[Identifier]) -> String {
    let mut listed = Vec::new();
    for name in names {
        listed.push(name.as_str());
    }
    let plural = if names.len() == 1 { "" } else { "s" };
    format!("{kind}{plural} {}", listed.join(", "))
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
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, SystemTime};

    use straum_domain::{Datum, Identifier, Spec, Store, Value};

    use super::{Change, Commit, Drift, Lsn, Step, Wal, Writer};
    use crate::app::AnswerError;
    use crate::app::tests::{WAITING, app, count, follow, id};
    use crate::source::{Source, SourceError};

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
        fn open(&mut self, _spec: &Spec) -> Result<Lsn, SourceError> {
            Ok(Lsn(1))
        }

        fn load(&mut self, _tables: &[Identifier], _store: &mut Store) -> Result<(), SourceError> {
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
        fn open(&mut self, _spec: &Spec) -> Result<Lsn, SourceError> {
            Err("connection refused".into())
        }

        fn load(&mut self, _tables: &[Identifier], _store: &mut Store) -> Result<(), SourceError> {
            Err("connection refused".into())
        }

        fn next_commit(&mut self, _wait: Duration) -> Result<Option<Commit>, SourceError> {
            Err("connection refused".into())
        }

        fn wal(&mut self) -> Result<Wal, SourceError> {
            Err("connection refused".into())
        }
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
