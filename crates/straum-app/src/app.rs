//! A store kept in step with a source by one writer, and read by asks while
//! it writes. The writer holds the lock only to apply what it has read,
//! never while it waits on the source, so asks never wait on PostgreSQL.

use std::fmt;
use std::mem;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant, SystemTime};

use straum_domain::{Answer, Ask, AskError, Ident, RowError, Spec, Store};

use crate::lag::Lag;
use crate::source::{Change, Commit, Lsn, Source, SourceError, Wal};

/// Panic message for facts taken from the spec.
const IN_SPEC: &str = "every applied fact is in the spec";

/// Panic message for a poisoned lock. The store may be half updated, so
/// nothing reads it again.
const POISONED: &str = "the writer panicked while it held the store";

/// Panic message for the lag of a started stream.
const STARTED: &str = "a started stream has a lag";

/// How often the writer reads the server's WAL while it waits for commits.
/// The lag can look up to this much longer than it is.
const MEASURE_EVERY: Duration = Duration::from_secs(1);

/// How far behind the server straum may fall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Past this, straum is not ready, though asks still answer.
    pub max_lag: Duration,
    /// Bytes of WAL the server may keep for straum's slot. Past this,
    /// straum starts over, so a straum that can't keep up doesn't fill the
    /// server's disk.
    pub max_retained_wal: u64,
}

/// The store as the writer last left it, for asks. Clones share it.
#[derive(Debug, Clone)]
pub struct App {
    state: Arc<RwLock<State>>,
}

#[derive(Debug)]
struct State {
    store: Store,
    /// Every commit up to here is applied. `None` until every table is
    /// loaded, and again after a failure.
    position: Option<Lsn>,
    /// When the commit at `position` committed. `None` when `position` is
    /// where a stream started.
    time: Option<SystemTime>,
    /// How far behind the server the stream is. Set with `position`.
    lag: Option<Lag>,
    /// The last reading of the server's WAL. Set with `position`.
    wal: Option<Wal>,
    limits: Limits,
    /// The last failure, until the next start.
    error: Option<String>,
    /// The last failure and when it came, kept across starts.
    last_failure: Option<(SystemTime, String)>,
    starts: u64,
    failures: u64,
}

/// Reads the source and keeps an [`App`] in step with it: the loop that
/// does the I/O. One writer per app.
#[derive(Debug)]
pub struct Writer<S> {
    spec: Spec,
    limits: Limits,
    source: S,
    app: App,
    /// When the server's WAL was last read.
    measured: Instant,
}

/// What [`Writer::step`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Loaded every table from a new stream that starts here.
    Started(Lsn),
    /// Applied the commit that ends here.
    Applied(Lsn),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answered {
    pub answer: Answer,
    /// The answer reflects every commit up to here and none after.
    pub lsn: Lsn,
    /// Every commit made before this, by straum's clock, is in the answer:
    /// when the slot was last known to have confirmed all the WAL there
    /// was. Unlike the time of the commit at `lsn`, it moves on while the
    /// database is idle.
    pub as_of: SystemTime,
}

/// What [`App::status`] reports, read at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// As [`App::ready`].
    pub ready: bool,
    /// Every commit up to here is applied. `None` until every table is
    /// loaded, and again after a failure.
    pub lsn: Option<Lsn>,
    /// When the commit at `lsn` committed. `None` when `lsn` is where the
    /// stream started.
    pub time: Option<SystemTime>,
    /// As [`Answered::as_of`]. `None` until every table is loaded.
    pub as_of: Option<SystemTime>,
    /// As [`App::lag`].
    pub lag: Option<Duration>,
    /// The last reading of the server's WAL, while a stream runs.
    pub wal: Option<Wal>,
    pub limits: Limits,
    /// The last failure, until the next start.
    pub error: Option<String>,
    /// The last failure and when it came, kept across starts.
    pub last_failure: Option<(SystemTime, String)>,
    /// Streams started, each with every table loaded.
    pub starts: u64,
    /// Steps that failed, after which straum started over.
    pub failures: u64,
}

/// A change that no longer fits its table, so straum starts over.
struct Stale;

