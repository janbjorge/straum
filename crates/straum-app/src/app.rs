//! The read side of Straum: one store, kept up to date by the `Writer`,
//! and read by asks through `App`. An ask takes the store's lock for
//! reading and answers from it, with how fresh the answer is. The
//! writer's progress, and so the status, go behind a lock of their own,
//! so they never wait on an ask.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};
use std::time::{Duration, Instant, SystemTime};

use straum_domain::{Answer, Ask, AskError, Cut, Spec, Store, Window, WindowAnswer};

use crate::lag::Lag;
use crate::source::{Lsn, Wal};

/// The store may be half updated, so nothing reads it again.
const POISONED: &str = "the writer panicked while it held the store";

const PROGRESS_POISONED: &str = "a thread panicked while it held the writer's progress";

/// The store's lock is taken before the progress's, never after, so
/// while an ask holds the store with a stream applied the progress
/// follows that stream.
const FOLLOWING: &str = "the progress follows a stream while the store has one applied";

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
    pub(crate) spec: Arc<Spec>,
    pub(crate) limits: Limits,
}

/// Behind the store's lock: the store, and the commit it holds up to.
pub(crate) struct State {
    pub(crate) store: Store,
    /// Every commit up to here is in the store. `None` until every table
    /// is loaded, and again after a failure.
    pub(crate) applied: Option<Lsn>,
}

/// What the writer did, beside the store.
pub(crate) struct Progress {
    /// `None` until every table is loaded, and again after a failure.
    pub(crate) following: Option<Following>,
}

/// Where a stream with every table loaded stands.
#[derive(Debug)]
pub(crate) struct Following {
    /// Every commit up to here is applied.
    pub(crate) applied: Lsn,
    /// When the commit at `applied` committed. `None` when `applied` is
    /// where the stream started.
    pub(crate) committed: Option<SystemTime>,
    /// How far behind the server the stream is.
    pub(crate) lag: Lag,
    /// The last reading of the server's WAL.
    pub(crate) wal: Wal,
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

impl App {
    /// An empty store that answers not ready until a [`Writer`](crate::Writer) has loaded
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
        let Some(following) = &progress.following else {
            return Status {
                ready: false,
                applied: None,
                committed: None,
                as_of: None,
                lag: None,
                wal: None,
                limits: self.limits,
            };
        };
        let lag = following.lag.behind(Instant::now());
        Status {
            ready: lag <= self.limits.max_lag,
            applied: Some(following.applied),
            committed: following.committed,
            as_of: Some(following.as_of()),
            lag: Some(lag),
            wal: Some(following.wal),
            limits: self.limits,
        }
    }

    pub(crate) fn read(&self) -> RwLockReadGuard<'_, State> {
        self.state.read().expect(POISONED)
    }

    pub(crate) fn write(&self) -> RwLockWriteGuard<'_, State> {
        self.state.write().expect(POISONED)
    }

    pub(crate) fn progress(&self) -> MutexGuard<'_, Progress> {
        self.progress.lock().expect(PROGRESS_POISONED)
    }
}

impl Following {
    /// As [`Answered::as_of`].
    fn as_of(&self) -> SystemTime {
        let since = self.lag.behind(Instant::now());
        // A clock set before 1970 gets the earliest time there is, which is
        // still true.
        (SystemTime::now().checked_sub(since)).unwrap_or(SystemTime::UNIX_EPOCH)
    }
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
pub(crate) mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use straum_domain::{Ask, Cut, Identifier, Start, Window};

    use super::{App, Following, Limits, Lsn, Wal};
    use crate::lag::Lag;

    pub(crate) fn id(name: &str) -> Identifier {
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

    pub(crate) fn app() -> App {
        App::new(
            spec(),
            Limits {
                max_lag: std::time::Duration::from_secs(30),
                max_retained_wal: 4 * BYTES_PER_GIB,
            },
        )
    }

    pub(crate) fn follow(app: &App) {
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

    pub(crate) fn count() -> Ask {
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

    /// How long a call here may take before it counts as waiting on the
    /// store's lock.
    pub(crate) const WAITING: Duration = Duration::from_millis(500);

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
