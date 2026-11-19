//! The heart of Straum: one store, filled and kept up to date by the
//! `Writer`, and read by asks through `App`. The writer loads every table
//! from the `Source`, then applies one commit at a time; it takes the
//! store's lock only to apply what it has read, never while it waits on
//! PostgreSQL, so asks never wait on the database. Its readings of the
//! server's WAL, and the status, go behind a lock of their own, so they
//! never wait on an ask.

use std::fmt;
use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant, SystemTime};

use straum_domain::{
    Answer, Ask, AskError, AskKey, ColumnType, Cut, Fact, Identifier, RowError, Spec, Store, Table,
    Width, Window, WindowAnswer,
};

use crate::lag::Lag;
use crate::source::{Change, Commit, Lsn, Source, SourceError, Wal};

/// The store may be half updated, so nothing reads it again.
const POISONED: &str = "the writer panicked while it held the store";

const PROGRESS_POISONED: &str = "a thread panicked while it held the writer's progress";

/// The store's lock is taken before the progress's, never after, so
/// while an ask holds the store with a stream applied the progress
/// follows that stream.
const FOLLOWING: &str = "the progress follows a stream while the store has one applied";

const IN_SPEC: &str = "the store holds every view and dim of its spec";

const STARTED: &str = "the writer applies commits only to a started stream";

const FAILED: &str = "a writer that failed takes no more steps: start a new one";

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
    /// Straum stops, so a Straum that can't keep up doesn't fill the
    /// server's disk.
    pub max_retained_wal: u64,
}

/// The store as the writer last left it, for asks. Clones share it.
#[derive(Clone)]
pub struct App {
    state: Arc<RwLock<State>>,
    /// Behind a lock of its own, so the status and the writer's readings
    /// of the WAL never wait on the store's: a long ask holds it for
    /// reading, and a writer waiting for it holds back every new reader.
    progress: Arc<Mutex<Progress>>,
    /// The store's spec, which never changes, read without its lock.
    spec: Arc<Spec>,
    limits: Limits,
}

/// Behind the store's lock: the store, and the commit it holds up to.
struct State {
    store: Store,
    /// Every commit up to here is in the store. `None` until every table
    /// is loaded, and again after a failure.
    applied: Option<Lsn>,
}

/// What the writer did, beside the store.
struct Progress {
    /// `None` until every table is loaded, and again after a failure.
    following: Option<Following>,
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

/// An ask's answer, and how fresh it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answered {
    pub answer: Answer,
    /// Where the answer's rows lie among all it has: every one, unless
    /// asked for a window of them.
    pub cut: Cut,
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
    /// The view's time, by the name asks group and bound it by.
    pub time: HeldKey,
    /// Every name an ask may group or filter by, the time aside.
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
            spec: Arc::new(spec.clone()),
            state: Arc::new(RwLock::new(State {
                store: Store::new(spec),
                applied: None,
            })),
            progress: Arc::new(Mutex::new(Progress { following: None })),
            limits,
        }
    }

    /// Answers once every table is loaded.
    pub fn ask(&self, ask: &Ask) -> Result<Answered, AnswerError> {
        self.answer(ask, None)
    }

    /// As [`App::ask`], building only the rows `window` shows.
    pub fn ask_window(&self, ask: &Ask, window: Window) -> Result<Answered, AnswerError> {
        self.answer(ask, Some(window))
    }

    fn answer(&self, ask: &Ask, window: Option<Window>) -> Result<Answered, AnswerError> {
        let state = self.read();
        // An ask the spec refuses is refused even while loading; only a
        // load knows the types the store checks.
        (state.store.spec().check_ask(ask)).map_err(AnswerError::Invalid)?;
        let applied = state.applied.ok_or(AnswerError::NotReady)?;
        let WindowAnswer { answer, cut } =
            (state.store.ask_window(ask, window)).map_err(AnswerError::Invalid)?;
        // The slot never confirms past what the store holds, so the time
        // the progress was last caught up is never ahead of the answer.
        let as_of = self.progress().following.as_ref().expect(FOLLOWING).as_of();
        Ok(Answered {
            answer,
            cut,
            applied,
            as_of,
        })
    }

    /// How Straum stands now: loaded or not, how far behind.
    pub fn status(&self) -> Status {
        let progress = self.progress();
        let mut status = Status {
            ready: false,
            applied: None,
            committed: None,
            as_of: None,
            lag: None,
            wal: None,
            limits: self.limits,
        };
        if let Some(following) = &progress.following {
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
        state.applied?;
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

    /// The ask for every row the view holds: each measure by the time and
    /// every key, both ends of the range open.
    /// `None` for a name no view has.
    pub fn rows_ask(&self, view: &str) -> Option<Ask> {
        let fact = self.spec.fact(view)?;
        let mut keys = vec![AskKey::Key(fact.time.name.clone())];
        for key in fact.keys() {
            keys.push(AskKey::Key(key.clone()));
        }
        Some(Ask {
            view: fact.name.clone(),
            measures: measure_names(fact),
            keys,
            filter: Vec::new(),
            having: Vec::new(),
            from: None,
            to: None,
            bounded: None,
        })
    }

    fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().expect(POISONED)
    }

    fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().expect(POISONED)
    }

    fn progress(&self) -> MutexGuard<'_, Progress> {
        self.progress.lock().expect(PROGRESS_POISONED)
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
        time: HeldKey {
            name: fact.time.name.clone(),
            column_type: store.time_type(name).expect(IN_SPEC),
        },
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
        Change::Unfit { .. } => return Err(Unfitting::Altered),
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
        let spec = self.app.read().store.spec().clone();
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
            self.load(&dim.name, "dim", &mut store)?;
        }
        for fact in spec.facts() {
            self.load(&fact.name, "view", &mut store)?;
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

    /// Loads one table, saying when it is done and how long it took.
    fn load(&mut self, name: &Identifier, kind: &str, store: &mut Store) -> Result<(), Error> {
        let clock = Instant::now();
        self.source.load(name, store)?;
        let seconds = clock.elapsed().as_secs_f64();
        (self.report)(&format!("loaded {kind} {name} in {seconds:.1} s"));
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
    /// [`Limits::max_retained_wal`], in bytes.
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
    /// The stream no longer carries what Straum reads of the table: it was
    /// altered or rewritten, or a change came without its old row.
    Altered,
}

impl fmt::Display for Unfitting {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unfitting::Row(error) => error.fmt(formatter),
            Unfitting::Altered => formatter.write_str(
                "the stream no longer carries what Straum reads of it: the table was altered \
                 or rewritten, or a change came without its old row",
            ),
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
            // Their words name Straum SQL, so the adapter that knows it
            // says each (ADR-0002).
            AnswerError::Invalid(errors) => {
                write!(formatter, "errors in the ask: {}", errors.len())
            }
        }
    }
}