impl App {
    /// An empty store that answers not ready until a [`Writer`] has loaded
    /// it.
    pub fn new(spec: Spec, limits: Limits) -> App {
        App {
            state: Arc::new(RwLock::new(State {
                store: Store::new(spec),
                position: None,
                time: None,
                lag: None,
                wal: None,
                limits,
                error: None,
                last_failure: None,
                starts: 0,
                failures: 0,
            })),
        }
    }

    /// Every table is loaded, and straum is at most [`Limits::max_lag`]
    /// behind the server.
    pub fn ready(&self) -> bool {
        let state = self.read();
        state.ready(state.behind())
    }

    /// How far behind the server straum is: the time since its slot was
    /// last known to have confirmed all the WAL there was. `None` until
    /// every table is loaded.
    pub fn lag(&self) -> Option<Duration> {
        self.read().behind()
    }

    /// Answers once every table is loaded.
    pub fn ask(&self, ask: &Ask) -> Result<Answered, AnswerError> {
        let state = self.read();
        state
            .store
            .spec()
            .check(ask)
            .map_err(AnswerError::Invalid)?;
        let lsn = state.position.ok_or(AnswerError::NotReady)?;
        let answer = state.store.ask(ask).map_err(AnswerError::Invalid)?;
        Ok(Answered {
            answer,
            lsn,
            as_of: state.as_of().expect(STARTED),
        })
    }

    /// The last failure, until the next start.
    pub fn error(&self) -> Option<String> {
        self.read().error.clone()
    }

    pub fn status(&self) -> Status {
        let state = self.read();
        let lag = state.behind();
        Status {
            ready: state.ready(lag),
            lsn: state.position,
            time: state.time,
            as_of: state.as_of(),
            lag,
            wal: state.wal,
            limits: state.limits,
            error: state.error.clone(),
            last_failure: state.last_failure.clone(),
            starts: state.starts,
            failures: state.failures,
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().expect(POISONED)
    }

    fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().expect(POISONED)
    }
}

impl State {
    fn behind(&self) -> Option<Duration> {
        (self.lag.as_ref()).map(|lag| lag.behind(Instant::now()))
    }

    /// As [`Answered::as_of`].
    fn as_of(&self) -> Option<SystemTime> {
        let since = Instant::now().saturating_duration_since(self.lag.as_ref()?.caught_up());
        // A clock set before 1970 gets the earliest time there is, which is
        // still true.
        Some((SystemTime::now().checked_sub(since)).unwrap_or(SystemTime::UNIX_EPOCH))
    }

    fn ready(&self, lag: Option<Duration>) -> bool {
        lag.is_some_and(|lag| lag <= self.limits.max_lag)
    }

    /// Applies the commit to every fact, or names the first fact it no
    /// longer fits.
    fn apply(&mut self, commit: Commit) -> Result<(), Ident> {
        let Commit { lsn, time, changes } = commit;
        for change in changes {
            let table = change.table().clone();
            // The publication may carry tables the spec doesn't name.
            if self.store.spec().fact(table.as_str()).is_none() {
                continue;
            }
            if self.apply_change(change).is_err() {
                return Err(table);
            }
        }
        self.position = Some(lsn);
        self.time = Some(time);
        Ok(())
    }

    /// A fact needs the whole old row to take it out of its cell.
    fn apply_change(&mut self, change: Change) -> Result<(), Stale> {
        match change {
            Change::Insert { table, row } => self.store.add_fact_row(table.as_str(), &row)?,
            Change::Update { table, old, new } => {
                self.store.remove_fact_row(table.as_str(), &old)?;
                self.store.add_fact_row(table.as_str(), &new)?;
            }
            Change::Delete { table, old } => self.store.remove_fact_row(table.as_str(), &old)?,
            Change::Truncate { table } => {
                self.store.rollup(table.as_str()).expect(IN_SPEC).clear();
            }
            Change::Altered { .. } => return Err(Stale),
        }
        Ok(())
    }
}

impl<S: Source> Writer<S> {
    pub fn new(app: App, source: S) -> Writer<S> {
        let (spec, limits) = {
            let state = app.read();
            (state.store.spec().clone(), state.limits)
        };
        Writer {
            spec,
            limits,
            source,
            app,
            measured: Instant::now(),
        }
    }

    pub fn app(&self) -> &App {
        &self.app
    }

