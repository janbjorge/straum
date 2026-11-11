//! A store kept in step with a source by one writer, and read by asks while
//! it writes. The writer holds the lock only to apply what it has read,
//! never while it waits on the source, so asks never wait on PostgreSQL.
//! With a spill, the writer seals `date_bin` buckets that ended
//! `seal_after` ago into it and unseals one before a change reaches it;
//! asks read sealed buckets from it (ADR-0033, ADR-0035).

use std::fmt;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant, SystemTime};

use straum_domain::{Answer, Ask, AskError, Identifier, SealedError, SealedId, Spec, Store};

use crate::lag::Lag;
use crate::source::{Change, Commit, Lsn, Source, SourceError, Wal};
use crate::spill::{SpillError, Spilling};

/// The store may be half updated, so nothing reads it again.
const POISONED: &str = "the writer panicked while it held the store";

const STARTED: &str = "the writer applies commits only to a started stream";

/// How often the writer reads the server's WAL while it waits for commits.
/// The lag can look up to this much longer than it is.
const WAL_EVERY: Duration = Duration::from_secs(1);

/// How far behind the server Straum may fall.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Past this, Straum is not ready, though asks still answer.
    pub max_lag: Duration,
    /// Bytes of WAL the server may keep for Straum's slot. Past this,
    /// Straum starts over, so a Straum that can't keep up doesn't fill the
    /// server's disk.
    pub max_retained_wal: u64,
}

/// The store as the writer last left it, for asks. Clones share it.
#[derive(Clone)]
pub struct App {
    state: Arc<RwLock<State>>,
}

struct State {
    store: Store,
    /// `None` until every table is loaded, and again after a failure.
    following: Option<Following>,
    limits: Limits,
    /// `None` keeps every bucket in memory.
    spilling: Option<Spilling>,
    /// The last failure and when it came, kept across starts.
    last_failure: Option<Failure>,
    starts: u64,
    failures: u64,
    unseals: u64,
}

/// Where a stream with every table loaded stands.
#[derive(Debug)]
struct Following {
    /// Every commit up to here is applied.
    applied: Lsn,
    /// When the commit at `applied` committed. `None` when `applied` is
    /// where the stream started.
    committed: Option<SystemTime>,
    /// How far behind the server the stream is.
    lag: Lag,
    /// The last reading of the server's WAL.
    wal: Wal,
}

/// Reads the source and keeps an [`App`] in step with it: the loop that
/// does the I/O. One writer per app.
pub struct Writer<S> {
    source: S,
    app: App,
    /// When the server's WAL was last read.
    wal_read_at: Instant,
}

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
    pub applied: Lsn,
    /// Every commit made before this, by Straum's clock, is in the answer:
    /// when the slot was last known to have confirmed all the WAL there
    /// was. Unlike the time of the commit at `lsn`, it moves on while the
    /// database is idle.
    pub as_of: SystemTime,
}

/// What [`App::status`] reports, read at one moment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// Every table is loaded, and Straum is at most [`Limits::max_lag`]
    /// behind the server.
    pub ready: bool,
    /// Every commit up to here is applied. `None` until every table is
    /// loaded, and again after a failure.
    pub applied: Option<Lsn>,
    /// When the commit at `applied` committed. `None` when `applied` is
    /// where the stream started.
    pub committed: Option<SystemTime>,
    /// As [`Answered::as_of`]. `None` until every table is loaded.
    pub as_of: Option<SystemTime>,
    /// The time since Straum's slot was last known to have confirmed all
    /// the WAL there was. `None` until every table is loaded.
    pub lag: Option<Duration>,
    /// The last reading of the server's WAL, while a stream runs.
    pub wal: Option<Wal>,
    pub limits: Limits,
    /// The last failure, until the next start.
    pub failure: Option<String>,
    /// The last failure and when it came, kept across starts.
    pub last_failure: Option<Failure>,
    /// Streams started, each with every table loaded.
    pub starts: u64,
    /// Steps that failed, after which Straum started over.
    pub failures: u64,
    /// Buckets in the spill now; `None` without a spill.
    pub sealed: Option<usize>,
    /// Sealed buckets a change brought back, since Straum started. One
    /// that keeps climbing means old buckets keep changing.
    pub unseals: u64,
}

/// A step that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub at: SystemTime,
    pub error: String,
}