impl std::error::Error for AnswerError {}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant, SystemTime};

    use straum_domain::{Ask, Datum, Identifier, Spec, Store, Value};

    use straum_domain::{Cut, Start, Window};

    use super::{AnswerError, App, Change, Commit, Following, Limits, Lsn, Step, Wal, Writer};
    use crate::lag::Lag;
    use crate::source::{Source, SourceError};

    fn id(name: &str) -> Identifier {
        Identifier::new(name).unwrap()
    }

    const BYTES_PER_GIB: u64 = 1 << 30;

    fn spec() -> straum_domain::Spec {
        straum_notation::parse(
            "CREATE VIEW trips AS
             SELECT buckets(t.at, '1 hour') AS bucket,
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
        state.applied = Some(Lsn(1));
        app.progress().following = Some(Following {
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
            bounded: None,
        }
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
                Change::Unfit { table: id("trips") },
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

        fn load(&mut self, _table: &Identifier, _store: &mut Store) -> Result<(), SourceError> {
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
                confirmed: Lsn(1),
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

        fn load(&mut self, _table: &Identifier, _store: &mut Store) -> Result<(), SourceError> {
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

    /// How long a call here may take before it counts as waiting on the
    /// store's lock.
    const WAITING: Duration = Duration::from_millis(500);

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

    /// An ask does wait on the store's lock: the writer holds it to apply.
    #[test]
    fn an_ask_waits_while_the_writer_holds_the_store() {
        let app = app();
        follow(&app);
        let held = app.write();
        let (done, finished) = mpsc::channel();
        let asking = app.clone();
        let reader = thread::spawn(move || {
            done.send(asking.ask(&count()).is_ok()).unwrap();
        });
        assert!(finished.recv_timeout(WAITING).is_err());
        drop(held);
        assert_eq!(finished.recv_timeout(WAITING * 4), Ok(true));
        reader.join().unwrap();
    }

    /// A window of the answer builds its rows and says where they lie.
    #[test]
    fn a_window_of_an_answer_says_where_its_rows_lie() {
        let app = app();
        follow(&app);
        let window = Window {
            start: Start::FromLast(5),
            count: 1,
        };
        let answered = app.ask_window(&count(), window).unwrap();
        assert_eq!(answered.answer.rows.len(), 1);
        let cut = Cut {
            first: 0,
            count: 1,
            total: 1,
        };
        assert_eq!(answered.cut, cut);
        assert_eq!(app.ask(&count()).unwrap().cut, Cut::whole(1));
    }
}