    /// Loads every table when nothing is loaded yet, else applies the next
    /// commit, waiting for one and reading the server's WAL meanwhile.
    /// After an error straum is not ready, and the next step starts over
    /// from a new stream.
    pub fn step(&mut self) -> Result<Step, Error> {
        let started = self.app.read().position.is_some();
        let step = if started {
            self.apply_next().map(Step::Applied)
        } else {
            self.start().map(Step::Started)
        };
        if let Err(e) = &step {
            let mut state = self.app.write();
            state.position = None;
            state.lag = None;
            state.wal = None;
            state.error = Some(e.to_string());
            state.last_failure = Some((SystemTime::now(), e.to_string()));
            state.failures += 1;
        }
        step
    }

    /// Loads every fact into a new store outside the lock, then swaps it
    /// in. The old store goes first, so the two never take memory at once.
    fn start(&mut self) -> Result<Lsn, Error> {
        let empty = || Store::new(self.spec.clone());
        drop(mem::replace(&mut self.app.write().store, empty()));
        // WAL the server writes from here on waits for the load.
        let opened = Instant::now();
        let start = self.source.open(&self.spec)?;
        let mut store = empty();
        for fact in self.spec.facts() {
            self.source.load_fact(fact, &mut store)?;
        }
        let wal = self.measure()?;
        let mut lag = Lag::new(opened);
        lag.read(&wal, self.measured);
        let mut state = self.app.write();
        state.store = store;
        state.position = Some(start);
        state.time = None;
        state.lag = Some(lag);
        state.wal = Some(wal);
        state.error = None;
        state.starts += 1;
        Ok(start)
    }

    fn apply_next(&mut self) -> Result<Lsn, Error> {
        loop {
            if self.measured.elapsed() >= MEASURE_EVERY {
                let wal = self.measure()?;
                let mut state = self.app.write();
                state.lag.as_mut().expect(STARTED).read(&wal, self.measured);
                state.wal = Some(wal);
            }
            let wait = MEASURE_EVERY.saturating_sub(self.measured.elapsed());
            if let Some(commit) = self.source.next_commit(wait)? {
                let lsn = commit.lsn;
                self.app.write().apply(commit).map_err(Error::Unfit)?;
                return Ok(lsn);
            }
        }
    }

    /// Reads the server's WAL, and fails once the slot holds more of it
    /// than [`Limits::max_retained_wal`].
    fn measure(&mut self) -> Result<Wal, Error> {
        // Taken before the read, so the reading holds all WAL written by then.
        let at = Instant::now();
        let wal = self.source.wal()?;
        self.measured = at;
        let (retained, limit) = (wal.retained(), self.limits.max_retained_wal);
        if retained > limit {
            return Err(Error::Retained { retained, limit });
        }
        Ok(wal)
    }
}

impl From<RowError> for Stale {
    fn from(_: RowError) -> Stale {
        Stale
    }
}

#[derive(Debug)]
pub enum Error {
    Source(SourceError),
    /// A change no longer fits the table: a row that doesn't fit, a fact
    /// change without the old row, an altered table.
    Unfit(Ident),
    /// The server holds more WAL for straum's slot than
    /// [`Limits::max_retained_wal`], in bytes.
    Retained {
        retained: u64,
        limit: u64,
    },
}

impl From<SourceError> for Error {
    fn from(e: SourceError) -> Error {
        Error::Source(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Source(e) => write!(f, "{e}"),
            Error::Unfit(table) => write!(f, "a change no longer fits {table}"),
            Error::Retained { retained, limit } => write!(
                f,
                "PostgreSQL holds {} of WAL for straum's slot, past the limit of {}",
                mib(*retained),
                mib(*limit)
            ),
        }
    }
}

impl std::error::Error for Error {}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / f64::from(1 << 20))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerError {
    /// The tables are loading.
    NotReady,
    Invalid(Vec<AskError>),
}

impl fmt::Display for AnswerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnswerError::NotReady => f.write_str("not ready"),
            AnswerError::Invalid(errors) => {
                for (i, e) in errors.iter().enumerate() {
                    if i > 0 {
                        writeln!(f)?;
                    }
                    write!(f, "{e}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for AnswerError {}
