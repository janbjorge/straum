//! A store kept in step with a source by one writer, and read by asks while
//! it writes. The writer holds the lock only to apply what it has read,
//! never while it waits on the source, so asks never wait on PostgreSQL.

use std::fmt;
use std::mem;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::SystemTime;

use straum_domain::{Answer, Ask, AskError, DimTable, Ident, Rollup, RowError, Spec, Store};

use crate::source::{Change, Commit, Lsn, Old, Source, SourceError};

/// Panic message for tables taken from the spec.
const IN_SPEC: &str = "every loaded table is in the spec";

/// Panic message for a poisoned lock. The store may be half updated, so
/// nothing reads it again.
const POISONED: &str = "the writer panicked while it held the store";

/// The store as the writer last left it, for asks. Clones share it.
#[derive(Debug, Clone)]
pub struct App {
    state: Arc<RwLock<State>>,
}

#[derive(Debug)]
struct State {
    spec: Spec,
    store: Store,
    /// Every commit up to here is applied. `None` until every table is
    /// loaded, and again after a failure.
    position: Option<Lsn>,
    /// When the commit at `position` committed. `None` when `position` is
    /// where a stream started.
    time: Option<SystemTime>,
    /// The last failure, until the next start.
    error: Option<String>,
}

/// Reads the source and keeps an [`App`] in step with it: the loop that
/// does the I/O. One writer per app.
#[derive(Debug)]
pub struct Writer<S> {
    spec: Spec,
    source: S,
    app: App,
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
    /// When the commit at `lsn` committed. `None` when `lsn` is where the
    /// stream started and no commit has come since.
    pub time: Option<SystemTime>,
}

/// A change that no longer fits its table, so straum starts over.
struct Stale;

impl App {
    /// An empty store that answers not ready until a [`Writer`] has loaded
    /// it.
    pub fn new(spec: Spec) -> App {
        App {
            state: Arc::new(RwLock::new(State {
                store: Store::new(spec.clone()),
                spec,
                position: None,
                time: None,
                error: None,
            })),
        }
    }

    /// Every table is loaded.
    pub fn ready(&self) -> bool {
        self.read().position.is_some()
    }

    /// Answers once every table is loaded.
    pub fn ask(&self, ask: &Ask) -> Result<Answered, AnswerError> {
        let state = self.read();
        state.spec.check(ask).map_err(AnswerError::Invalid)?;
        let lsn = state.position.ok_or(AnswerError::NotReady)?;
        let answer = state.store.ask(ask).expect("checked by Spec::check");
        Ok(Answered {
            answer,
            lsn,
            time: state.time,
        })
    }

    /// The last failure, until the next start.
    pub fn error(&self) -> Option<String> {
        self.read().error.clone()
    }

    fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().expect(POISONED)
    }

    fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().expect(POISONED)
    }
}

impl State {
    /// Applies the commit to every table, or names the first table it no
    /// longer fits.
    fn apply(&mut self, commit: Commit) -> Result<(), Ident> {
        let Commit { lsn, time, changes } = commit;
        for change in changes {
            let table = change.table().clone();
            // The publication may carry tables the spec doesn't name.
            let name = table.as_str();
            if self.spec.fact(name).is_none() && self.spec.dim(name).is_none() {
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

    /// A fact needs the whole old row to take it out of its cell. A dim
    /// needs only the old key.
    fn apply_change(&mut self, change: Change) -> Result<(), Stale> {
        match change {
            Change::Insert { table, row } => match self.target(&table) {
                Target::Fact(rollup) => rollup.add(&row)?,
                Target::Dim(rows) => rows.upsert(&row)?,
            },
            Change::Update { table, old, new } => match self.target(&table) {
                Target::Fact(rollup) => {
                    let Old::Row(old) = old else {
                        return Err(Stale);
                    };
                    rollup.remove(&old)?;
                    rollup.add(&new)?;
                }
                Target::Dim(rows) => {
                    rows.upsert(&new)?;
                    if let Some(key) = old.dim_key()
                        && key != new[0]
                    {
                        rows.delete(&key)?;
                    }
                }
            },
            Change::Delete { table, old } => match self.target(&table) {
                Target::Fact(rollup) => {
                    let Old::Row(old) = old else {
                        return Err(Stale);
                    };
                    rollup.remove(&old)?;
                }
                Target::Dim(rows) => rows.delete(&old.dim_key().ok_or(Stale)?)?,
            },
            Change::Truncate { table } => match self.target(&table) {
                Target::Fact(rollup) => rollup.clear(),
                Target::Dim(rows) => rows.clear(),
            },
            Change::Altered { .. } => return Err(Stale),
        }
        Ok(())
    }

    fn target(&mut self, table: &Ident) -> Target<'_> {
        let name = table.as_str();
        if self.spec.fact(name).is_some() {
            Target::Fact(self.store.rollup(name).expect(IN_SPEC))
        } else {
            Target::Dim(self.store.dim(name).expect(IN_SPEC))
        }
    }
}

enum Target<'a> {
    Fact(&'a mut Rollup),
    Dim(&'a mut DimTable),
}

impl<S: Source> Writer<S> {
    pub fn new(app: App, source: S) -> Writer<S> {
        let spec = app.read().spec.clone();
        Writer { spec, source, app }
    }

    pub fn app(&self) -> &App {
        &self.app
    }

    /// Loads every table when nothing is loaded yet, else applies the next
    /// commit, waiting for one. After an error straum is not ready, and the
    /// next step starts over from a new stream.
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
            state.error = Some(e.to_string());
        }
        step
    }

    /// Loads every table into a new store outside the lock, then swaps it
    /// in. The old store goes first, so the two never take memory at once.
    fn start(&mut self) -> Result<Lsn, Error> {
        let empty = || Store::new(self.spec.clone());
        drop(mem::replace(&mut self.app.write().store, empty()));
        let start = self.source.open(&self.spec)?;
        let mut store = empty();
        for dim in self.spec.dims() {
            let rows = store.dim(dim.name.as_str()).expect(IN_SPEC);
            self.source.load_dim(dim, rows)?;
        }
        for fact in self.spec.facts() {
            let rollup = store.rollup(fact.name.as_str()).expect(IN_SPEC);
            self.source.load_fact(fact, rollup)?;
        }
        let mut state = self.app.write();
        state.store = store;
        state.position = Some(start);
        state.time = None;
        state.error = None;
        Ok(start)
    }

    fn apply_next(&mut self) -> Result<Lsn, Error> {
        let commit = self.source.next_commit()?;
        let lsn = commit.lsn;
        self.app.write().apply(commit).map_err(Error::Unfit)?;
        Ok(lsn)
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
        }
    }
}

impl std::error::Error for Error {}

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
