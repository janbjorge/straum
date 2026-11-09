//! A store kept in step with a source by one writer, and read by asks while
//! it writes. The writer holds the lock only to apply what it has read,
//! never while it waits on the source, so asks never wait on PostgreSQL.

use std::collections::HashMap;
use std::fmt;
use std::iter;
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
    /// The LSN each table was last loaded at. Its changes in commits up to
    /// there are already in it, and asks on it wait until the stream gets
    /// there. A table being loaded is not here.
    loaded: HashMap<Ident, Lsn>,
    starts: u64,
    commits: u64,
    reloads: u64,
    failures: u64,
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

/// What [`App::status`] reports, for health checks and metrics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// See [`App::ready`].
    pub ready: bool,
    pub lsn: Option<Lsn>,
    pub time: Option<SystemTime>,
    /// In spec order. A fact's size is its cells.
    pub facts: Vec<TableStatus>,
    /// In spec order. A dim's size is its rows.
    pub dims: Vec<TableStatus>,
    /// Streams started, each with every table loaded.
    pub starts: u64,
    pub commits: u64,
    /// Tables read again after a change that didn't fit them.
    pub reloads: u64,
    /// Steps that failed, after which straum started over.
    pub failures: u64,
    /// The last failure, until the next start.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableStatus {
    pub name: Ident,
    pub size: usize,
    /// Asks on the table answer: it is loaded and the stream has passed
    /// its load.
    pub ready: bool,
}

/// A change that no longer fits its table, so the table is read again.
struct Stale;

/// A table taken out of the store while it loads, so asks on the others
/// go on.
enum Taken {
    Fact(Rollup),
    Dim(DimTable),
}

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
                loaded: HashMap::new(),
                starts: 0,
                commits: 0,
                reloads: 0,
                failures: 0,
                error: None,
            })),
        }
    }

    /// Every table is loaded and the stream has passed each load.
    pub fn ready(&self) -> bool {
        let state = self.read();
        tables(&state.spec).all(|t| state.current(t))
    }

    /// Answers when the fact and every dim it refs are ready.
    pub fn ask(&self, ask: &Ask) -> Result<Answered, AnswerError> {
        let state = self.read();
        let fact = state.spec.check(ask).map_err(AnswerError::Invalid)?;
        let mut tables = iter::once(&fact.name).chain(fact.refs.iter().map(|r| &r.dim));
        let lsn = state
            .position
            .filter(|_| tables.all(|t| state.current(t)))
            .ok_or(AnswerError::NotReady)?;
        let answer = state.store.ask(ask).expect("checked by Spec::check");
        Ok(Answered {
            answer,
            lsn,
            time: state.time,
        })
    }

    pub fn status(&self) -> Status {
        let state = self.read();
        let table = |name: &Ident, size| TableStatus {
            name: name.clone(),
            size,
            ready: state.current(name),
        };
        Status {
            ready: tables(&state.spec).all(|t| state.current(t)),
            lsn: state.position,
            time: state.time,
            facts: (state.spec.facts().iter())
                .map(|f| table(&f.name, state.store.cells(f.name.as_str()).expect(IN_SPEC)))
                .collect(),
            dims: (state.spec.dims().iter())
                .map(|d| table(&d.name, state.store.rows(d.name.as_str()).expect(IN_SPEC)))
                .collect(),
            starts: state.starts,
            commits: state.commits,
            reloads: state.reloads,
            failures: state.failures,
            error: state.error.clone(),
        }
    }

    fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().expect(POISONED)
    }

    fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().expect(POISONED)
    }
}

/// Dims first, so a fact is never ready before the dims it refs.
fn tables(spec: &Spec) -> impl Iterator<Item = &Ident> {
    (spec.dims().iter().map(|d| &d.name)).chain(spec.facts().iter().map(|f| &f.name))
}

impl State {
    /// Loaded, and the stream has passed the load.
    fn current(&self, table: &Ident) -> bool {
        self.position
            .is_some_and(|position| self.loaded.get(table).is_some_and(|&at| at <= position))
    }