impl App {
    /// An empty store that answers not ready until a [`Writer`] has loaded
    /// it. `spilling` seals buckets into a spill; `None` keeps them all in
    /// memory.
    pub fn new(spec: Spec, limits: Limits, spilling: Option<Spilling>) -> App {
        App {
            state: Arc::new(RwLock::new(State {
                store: Store::new(spec),
                following: None,
                limits,
                spilling,
                last_failure: None,
                starts: 0,
                failures: 0,
                unseals: 0,
            })),
        }
    }

    /// Answers once every table is loaded.
    pub fn ask(&self, ask: &Ask) -> Result<Answered, AnswerError> {
        let state = self.read();
        let store = &state.store;
        // Checks the ask, so an invalid one is refused even while loading.
        let ids = store.sealed_read(ask).map_err(AnswerError::Invalid)?;
        let following = state.following.as_ref().ok_or(AnswerError::NotReady)?;
        let thawed = if ids.is_empty() {
            Vec::new()
        } else {
            let spilling = state
                .spilling
                .as_ref()
                .expect("only a spilling writer seals");
            (ids.iter())
                .map(|id| {
                    let sealed = spilling.spill.get(id).map_err(spill_error)?;
                    store.thaw(sealed).map_err(spill_error)
                })
                .collect::<Result<_, _>>()?
        };
        let answer = store.ask_with(ask, &thawed).map_err(AnswerError::Invalid)?;
        Ok(Answered {
            answer,
            applied: following.applied,
            as_of: following.as_of(),
        })
    }

    pub fn status(&self) -> Status {
        let state = self.read();
        let following = state.following.as_ref();
        let lag = state.behind();
        Status {
            ready: state.ready(lag),
            applied: following.map(|f| f.applied),
            committed: following.and_then(|f| f.committed),
            as_of: following.map(Following::as_of),
            lag,
            wal: following.map(|f| f.wal),
            limits: state.limits,
            failure: (state.last_failure.as_ref())
                .filter(|_| following.is_none())
                .map(|failure| failure.error.clone()),
            last_failure: state.last_failure.clone(),
            starts: state.starts,
            failures: state.failures,
            sealed: state.spilling.as_ref().map(|_| {
                (state.store.spec().facts().iter())
                    .map(|fact| state.store.sealed(fact.name.as_str()).len())
                    .sum()
            }),
            unseals: state.unseals,
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().expect(POISONED)
    }

    fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().expect(POISONED)
    }
}

fn spill_error(e: impl fmt::Display) -> AnswerError {
    AnswerError::Spill(e.to_string())
}

impl Following {
    fn behind(&self) -> Duration {
        self.lag.behind(Instant::now())
    }

    /// As [`Answered::as_of`].
    fn as_of(&self) -> SystemTime {
        let since = Instant::now().saturating_duration_since(self.lag.caught_up());
        // A clock set before 1970 gets the earliest time there is, which is
        // still true.
        (SystemTime::now().checked_sub(since)).unwrap_or(SystemTime::UNIX_EPOCH)
    }
}

impl State {
    fn behind(&self) -> Option<Duration> {
        self.following.as_ref().map(Following::behind)
    }

    fn following(&mut self) -> &mut Following {
        self.following.as_mut().expect(STARTED)
    }

    fn ready(&self, lag: Option<Duration>) -> bool {
        lag.is_some_and(|lag| lag <= self.limits.max_lag)
    }

    /// Applies the commit to every fact and dim, or names the first table
    /// it no longer fits.
    fn apply(&mut self, commit: Commit) -> Result<(), Identifier> {
        let Commit {
            lsn,
            committed,
            changes,
        } = commit;
        for change in changes {
            let table = change.table();
            let spec = self.store.spec();
            // The publication may carry tables the spec doesn't name.
            if spec.fact(table.as_str()).is_none() && spec.dim(table.as_str()).is_none() {
                continue;
            }
            if !apply_change(&mut self.store, &change) {
                return Err(table.clone());
            }
        }
        let following = self.following();
        following.applied = lsn;
        following.committed = Some(committed);
        Ok(())
    }
}

