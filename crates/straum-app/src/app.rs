//! The heart of Straum: one store, filled and kept up to date by the
//! `Writer`, and read by asks through `App`. The writer loads every table
//! from the `Source`, then applies one commit at a time; it takes the lock
//! only to apply what it has read, never while it waits on PostgreSQL, so
//! asks never wait on the database.

use std::fmt;
use std::ops::Range;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant, SystemTime};

use straum_domain::{
    Answer, Ask, AskError, AskKey, ColumnType, Fact, Identifier, Spec, Store, Table, Width,
};

use crate::lag::Lag;
use crate::source::{Change, Commit, Lsn, Source, SourceError, Wal};

/// The store may be half updated, so nothing reads it again.
const POISONED: &str = "the writer panicked while it held the store";

const IN_SPEC: &str = "the store holds every view and dim of its spec";

const STARTED: &str = "the writer applies commits only to a started stream";

/// How often the writer reads the server's WAL while it waits for commits.
/// The lag can look up to this much longer than it is.
const WAL_EVERY: Duration = Duration::from_secs(1);

const BYTES_PER_MIB: u32 = 1 << 20;

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
    limits: Limits,
}

/// Everything behind the lock: the store and what the writer did to it.
struct State {
    store: Store,
    /// `None` until every table is loaded, and again after a failure.
    following: Option<Following>,
    /// The last failure and when it came, kept across starts.
    last_failure: Option<Failure>,
    starts: u64,
    failures: u64,
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
// Generic over the source: the binary runs it on PostgreSQL, the tests on
// an in-memory fake.
pub struct Writer<S> {
    source: S,
    app: App,
    /// When the server's WAL was last read.
    wal_read_at: Instant,
}

/// What one [`Writer::step`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Loaded every table from a new stream that starts here.
    Started(Lsn),
    /// Applied the commit that ends here.
    Applied(Lsn),
}

/// An ask's answer, and how fresh it is.
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
    /// The last failure and when it came, kept across starts.
    pub last_failure: Option<Failure>,
    /// Streams started, each with every table loaded.
    pub starts: u64,
    /// Steps that failed, after which Straum started over.
    pub failures: u64,
}

/// A step that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub at: SystemTime,
    pub error: String,
}

/// What [`App::held`] reports, read at one moment: every view and dim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Held {
    pub views: Vec<HeldView>,
    pub dims: Vec<HeldDim>,
}

/// A view as asks may read it, and how much of it the store holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldView {
    pub name: Identifier,
    /// The `date_bin` width, then the wider ones.
    pub widths: Vec<Width>,
    /// The type of `bucket`.
    pub bucket: ColumnType,
    /// Every name an ask may group or filter by, `bucket` aside.
    pub keys: Vec<HeldKey>,
    pub measures: Vec<Identifier>,
    /// Keyed cells at the `date_bin` width.
    pub cells: usize,
    /// Bound cells at the `date_bin` width, for a view that keeps them.
    pub bound_cells: Option<usize>,
    /// From the start of the first `date_bin` bucket to the end of the
    /// last, in Unix seconds; `None` for a view with no bucket.
    pub range: Option<Range<i64>>,
}

/// A key an ask may name, and the type of its values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldKey {
    pub name: Identifier,
    pub column_type: ColumnType,
}

/// A dim's table, how many rows of it the store holds, and the view keys
/// that read its columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldDim {
    pub table: Table,
    pub rows: usize,
    /// An ask never reads a dim; it reads these keys of the views instead.
    pub asked_as: Vec<ViewKey>,
}

/// A key of a view, as `view.key`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewKey {
    pub view: Identifier,
    pub key: Identifier,
}

impl App {
    /// An empty store that answers not ready until a [`Writer`] has loaded
    /// it.
    pub fn new(spec: Spec, limits: Limits) -> App {
        App {
            state: Arc::new(RwLock::new(State {
                store: Store::new(spec),
                following: None,
                last_failure: None,
                starts: 0,
                failures: 0,
            })),
            limits,
        }
    }

    /// Answers once every table is loaded.
    pub fn ask(&self, ask: &Ask) -> Result<Answered, AnswerError> {
        let state = self.read();
        // An ask the spec refuses is refused even while loading; only a
        // load knows the types the store checks.
        (state.store.spec().check_ask(ask)).map_err(AnswerError::Invalid)?;
        let following = state.following.as_ref().ok_or(AnswerError::NotReady)?;
        let answer = state.store.ask(ask).map_err(AnswerError::Invalid)?;
        Ok(Answered {
            answer,
            applied: following.applied,
            as_of: following.as_of(),
        })
    }