    /// Swaps an empty table in for the loaded one. Asks on it answer not
    /// ready until [`State::put`] brings it back.
    fn take(&mut self, table: &Ident) -> Taken {
        self.loaded.remove(table);
        let name = table.as_str();
        match self.spec.fact(name) {
            Some(fact) => {
                let empty = Rollup::new(fact);
                Taken::Fact(mem::replace(self.store.rollup(name).expect(IN_SPEC), empty))
            }
            None => {
                let empty = DimTable::new(self.spec.dim(name).expect(IN_SPEC));
                Taken::Dim(mem::replace(self.store.dim(name).expect(IN_SPEC), empty))
            }
        }
    }

    fn put(&mut self, table: Ident, taken: Taken, at: Lsn) {
        let name = table.as_str();
        match taken {
            Taken::Fact(rollup) => *self.store.rollup(name).expect(IN_SPEC) = rollup,
            Taken::Dim(rows) => *self.store.dim(name).expect(IN_SPEC) = rows,
        }
        self.loaded.insert(table, at);
    }

    /// Applies the commit to every loaded table that hasn't seen it, and
    /// returns the tables it no longer fits. Those are no longer loaded, so
    /// asks on them wait for them to be read again.
    fn apply(&mut self, commit: Commit) -> Vec<Ident> {
        let Commit { lsn, time, changes } = commit;
        let mut stale = Vec::new();
        for change in changes {
            let table = change.table().clone();
            // The publication may carry tables the spec doesn't name, and a
            // table found stale earlier in the commit is no longer loaded.
            let Some(&at) = self.loaded.get(&table) else {
                continue;
            };
            if lsn <= at {
                continue;
            }
            if self.apply_change(change).is_err() {
                self.loaded.remove(&table);
                stale.push(table);
            }
        }
        self.position = Some(lsn);
        self.time = Some(time);
        self.commits += 1;
        stale
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
            state.failures += 1;
            state.error = Some(e.to_string());
        }
        step
    }

    fn start(&mut self) -> Result<Lsn, Error> {
        let start = self.source.open(&self.spec)?;
        let tables: Vec<Ident> = tables(&self.spec).cloned().collect();
        for table in tables {
            self.load(table, start)?;
        }
        let mut state = self.app.write();
        state.position = Some(start);
        state.time = None;
        state.starts += 1;
        state.error = None;
        Ok(start)
    }

    /// Reads the table from the source again, outside the lock. The source
    /// must not miss a commit up to `position`.
    fn load(&mut self, table: Ident, position: Lsn) -> Result<(), Error> {
        let mut taken = self.app.write().take(&table);
        let name = table.as_str();
        let at = match &mut taken {
            Taken::Fact(rollup) => {
                rollup.clear();
                let fact = self.spec.fact(name).expect(IN_SPEC);
                self.source.load_fact(fact, rollup)?
            }
            Taken::Dim(rows) => {
                rows.clear();
                let dim = self.spec.dim(name).expect(IN_SPEC);
                self.source.load_dim(dim, rows)?
            }
        };
        if at < position {
            return Err(Error::LoadBehind {
                table,
                at,
                position,
            });
        }
        self.app.write().put(table, taken, at);
        Ok(())
    }

    /// Applies the next commit, then reads again the tables it no longer
    /// fits.
    fn apply_next(&mut self) -> Result<Lsn, Error> {
        let commit = self.source.next_commit()?;
        let lsn = commit.lsn;
        let stale = self.app.write().apply(commit);
        for table in stale {
            self.load(table, lsn)?;
            self.app.write().reloads += 1;
        }
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
    /// The source read a table as it was before commits straum already
    /// applied, so the table would miss them.
    LoadBehind {
        table: Ident,
        at: Lsn,
        position: Lsn,
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
            Error::LoadBehind {
                table,
                at,
                position,
            } => write!(
                f,
                "{table} was read at {at}, behind the stream at {position}"
            ),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerError {
    /// A table the ask reads is loading, or the stream hasn't reached its
    /// load yet.
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