/// Applies one change; `false` when it no longer fits. Facts and dims need
/// the whole old row: a fact to take it out of its cell, a dim because a
/// key change is the old row out and the new one in.
fn apply_change(store: &mut Store, change: &Change) -> bool {
    match change {
        Change::Insert { table, row } => store.add_row(table.as_str(), row).is_ok(),
        Change::Update { table, old, new } => store.update_row(table.as_str(), old, new).is_ok(),
        Change::Delete { table, old } => store.remove_row(table.as_str(), old).is_ok(),
        Change::Truncate { table } => {
            store.clear(table.as_str());
            true
        }
        Change::Unfit { .. } => false,
    }
}

impl<S: Source> Writer<S> {
    pub fn new(app: App, source: S) -> Writer<S> {
        Writer {
            source,
            app,
            wal_read_at: Instant::now(),
        }
    }

    pub fn app(&self) -> &App {
        &self.app
    }

    /// Loads every table when nothing is loaded yet, else applies the next
    /// commit, waiting for one and reading the server's WAL meanwhile.
    /// After an error Straum is not ready, and the next step starts over
    /// from a new stream.
    pub fn step(&mut self) -> Result<Step, Error> {
        let started = self.app.read().following.is_some();
        let step = if started {
            self.apply_next().map(Step::Applied)
        } else {
            self.start().map(Step::Started)
        };
        if let Err(e) = &step {
            let mut state = self.app.write();
            state.following = None;
            state.last_failure = Some(Failure {
                at: SystemTime::now(),
                error: e.to_string(),
            });
            state.failures += 1;
        }
        step
    }

    /// Loads every dim and fact into a new store outside the lock, then
    /// swaps it in. The old store goes first, so the two never take memory
    /// at once.
    fn start(&mut self) -> Result<Lsn, Error> {
        let spec = self.app.read().store.spec().clone();
        self.app.write().store = Store::new(spec.clone());
        if let Some(spilling) = self.spilling() {
            spilling.spill.clear().map_err(Error::Spill)?;
        }
        // WAL the server writes from here on waits for the load.
        let opened = Instant::now();
        let start = self.source.open(&spec)?;
        let mut store = Store::new(spec.clone());
        let dims = spec.dims().iter().map(|dim| &dim.name);
        let facts = spec.facts().iter().map(|fact| &fact.name);
        for table in dims.chain(facts) {
            self.source.load(table, &mut store)?;
        }
        let wal = self.read_wal()?;
        let mut lag = Lag::new(opened);
        lag.read(&wal, self.wal_read_at);
        let mut state = self.app.write();
        state.store = store;
        state.following = Some(Following {
            applied: start,
            committed: None,
            lag,
            wal,
        });
        state.starts += 1;
        drop(state);
        self.seal()?;
        Ok(start)
    }

    fn apply_next(&mut self) -> Result<Lsn, Error> {
        loop {
            if self.wal_read_at.elapsed() >= WAL_EVERY {
                let wal = self.read_wal()?;
                let mut state = self.app.write();
                let following = state.following();
                following.lag.read(&wal, self.wal_read_at);
                following.wal = wal;
                drop(state);
                self.seal()?;
            }
            let wait = WAL_EVERY.saturating_sub(self.wal_read_at.elapsed());
            if let Some(commit) = self.source.next_commit(wait)? {
                let lsn = commit.lsn;
                self.unseal(&commit)?;
                let truncated = truncated(&self.app.read().store, &commit);
                self.app.write().apply(commit).map_err(Error::Unfit)?;
                self.forget(&truncated)?;
                return Ok(lsn);
            }
        }
    }