    /// How Straum stands now: loaded or not, how far behind, what failed.
    pub fn status(&self) -> Status {
        let state = self.read();
        let mut status = Status {
            ready: false,
            applied: None,
            committed: None,
            as_of: None,
            lag: None,
            wal: None,
            limits: self.limits,
            last_failure: state.last_failure.clone(),
            starts: state.starts,
            failures: state.failures,
        };
        if let Some(following) = &state.following {
            let lag = following.lag.behind(Instant::now());
            status.ready = lag <= self.limits.max_lag;
            status.applied = Some(following.applied);
            status.committed = following.committed;
            status.as_of = Some(following.as_of());
            status.lag = Some(lag);
            status.wal = Some(following.wal);
        }
        status
    }

    /// `None` until every table is loaded: only a load knows the types.
    pub fn held(&self) -> Option<Held> {
        let state = self.read();
        state.following.as_ref()?;
        let store = &state.store;
        let mut views = Vec::new();
        for fact in store.spec().facts() {
            views.push(held_view(store, fact));
        }
        let mut dims = Vec::new();
        for dim in store.spec().dims() {
            dims.push(HeldDim {
                table: dim.table.clone(),
                rows: store.dim_rows(dim.name.as_str()).expect(IN_SPEC),
                asked_as: asked_as(store.spec(), &dim.name),
            });
        }
        Some(Held { views, dims })
    }

    /// The ask for every row PostgreSQL would give for the view: each
    /// measure by the bucket and every key, over the buckets the store
    /// holds. `None` for a name no view has.
    pub fn rows_ask(&self, view: &str) -> Option<Ask> {
        let state = self.read();
        let fact = state.store.spec().fact(view)?;
        // A view with no bucket is asked for one, and answers no rows.
        let range = (state.store.range(view)).unwrap_or(0..i64::from(fact.time.width.seconds));
        let mut keys = vec![AskKey::Bucket];
        for key in fact.keys() {
            keys.push(AskKey::Key(key.clone()));
        }
        Some(Ask {
            view: fact.name.clone(),
            measures: measure_names(fact),
            keys,
            filter: Vec::new(),
            having: Vec::new(),
            from: Some(range.start),
            to: Some(range.end),
        })
    }

    fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().expect(POISONED)
    }

    fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().expect(POISONED)
    }
}

/// Each key a view reads from `dim` through a ref.
fn asked_as(spec: &Spec, dim: &Identifier) -> Vec<ViewKey> {
    let mut keys = Vec::new();
    for fact in spec.facts() {
        for join in &fact.refs {
            if join.dim != *dim {
                continue;
            }
            for attribute in &join.reads {
                keys.push(ViewKey {
                    view: fact.name.clone(),
                    key: attribute.name.clone(),
                });
            }
        }
    }
    keys
}

fn held_view(store: &Store, fact: &Fact) -> HeldView {
    let name = fact.name.as_str();
    let mut widths = vec![fact.time.width];
    widths.extend(fact.time.wider.iter().copied());
    let mut keys = Vec::new();
    for key in fact.keys() {
        keys.push(HeldKey {
            name: key.clone(),
            column_type: store.key_type(name, key).expect(IN_SPEC),
        });
    }
    HeldView {
        name: fact.name.clone(),
        widths,
        bucket: fact.time.origin.column_type(),
        keys,
        measures: measure_names(fact),
        cells: store.cells(name).expect(IN_SPEC),
        bound_cells: store.bound_cells(name),
        range: store.range(name),
    }
}

fn measure_names(fact: &Fact) -> Vec<Identifier> {
    let mut names = Vec::new();
    for measure in &fact.measures {
        names.push(measure.name.clone());
    }
    names
}

impl Following {
    /// As [`Answered::as_of`].
    fn as_of(&self) -> SystemTime {
        let since = Instant::now().saturating_duration_since(self.lag.caught_up());
        // A clock set before 1970 gets the earliest time there is, which is
        // still true.
        (SystemTime::now().checked_sub(since)).unwrap_or(SystemTime::UNIX_EPOCH)
    }
}

