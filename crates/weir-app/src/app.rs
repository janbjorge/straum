//! A store kept in step with a source, one step at a time by one writer.

use std::collections::HashMap;
use std::fmt;
use std::iter;

use weir_domain::{Answer, Ask, AskError, DimTable, Ident, Rollup, RowError, Spec, Store};

use crate::source::{Change, Commit, Lsn, Old, Source, SourceError};

/// Panic message for tables taken from the spec.
const IN_SPEC: &str = "every loaded table is in the spec";

#[derive(Debug)]
pub struct App<S> {
    spec: Spec,
    source: S,
    store: Store,
    /// Every commit up to here is applied. `None` until every table is
    /// loaded, and again after a failure.
    position: Option<Lsn>,
    /// The LSN each table was last loaded at. Its changes in commits up to
    /// there are already in it, and asks on it wait until the stream gets
    /// there.
    loaded: HashMap<Ident, Lsn>,
}

/// What [`App::step`] did.
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
}

/// A table that no longer matches what weir holds, so it is read again.
struct Stale;

enum Target<'a> {
    Fact(&'a mut Rollup),
    Dim(&'a mut DimTable),
}

impl<S: Source> App<S> {
    pub fn new(spec: Spec, source: S) -> App<S> {
        App {
            store: Store::new(spec.clone()),
            spec,
            source,
            position: None,
            loaded: HashMap::new(),
        }
    }

    /// Loads every table when nothing is loaded yet, else applies the next
    /// commit. After an error weir is not ready, and the next step starts
    /// over from a new stream.
    pub fn step(&mut self) -> Result<Step, Error> {
        let step = match self.position {
            None => self.start().map(Step::Started),
            Some(_) => self.apply_next().map(Step::Applied),
        };
        if step.is_err() {
            self.position = None;
        }
        step
    }

    /// Every table is loaded and the stream has passed each load.
    pub fn ready(&self) -> bool {
        self.position
            .is_some_and(|position| self.loaded.values().all(|&at| at <= position))
    }

    /// Answers when the fact and every dim it refs are ready.
    pub fn ask(&self, ask: &Ask) -> Result<Answered, AnswerError> {
        let fact = self.spec.check(ask).map_err(AnswerError::Invalid)?;
        let mut tables = iter::once(&fact.name).chain(fact.refs.iter().map(|r| &r.dim));
        let lsn = self
            .position
            .filter(|&position| tables.all(|t| self.loaded[t] <= position))
            .ok_or(AnswerError::NotReady)?;
        let answer = self.store.ask(ask).expect("checked by Spec::check");
        Ok(Answered { answer, lsn })
    }

    fn start(&mut self) -> Result<Lsn, Error> {
        let start = self.source.open(&self.spec)?;
        let tables: Vec<Ident> = (self.spec.dims().iter().map(|d| &d.name))
            .chain(self.spec.facts().iter().map(|f| &f.name))
            .cloned()
            .collect();
        for table in tables {
            self.load(table, start)?;
        }
        self.position = Some(start);
        Ok(start)
    }

    /// Empties the table and reads it from the source again, which must
    /// not miss a commit up to `position`.
    fn load(&mut self, table: Ident, position: Lsn) -> Result<(), Error> {
        let name = table.as_str();
        let at = match self.spec.fact(name) {
            Some(fact) => {
                let rollup = self.store.rollup(name).expect(IN_SPEC);
                rollup.clear();
                self.source.load_fact(fact, rollup)?
            }
            None => {
                let dim = self.spec.dim(name).expect(IN_SPEC);
                let rows = self.store.dim(name).expect(IN_SPEC);
                rows.clear();
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
        self.loaded.insert(table, at);
        Ok(())
    }

    /// Applies the next commit to every table that hasn't seen it, then
    /// reads again the tables it no longer fits.
    fn apply_next(&mut self) -> Result<Lsn, Error> {
        let Commit { lsn, changes } = self.source.next_commit()?;
        let mut stale = Vec::new();
        for change in changes {
            let table = change.table().clone();
            // The publication may carry tables the spec doesn't name.
            let Some(&at) = self.loaded.get(&table) else {
                continue;
            };
            if lsn <= at || stale.contains(&table) {
                continue;
            }
            if self.apply(change).is_err() {
                stale.push(table);
            }
        }
        self.position = Some(lsn);
        for table in stale {
            self.load(table, lsn)?;
        }
        Ok(lsn)
    }

    /// A fact needs the whole old row to take it out of its cell. A dim
    /// needs only the old key.
    fn apply(&mut self, change: Change) -> Result<(), Stale> {
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

impl From<RowError> for Stale {
    fn from(_: RowError) -> Stale {
        Stale
    }
}

#[derive(Debug)]
pub enum Error {
    Source(SourceError),
    /// The source read a table as it was before commits weir already
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