    /// Seals each bucket that ended [`Spilling::seal_after`] ago: put into
    /// the spill before its cells go, so an ask never meets a sealed bucket
    /// the spill doesn't hold. Only the writer changes the store, so the
    /// copy is still the bucket when its cells go.
    fn seal(&mut self) -> Result<(), Error> {
        let Some(spilling) = self.spilling() else {
            return Ok(());
        };
        let since_epoch = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH);
        let now = since_epoch.map_or(0, seconds);
        let before = now.saturating_sub(seconds(spilling.seal_after));
        let ids = self.app.read().store.sealable(before);
        for id in ids {
            let Some(sealed) = self.app.read().store.sealed_copy(&id) else {
                continue;
            };
            spilling.spill.put(&sealed).map_err(Error::Spill)?;
            self.app.write().store.seal(&id)?;
        }
        Ok(())
    }

    /// Unseals the buckets the commit's fact rows fall in, so it applies.
    fn unseal(&mut self, commit: &Commit) -> Result<(), Error> {
        let Some(spilling) = self.spilling() else {
            return Ok(());
        };
        let ids = touched(&self.app.read().store, commit);
        if ids.is_empty() {
            return Ok(());
        }
        let sealed = (ids.iter())
            .map(|id| spilling.spill.get(id))
            .collect::<Result<Vec<_>, _>>()
            .map_err(Error::Spill)?;
        {
            let mut state = self.app.write();
            for sealed in sealed {
                let thawed = state.store.thaw(sealed)?;
                state.store.unseal(thawed)?;
                state.unseals += 1;
            }
        }
        self.forget(&ids)
    }

    /// Removes buckets the store no longer holds sealed, so the spill
    /// holds what the store has sealed and no more.
    fn forget(&self, ids: &[SealedId]) -> Result<(), Error> {
        let Some(spilling) = self.spilling() else {
            return Ok(());
        };
        for id in ids {
            spilling.spill.remove(id).map_err(Error::Spill)?;
        }
        Ok(())
    }

    /// The spill, read out of the state so no lock is held while it works.
    fn spilling(&self) -> Option<Spilling> {
        self.app.read().spilling.clone()
    }

    /// Reads the server's WAL, and fails once the slot retains more of it
    /// than [`Limits::max_retained_wal`].
    fn read_wal(&mut self) -> Result<Wal, Error> {
        // Taken before the read, so the reading holds all WAL written by then.
        let at = Instant::now();
        let wal = self.source.wal()?;
        self.wal_read_at = at;
        let (retained, limit) = (wal.retained(), self.app.read().limits.max_retained_wal);
        if retained > limit {
            return Err(Error::Retained { retained, limit });
        }
        Ok(wal)
    }
}

/// The sealed buckets a commit's fact rows fall in, each once.
fn touched(store: &Store, commit: &Commit) -> Vec<SealedId> {
    let mut ids = Vec::new();
    for change in &commit.changes {
        let table = change.table().as_str();
        if store.spec().fact(table).is_none() {
            continue;
        }
        let rows: &[&Vec<_>] = match change {
            Change::Insert { row, .. } => &[row],
            Change::Update { old, new, .. } => &[old, new],
            Change::Delete { old, .. } => &[old],
            Change::Truncate { .. } | Change::Unfit { .. } => &[],
        };
        for row in rows {
            ids.extend(store.sealed_touched(table, row));
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

/// The sealed buckets of the facts a commit truncates.
fn truncated(store: &Store, commit: &Commit) -> Vec<SealedId> {
    (commit.changes.iter())
        .filter_map(|change| match change {
            Change::Truncate { table } if store.spec().fact(table.as_str()).is_some() => {
                Some(store.sealed(table.as_str()))
            }
            _ => None,
        })
        .flatten()
        .collect()
}

/// Whole seconds, saturating.
fn seconds(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

#[derive(Debug)]
pub enum Error {
    Source(SourceError),
    /// A change no longer fits the table: a row that doesn't fit, a change
    /// without the old row, a dim row Straum never held, an altered table.
    Unfit(Identifier),
    /// The server holds more WAL for Straum's slot than
    /// [`Limits::max_retained_wal`], in bytes.
    Retained {
        retained: u64,
        limit: u64,
    },
    Spill(SpillError),
    /// A bucket from the spill doesn't fit the store.
    Sealed(SealedError),
}

impl From<SealedError> for Error {
    fn from(e: SealedError) -> Error {
        Error::Sealed(e)
    }
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
                "PostgreSQL holds {} of WAL for Straum's slot, past the limit of {}",
                mib(*retained),
                mib(*limit)
            ),
            Error::Spill(e) => write!(f, "spill: {e}"),
            Error::Sealed(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

/// Bytes as mebibytes, for messages.
pub fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / f64::from(1 << 20))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerError {
    /// The tables are loading.
    NotReady,
    Invalid(Vec<AskError>),
    /// A sealed bucket the ask reads could not be read back.
    Spill(String),
}

impl fmt::Display for AnswerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnswerError::NotReady => f.write_str("not ready"),
            AnswerError::Spill(e) => write!(f, "reading a sealed bucket failed: {e}"),
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