impl State {
    fn following(&mut self) -> &mut Following {
        self.following.as_mut().expect(STARTED)
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
                // The earlier changes of this commit are in the store.
                // Clear the stream before the write guard drops, so an ask
                // waiting on it cannot read them.
                self.following = None;
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
    /// A writer that has loaded nothing yet; its first step loads.
    pub fn new(app: App, source: S) -> Writer<S> {
        Writer {
            source,
            app,
            wal_read_at: Instant::now(),
        }
    }

    /// The app this writer keeps in step, for asks.
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
        if let Err(error) = &step {
            let mut state = self.app.write();
            state.following = None;
            state.last_failure = Some(Failure {
                at: SystemTime::now(),
                error: error.to_string(),
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
        // WAL the server writes from here on waits for the load.
        let opened = Instant::now();
        let start = self.source.open(&spec)?;
        let mut store = Store::new(spec.clone());
        // Dims first, so a fact's rows find theirs when asked.
        for dim in spec.dims() {
            self.source.load(&dim.name, &mut store)?;
        }
        for fact in spec.facts() {
            self.source.load(&fact.name, &mut store)?;
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
            }
            let wait = WAL_EVERY.saturating_sub(self.wal_read_at.elapsed());
            if let Some(commit) = self.source.next_commit(wait)? {
                let lsn = commit.lsn;
                self.app.write().apply(commit).map_err(Error::Unfit)?;
                return Ok(lsn);
            }
        }
    }

    /// Reads the server's WAL, and fails once the slot retains more of it
    /// than [`Limits::max_retained_wal`].
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

/// Why a [`Writer::step`] failed; the next step starts over.
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
            Error::Unfit(table) => write!(formatter, "a change no longer fits {table}"),
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

/// Why an ask got no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerError {
    /// The tables are loading.
    NotReady,
    Invalid(Vec<AskError>),
}

impl fmt::Display for AnswerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AnswerError::NotReady => formatter.write_str("not ready"),
            AnswerError::Invalid(errors) => {
                for (index, error) in errors.iter().enumerate() {
                    if index > 0 {
                        writeln!(formatter)?;
                    }
                    write!(formatter, "{error}")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for AnswerError {}

#[cfg(test)]
mod tests {
    use std::time::{Instant, SystemTime};

    use straum_domain::{Ask, Datum, Identifier, Value};

    use super::{AnswerError, App, Change, Commit, Following, Limits, Lsn, Wal};
    use crate::lag::Lag;

    fn id(name: &str) -> Identifier {
        Identifier::new(name).unwrap()
    }

    const BYTES_PER_GIB: u64 = 1 << 30;

    fn spec() -> straum_domain::Spec {
        straum_notation::parse(
            "CREATE VIEW trips WITH (buckets = '1 hour') AS
             SELECT date_bin('1 hour', t.at, timestamptz 'epoch') AS bucket,
                    count(*) AS trips
             FROM public.trips AS t
             GROUP BY 1;",
        )
        .unwrap()
    }

    fn app() -> App {
        App::new(
            spec(),
            Limits {
                max_lag: std::time::Duration::from_secs(30),
                max_retained_wal: 4 * BYTES_PER_GIB,
            },
        )
    }

    fn follow(app: &App) {
        let mut state = app.write();
        state.following = Some(Following {
            applied: Lsn(1),
            committed: None,
            lag: Lag::new(Instant::now()),
            wal: Wal {
                written: Lsn(1),
                confirmed: Lsn(1),
                restart: Lsn(1),
            },
        });
    }

    fn count() -> Ask {
        Ask {
            view: id("trips"),
            measures: vec![id("trips")],
            keys: Vec::new(),
            filter: Vec::new(),
            having: Vec::new(),
            from: Some(0),
            to: Some(3_600),
        }
    }

    #[test]
    fn a_failed_apply_hides_the_commit_before_the_lock_drops() {
        let app = app();
        follow(&app);
        let mut state = app.write();
        let applied = state.apply(Commit {
            lsn: Lsn(2),
            committed: SystemTime::UNIX_EPOCH,
            changes: vec![
                Change::Insert {
                    table: id("trips"),
                    row: vec![Datum::Int(0)],
                },
                Change::Unfit { table: id("trips") },
            ],
        });
        assert!(applied.is_err());
        assert!(state.following.is_none());
        // The insert landed. Nothing can answer it: the stream is already clear.
        assert!(state.store.cells("trips").unwrap() > 0);
        drop(state);
        assert!(matches!(app.ask(&count()), Err(AnswerError::NotReady)));
    }

    #[test]
    fn a_commit_that_fits_stays_readable() {
        let app = app();
        follow(&app);
        {
            let mut state = app.write();
            state
                .apply(Commit {
                    lsn: Lsn(2),
                    committed: SystemTime::UNIX_EPOCH,
                    changes: vec![Change::Insert {
                        table: id("trips"),
                        row: vec![Datum::Int(0)],
                    }],
                })
                .unwrap();
            let following = state.following.as_ref().unwrap();
            assert_eq!(following.applied, Lsn(2));
        }
        let answered = app.ask(&count()).unwrap();
        assert_eq!(answered.applied, Lsn(2));
        assert_eq!(answered.answer.rows[0].values[0], Value::Int(1));
    }
}
