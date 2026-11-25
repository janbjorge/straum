//! [`Postgres`], the [`Source`] over PostgreSQL, where straum-pg meets
//! `straum-app`. Logical: a temporary replication slot (PostgreSQL's
//! bookmark in its WAL) exports its snapshot, a plain connection takes it
//! up for the loads, then the slot's connection streams from its
//! consistent point while they run. Physical: a temporary physical slot
//! streams from the last redo point while the loads run in a snapshot
//! taken once every later commit is whole in what it read. Either way the
//! stream's thread reads beside the loads, and each load checks it has not
//! failed. A plain connection on the side reads the server's WAL position.

use std::mem;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use postgres_protocol::escape::{escape_identifier, escape_literal};
use straum_app::{Commit, Estimate, LoadCounts, Lsn, Opened, Source, SourceError, Wal};
use straum_domain::{Identifier, Spec, Store};

use crate::catalog::{self, Checked, ColumnVersions, Followed, Tables};
use crate::decode::Decoder;
use crate::dsn::Dsn;
use crate::error::Error;
use crate::load;
use crate::physical::{self, Physical};
use crate::problem::{Problem, TableProblem, WaitProblem};
use crate::stream::{Reader, Stream};
use crate::unexpected::Unexpected;
use crate::wal::pipeline::{Pipeline, Sight};
use crate::wal::record_end;
use crate::wal::transactions::{Consistency, TransactionSnapshot};
use crate::watchdog::{SlotGuard, UNWATCHED, Watching};
use crate::wire::{Connection, ConnectionKind, first_row_value, plain_connection, timed_out};

/// Which WAL Straum streams: logical decoding's, through a publication,
/// or the physical WAL, which `wal_level = replica` writes (ADR-0008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replication {
    /// Logical decoding's changes, through the publication.
    Logical,
    /// Past `max_page_memory` bytes of rows held, Straum stops.
    Physical { max_page_memory: usize },
}

/// How long a physical stream waits for PostgreSQL to log which
/// transactions run, which the background writer does every 15 seconds
/// while the WAL moves, and `pg_log_standby_snapshot()` at once.
const CONSISTENT_WAIT: Duration = Duration::from_secs(60);
/// Where a logical slot's WAL counts from for the watchdog: all of it,
/// as the slot keeps WAL only from its consistent point on.
const LOGICAL_FROM: Lsn = Lsn(0);
/// How long a reading of the WAL position waits for its answer: as long
/// as the watchdog gives the slot's.
pub(crate) const WAL_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a physical start waits before it takes the load's snapshot
/// again.
const SNAPSHOT_AGAIN: Duration = Duration::from_millis(20);
/// How long a start waits for a lock on a spec table as it reads the
/// catalog and holds the tables. A lock request queues behind a waiting
/// `ALTER TABLE`, which may itself wait on a long transaction.
const LOCK_WAIT: Duration = Duration::from_secs(5);
/// The SQLSTATE of a statement that gave up at `lock_timeout`.
const LOCK_NOT_AVAILABLE: &str = "55P03";

/// What a catalog read under the lock timeout goes on to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AfterRead {
    Nothing,
    /// Hold the tables until the snapshot's transaction ends.
    HoldTables,
}

/// Slots made by this process, for unique names.
static SLOTS: AtomicU64 = AtomicU64::new(0);

/// Straum's [`Source`] over PostgreSQL 17 and newer with
/// `wal_level = logical`, or `replica` from 19 (ADR-0007).
pub struct Postgres {
    pub(crate) settings: Settings,
    /// Keeps the slot from holding more WAL than `--max-retained-wal`,
    /// whatever the stream and the writer are doing.
    pub(crate) guard: SlotGuard,
    state: State,
    /// The slot of the last [`Source::open`].
    slot: String,
    /// Where that slot's retained WAL counts from, as the watchdog counts.
    from: Lsn,
    /// The spec's tables as the last [`Source::open`] checked them, which
    /// each reading of the WAL checks are still those tables.
    followed: Followed,
    /// Reads the server's WAL, since the stream's connection is busy
    /// streaming. Connects again after an error.
    plain: Option<Connection>,
}

/// What every start reads PostgreSQL with, fixed when the source is made.
pub(crate) struct Settings {
    pub(crate) dsn: Dsn,
    pub(crate) publication: String,
    /// The most rows one transaction may change in the spec's tables; past
    /// it Straum stops, so a bulk update costs a load and not the
    /// write lock for all of it.
    pub(crate) max_transaction_rows: usize,
    pub(crate) replication: Replication,
    /// The bytes of commits the stream may hold while the tables load, and
    /// the bytes of rows one transaction may hold; past it Straum stops.
    pub(crate) max_stream_memory: usize,
    /// How long a start waits for its slot, and a physical start for a
    /// consistent point.
    pub(crate) consistent_wait: Duration,
}

impl Settings {
    /// Waits `CONSISTENT_WAIT` for a slot or a consistent point.
    pub(crate) fn new(
        dsn: Dsn,
        publication: &str,
        max_transaction_rows: usize,
        replication: Replication,
        max_stream_memory: usize,
    ) -> Settings {
        Settings {
            dsn,
            publication: publication.to_owned(),
            max_transaction_rows,
            replication,
            max_stream_memory,
            consistent_wait: CONSISTENT_WAIT,
        }
    }
}

/// Where the source is between `open` and the stream.
enum State {
    Closed,
    /// Every dim and fact loads in the snapshot of the slot the stream
    /// starts from. Boxed: it outweighs the other states many times.
    Loading(Box<Snapshot>),
    Streaming(Stream),
}

/// A plain connection in a read-only, repeatable-read transaction that
/// sees the database as of where the stream starts, beside the stream
/// already reading.
struct Snapshot {
    connection: Connection,
    slot: String,
    /// Where the slot's retained WAL counts from, as the watchdog counts.
    from: Lsn,
    at: Lsn,
    /// The server's clock as the transaction began.
    taken: SystemTime,
    tables: Tables,
    /// The versions of the columns read, as the snapshot sees them.
    versions: Vec<ColumnVersions>,
    /// Tables not loaded yet. The transaction ends with the last load, so
    /// it pins xmin and blocks DDL on the tables no longer than it must.
    unloaded: Vec<Identifier>,
    /// The stream, which reads on while the tables load.
    stream: Stream,
}

impl Postgres {
    /// Nothing connects until [`Source::open`].
    pub fn new(
        dsn: Dsn,
        publication: &str,
        max_transaction_rows: usize,
        max_retained_wal: u64,
        replication: Replication,
        max_stream_memory: usize,
    ) -> Postgres {
        let settings = Settings::new(
            dsn,
            publication,
            max_transaction_rows,
            replication,
            max_stream_memory,
        );
        Postgres {
            settings,
            guard: SlotGuard::new(Watching {
                max_retained_wal,
                unwatched: UNWATCHED,
            }),
            state: State::Closed,
            slot: String::new(),
            from: LOGICAL_FROM,
            followed: Followed::default(),
            plain: None,
        }
    }
}

/// A new temporary logical slot streaming from its consistent point, and a
/// transaction on a plain connection in the snapshot the slot exported.
fn snapshot(settings: &Settings, spec: &Spec, guard: &mut SlotGuard) -> Result<Snapshot, Error> {
    let dsn = &settings.dsn;
    let wait = settings.consistent_wait;
    let mut replication = Connection::connect(dsn, ConnectionKind::Replication)?;
    let slot = slot_name();
    // PostgreSQL makes the slot once every transaction that writes and
    // was open when it began has ended: at most `wait`, then the start
    // names them, as a physical start does.
    replication.set_read_timeout(wait)?;
    let created = replication.rows(&format!(
        "CREATE_REPLICATION_SLOT {slot} TEMPORARY LOGICAL pgoutput (SNAPSHOT 'export')"
    ));
    if let Err(Error::Io(error)) = &created
        && timed_out(error)
    {
        return Err(waited_for(wait.as_secs(), slot_wait(dsn, &slot)?));
    }
    replication.clear_read_timeout()?;
    let created = match created {
        Ok(created) => created,
        // Without the wal_level logical decoding needs, PostgreSQL's error
        // names no fix; the setup check says what to change.
        Err(error) => {
            read_in_time(&mut replication, spec, settings, AfterRead::Nothing)?;
            return Err(error);
        }
    };
    let at = (first_row_value(&created, 1).and_then(parse_lsn))
        .ok_or(Error::Protocol(Unexpected::SlotWithoutConsistentPoint))?;
    let exported =
        first_row_value(&created, 2).ok_or(Error::Protocol(Unexpected::SlotWithoutSnapshot))?;
    let mut connection = Connection::connect(dsn, ConnectionKind::Sql)?;
    let closers = vec![replication.closer()?, connection.closer()?];
    guard.watch(dsn, &slot, LOGICAL_FROM, closers)?;
    begin_in_snapshot(&mut connection, exported)?;
    let taken = snapshot_time(&mut connection)?;
    let tables = read_in_time(&mut connection, spec, settings, AfterRead::HoldTables)?;
    let versions = catalog::column_versions(&mut connection, &checked(&tables))?;
    // After the import: the slot's connection keeps its snapshot only
    // until its next command.
    let names = replication_literal(&escape_identifier(&settings.publication));
    replication.copy_both(&format!(
        "START_REPLICATION SLOT {slot} LOGICAL {at} (proto_version '1', publication_names {names})"
    ))?;
    let reader = Decoder::new(
        checked(&tables),
        settings.max_transaction_rows,
        settings.max_stream_memory,
    );
    let reader = Reader::Logical(reader);
    let stream = Stream::start(replication, reader, at, settings.max_stream_memory)?;
    let unloaded = unloaded(&tables);
    Ok(Snapshot {
        connection,
        slot,
        from: LOGICAL_FROM,
        at,
        taken,
        tables,
        versions,
        unloaded,
        stream,
    })
}

/// The server's clock as the load's transaction on `connection` began:
/// `now()` holds still through a transaction, and reads the clock each
/// commit's time is stamped by.
fn snapshot_time(connection: &mut Connection) -> Result<SystemTime, Error> {
    let rows = connection.rows("SELECT floor(extract(epoch FROM now()))::int8")?;
    let Some(Ok(seconds)) = first_row_value(&rows, 0).map(str::parse::<i64>) else {
        return Err(Error::Protocol(Unexpected::SnapshotShape));
    };
    let since = Duration::from_secs(seconds.unsigned_abs());
    let taken = if seconds < 0 {
        UNIX_EPOCH.checked_sub(since)
    } else {
        UNIX_EPOCH.checked_add(since)
    };
    taken.ok_or(Error::Protocol(Unexpected::SnapshotShape))
}

/// Begins the load's transaction on `connection` in the snapshot a slot
/// exported as `exported`.
fn begin_in_snapshot(connection: &mut Connection, exported: &str) -> Result<(), Error> {
    connection.execute("BEGIN READ ONLY ISOLATION LEVEL REPEATABLE READ")?;
    connection.execute(&format!(
        "SET TRANSACTION SNAPSHOT {}",
        escape_literal(exported)
    ))
}

/// A physical stream reading from the last redo point, and a transaction
/// on a plain connection whose snapshot is taken once every transaction
/// committing after it is whole in the WAL the stream read (ADR-0008).
fn physical_snapshot(
    settings: &Settings,
    spec: &Spec,
    pages: PageLimits,
    guard: &mut SlotGuard,
) -> Result<Snapshot, Error> {
    let dsn = &settings.dsn;
    let wait = settings.consistent_wait;
    let mut connection = Connection::connect(dsn, ConnectionKind::Sql)?;
    let tables = read_in_time(&mut connection, spec, settings, AfterRead::Nothing)?;
    let started = start_physical_stream(settings, &mut connection, checked(&tables), pages, guard)?;
    let PhysicalStream {
        mut stream,
        slot,
        from,
    } = started;
    // Logs which transactions run now, rather than at the next checkpoint
    // or background writer round. Superusers, and roles granted it, may.
    let _ = connection.execute("SELECT pg_log_standby_snapshot()");
    wait_consistent(&mut stream, wait)?;
    let at = seen_snapshot(&mut connection, &mut stream, wait)?;
    let taken = snapshot_time(&mut connection)?;
    let tables = read_in_time(&mut connection, spec, settings, AfterRead::HoldTables)?;
    let unloaded = unloaded(&tables);
    // The physical stream reads a rewrite in the WAL and stops.
    Ok(Snapshot {
        connection,
        slot,
        from,
        at,
        taken,
        tables,
        versions: Vec::new(),
        unloaded,
        stream,
    })
}

/// What bounds a physical stream's rows: the row limit of a commit, and
/// the bytes of rows its pages may hold.
#[derive(Debug, Clone, Copy)]
struct PageLimits {
    limit: usize,
    max_page_memory: usize,
}

/// A physical stream started, the name of its slot, and where the slot's
/// retained WAL counts from.
struct PhysicalStream {
    stream: Stream,
    slot: String,
    from: Lsn,
}

/// Starts the physical stream from the last redo point for `checked`,
/// whose layouts it reads on `connection`, and has the watchdog close it
/// and `connection` once the slot holds too much WAL.
fn start_physical_stream(
    settings: &Settings,
    connection: &mut Connection,
    checked: Vec<Checked>,
    pages: PageLimits,
    guard: &mut SlotGuard,
) -> Result<PhysicalStream, Error> {
    // After the slot: a rewrite between the two would be in neither the
    // layouts nor the WAL read.
    let dsn = &settings.dsn;
    let (stream, start) = Physical::start(dsn)?;
    let from = stream.made();
    // The stream thread reads layouts on this one after DDL: a server
    // that falls silent there must not hold it, nor a stop that joins it.
    let layouts_connection = Connection::connect(dsn, ConnectionKind::Sql)?;
    layouts_connection.set_read_timeout(physical::DDL_WAIT)?;
    let layouts_closer = layouts_connection.closer()?;
    guard.watch(
        dsn,
        stream.slot(),
        from,
        vec![
            stream.closer()?,
            connection.closer()?,
            layouts_connection.closer()?,
        ],
    )?;
    let layouts = physical::read_layouts(connection, &checked)?;
    let slot = stream.slot().to_owned();
    let pipeline = Pipeline::new(
        checked.clone(),
        layouts,
        stream.major(),
        pages.limit,
        pages.max_page_memory,
        settings.max_stream_memory,
    );
    let (replication, replay) = stream.into_replay(pipeline, layouts_connection, checked);
    let reader = Reader::Physical(Box::new(replay));
    let mut stream = Stream::start(replication, reader, start, settings.max_stream_memory)?;
    stream.close_too(layouts_closer);
    Ok(PhysicalStream { stream, slot, from })
}

/// Begins the load's transaction on `connection` once its snapshot sees
/// every commit `stream` read before it, waiting at most `wait`, and
/// returns where the stream's commits after the snapshot begin.
fn seen_snapshot(
    connection: &mut Connection,
    stream: &mut Stream,
    wait: Duration,
) -> Result<Lsn, Error> {
    // PostgreSQL logs a commit before snapshots see it, and waits for
    // synchronous standbys in between: a snapshot blind to one the stream
    // read as before its start would lose it, so another is taken.
    let deadline = Instant::now() + wait;
    loop {
        connection.execute("BEGIN READ ONLY ISOLATION LEVEL REPEATABLE READ")?;
        let taken = connection.rows("SELECT pg_current_snapshot(), pg_current_wal_insert_lsn()")?;
        let seen = (first_row_value(&taken, 0).and_then(TransactionSnapshot::parse))
            .ok_or(Error::Protocol(Unexpected::SnapshotShape))?;
        let written = (first_row_value(&taken, 1).and_then(parse_lsn))
            .ok_or(Error::Protocol(Unexpected::NoWalPosition))?;
        let xids = match stream.see(seen, written)? {
            Sight::Through(at) => return Ok(at),
            Sight::Blind(xids) => xids,
        };
        connection.execute("ROLLBACK")?;
        if Instant::now() >= deadline {
            let problem = WaitProblem::InvisibleCommits(xids);
            return Err(waited_for(wait.as_secs(), problem));
        }
        thread::sleep(SNAPSHOT_AGAIN);
    }
}

/// Reads the catalog, and holds the tables when `after` says so, waiting
/// at most `LOCK_WAIT` for each lock: `pg_publication_tables` locks each
/// table it reads. Later statements wait on no lock timeout.
pub(crate) fn read_in_time(
    connection: &mut Connection,
    spec: &Spec,
    settings: &Settings,
    after: AfterRead,
) -> Result<Tables, Error> {
    connection.execute(&format!("SET lock_timeout = '{}s'", LOCK_WAIT.as_secs()))?;
    let read = catalog::read(
        connection,
        spec,
        &settings.publication,
        settings.replication,
    );
    let tables = match read {
        Ok(tables) => tables,
        Err(error) => return Err(name_locked(error, connection, spec)),
    };
    if after == AfterRead::HoldTables
        && let Err(error) = hold_tables(connection, &tables)
    {
        return Err(name_locked(error, connection, spec));
    }
    connection.execute("SET lock_timeout = 0")?;
    Ok(tables)
}

/// `error`, or, when it gave up at the lock timeout, the spec tables
/// another session locks against a read.
fn name_locked(error: Error, connection: &mut Connection, spec: &Spec) -> Error {
    let Error::Server { code, .. } = &error else {
        return error;
    };
    if code != LOCK_NOT_AVAILABLE {
        return error;
    }
    // The failed statement ended any transaction; the locks read outside.
    if connection.execute("ROLLBACK").is_err() {
        return error;
    }
    let Ok(tables) = catalog::locked_tables(connection, spec) else {
        return error;
    };
    if tables.is_empty() {
        return error;
    }
    waited_for(LOCK_WAIT.as_secs(), WaitProblem::Locked(tables))
}

/// Holds every spec table against a rewrite until the snapshot's
/// transaction ends with the last load, and fails when one was rewritten
/// since the snapshot was taken. A rewrite gives its rows the xid of the
/// transaction that rewrote them, so the snapshot sees the table empty,
/// and the logical stream sends nothing of it.
pub(crate) fn hold_tables(connection: &mut Connection, tables: &Tables) -> Result<(), Error> {
    let mut names = Vec::new();
    let mut oids = Vec::new();
    for table in tables.facts.iter().chain(&tables.dims) {
        names.push(catalog::escaped(&table.schema, &table.table_name));
        oids.push(table.oid.to_string());
    }
    if names.is_empty() {
        return Ok(());
    }
    connection.execute(&format!(
        "LOCK TABLE {} IN ACCESS SHARE MODE",
        names.join(", ")
    ))?;
    // The snapshot reads `pg_class` as it was; the function reads the
    // file the table has now, the lock having taken in what changed.
    let rewritten = connection.rows(&format!(
        "SELECT c.oid FROM pg_class c
          WHERE c.oid IN ({}) AND c.relfilenode <> pg_relation_filenode(c.oid)",
        oids.join(", ")
    ))?;
    let mut problems = Vec::new();
    for table in tables.facts.iter().chain(&tables.dims) {
        let oid = table.oid.to_string();
        for row in &rewritten {
            if row.first().and_then(Option::as_deref) == Some(oid.as_str()) {
                problems.push(table.names().table_problem(TableProblem::Rewritten));
            }
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(Error::Setup(problems))
}

/// What the logical slot `slot` waited for in vain: the transactions open
/// with an xid, prepared ones included. Ends the session of the slot.
fn slot_wait(dsn: &Dsn, slot: &str) -> Result<WaitProblem, Error> {
    let mut connection = Connection::connect(dsn, ConnectionKind::Sql)?;
    let rows = connection.rows(
        "SELECT backend_xid::text FROM pg_stat_activity WHERE backend_xid IS NOT NULL
         UNION SELECT transaction::text FROM pg_prepared_xacts",
    )?;
    // The session of the slot waits for those transactions and does not
    // see that Straum closed its socket. Until they end, it uses one of
    // the max_wal_senders.
    let _ = connection.execute(&format!(
        "SELECT pg_terminate_backend(active_pid) FROM pg_replication_slots
          WHERE slot_name = {} AND active_pid IS NOT NULL",
        escape_literal(slot)
    ));
    let mut xids = Vec::new();
    for row in rows {
        let Some(Some(text)) = row.into_iter().next() else {
            continue;
        };
        if let Ok(xid) = text.parse() {
            xids.push(xid);
        }
    }
    xids.sort_unstable();
    Ok(WaitProblem::SlotCreation(xids))
}

/// Waits at most `wait` for the physical stream to be consistent; past
/// it, says what holds it back.
fn wait_consistent(stream: &mut Stream, wait: Duration) -> Result<(), Error> {
    let waited = wait.as_secs();
    match stream.wait_consistent(wait)? {
        Consistency::Consistent => Ok(()),
        Consistency::Unknown => Err(waited_for(waited, WaitProblem::NotLogged)),
        Consistency::Waiting(xids) => Err(waited_for(waited, WaitProblem::OpenTransactions(xids))),
    }
}

/// The setup error of a start that waited `waited_seconds` in vain.
pub(crate) fn waited_for(waited_seconds: u64, problem: WaitProblem) -> Error {
    Error::Setup(vec![Problem::Wait {
        waited_seconds,
        problem,
    }])
}

fn unloaded(tables: &Tables) -> Vec<Identifier> {
    let mut names = Vec::new();
    for table in tables.dims.iter().chain(&tables.facts) {
        names.push(table.name.clone());
    }
    names
}

/// Every table of `tables`, the facts then the dims, as the decoder takes
/// them.
fn checked(tables: &Tables) -> Vec<Checked> {
    let mut checked = tables.facts.clone();
    checked.extend(tables.dims.iter().cloned());
    checked
}

/// A slot name no other Straum process or slot of this one has.
pub(crate) fn slot_name() -> String {
    // A clock before 1970 still names a slot; the counter keeps it unique.
    let since = SystemTime::now().duration_since(UNIX_EPOCH);
    let nanos = since.unwrap_or_default().as_nanos();
    format!(
        "straum_{}_{nanos}_{}",
        process::id(),
        SLOTS.fetch_add(1, Ordering::Relaxed)
    )
}

/// The plain connection the WAL is read on, made on first use and after a
/// failure.
// 'c: the connection returned is the one held in `plain`.
pub(crate) fn plain<'c>(
    plain: &'c mut Option<Connection>,
    dsn: &Dsn,
) -> Result<&'c mut Connection, Error> {
    // A server that stops answering must fail the reading, so the lag it
    // no longer moves can grow.
    plain_connection(plain, dsn, WAL_READ_TIMEOUT)
}

impl Source for Postgres {
    fn open(&mut self, spec: &Spec) -> Result<Opened, SourceError> {
        // The old stream goes first, and its slot with it.
        self.guard.stop();
        self.state = State::Closed;
        self.plain = None;
        let settings = &self.settings;
        let opened = match settings.replication {
            Replication::Logical => snapshot(settings, spec, &mut self.guard),
            Replication::Physical { max_page_memory } => {
                let pages = PageLimits {
                    limit: settings.max_transaction_rows,
                    max_page_memory,
                };
                physical_snapshot(settings, spec, pages, &mut self.guard)
            }
        };
        let snapshot = match opened {
            Ok(snapshot) => snapshot,
            Err(error) => return Err(self.failed(error)),
        };
        let start = snapshot.at;
        let at = snapshot.taken;
        self.slot.clone_from(&snapshot.slot);
        self.from = snapshot.from;
        self.followed = Followed {
            tables: checked(&snapshot.tables),
            labels: snapshot.tables.labels.clone(),
            versions: snapshot.versions.clone(),
        };
        self.state = State::Loading(Box::new(snapshot));
        Ok(Opened { start, at })
    }

    /// Read in the snapshot's transaction, between the copies.
    fn estimate(&mut self, tables: &[Identifier]) -> Result<Estimate, SourceError> {
        let State::Loading(snapshot) = &mut self.state else {
            return Err(Error::Stopped.into());
        };
        let Some(first) = tables.first() else {
            return Ok(Estimate {
                rows: Some(0),
                bytes: 0,
            });
        };
        let table = snapshot.tables.get(first);
        match load::estimate(&mut snapshot.connection, table) {
            Ok(estimate) => Ok(estimate),
            Err(error) => Err(self.failed(error)),
        }
    }

    fn load(
        &mut self,
        tables: &[Identifier],
        store: &mut Store,
        counts: &LoadCounts,
    ) -> Result<(), SourceError> {
        let State::Loading(snapshot) = &mut self.state else {
            return Err(Error::Stopped.into());
        };
        match load_tables(snapshot, tables, store, counts) {
            Ok(()) => Ok(()),
            Err(error) => Err(self.failed(error)),
        }
    }

    fn next_commit(&mut self, wait: Duration) -> Result<Option<Commit>, SourceError> {
        match self.next(wait) {
            Ok(commit) => Ok(commit),
            Err(error) => Err(self.failed(error)),
        }
    }

    fn wal(&mut self) -> Result<Wal, SourceError> {
        if matches!(self.state, State::Closed) {
            return Err(Error::Stopped.into());
        }
        // A reading's connection may die on its own, terminated or cut,
        // while the stream is whole: one more try, on a new connection,
        // before Straum stops for it.
        match self.read_wal_once() {
            Ok(wal) => Ok(wal),
            Err(_) => Ok(self.read_wal_once()?),
        }
    }
}

impl Postgres {
    /// Ends the snapshot or the stream after `error`, which starts Straum
    /// over: kept through the backoff, a snapshot would hold its tables'
    /// locks and the vacuum horizon, and the slot its WAL, up to a minute.
    fn failed(&mut self, error: Error) -> SourceError {
        let error = self.guard.blame(error);
        self.guard.stop();
        self.state = State::Closed;
        error.into()
    }

    /// One reading of the WAL, and a check that the spec's tables are
    /// still the ones loaded and published; its connection goes after a
    /// failure.
    fn read_wal_once(&mut self) -> Result<Wal, Error> {
        // Taken before the server's position, so the lag a reading gives
        // is never shorter than it is.
        let applied = match &self.state {
            State::Loading(snapshot) => snapshot.stream.caught_up(),
            State::Streaming(stream) => stream.caught_up(),
            State::Closed => return Err(Error::Stopped),
        };
        let wal = match plain(&mut self.plain, &self.settings.dsn) {
            Ok(connection) => read_wal_and_check(
                connection,
                &self.slot,
                self.from,
                applied,
                &self.followed,
                &self.settings,
            ),
            Err(error) => Err(error),
        };
        if wal.is_err() {
            self.plain = None;
        }
        wal
    }
}

impl Postgres {
    /// The next commit, streaming once the last table is loaded.
    fn next(&mut self, wait: Duration) -> Result<Option<Commit>, Error> {
        self.state = match mem::replace(&mut self.state, State::Closed) {
            State::Loading(snapshot) => State::Streaming(stream(*snapshot)?),
            other => other,
        };
        let State::Streaming(stream) = &mut self.state else {
            return Err(Error::Stopped);
        };
        stream.next(wait)
    }
}

/// Loads `tables`, which read one table of PostgreSQL, in the snapshot,
/// with one copy of it.
fn load_tables(
    snapshot: &mut Snapshot,
    tables: &[Identifier],
    store: &mut Store,
    counts: &LoadCounts,
) -> Result<(), Error> {
    let mut checked = Vec::new();
    for table in tables {
        checked.push(snapshot.tables.get(table));
    }
    load::tables(
        &mut snapshot.connection,
        &checked,
        store,
        &mut snapshot.stream,
        counts,
    )?;
    loaded(snapshot, tables)
}

/// The tables are loaded; the snapshot's transaction ends with the last
/// one.
fn loaded(snapshot: &mut Snapshot, tables: &[Identifier]) -> Result<(), Error> {
    snapshot
        .unloaded
        .retain(|unloaded| !tables.contains(unloaded));
    if snapshot.unloaded.is_empty() {
        snapshot.connection.execute("COMMIT")?;
    }
    Ok(())
}

/// The server's WAL position, then a check that the spec's tables are
/// still followed.
fn read_wal_and_check(
    connection: &mut Connection,
    slot: &str,
    from: Lsn,
    applied: Lsn,
    followed: &Followed,
    settings: &Settings,
) -> Result<Wal, Error> {
    let wal = read_wal(connection, slot, from, applied)?;
    let publication = &settings.publication;
    catalog::still_followed(connection, followed, publication, settings.replication)?;
    Ok(wal)
}

/// The server's WAL position and where the slot's retained WAL counts
/// from, never before `from`, beside `applied`, how far the stream has
/// applied.
pub(crate) fn read_wal(
    connection: &mut Connection,
    slot: &str,
    from: Lsn,
    applied: Lsn,
) -> Result<Wal, Error> {
    // The insert position, not the write position: a commit is
    // acknowledged once inserted under synchronous_commit = off, and
    // as_of must not lead it (ADR-0005).
    let rows = connection.rows(&format!(
        "SELECT pg_current_wal_insert_lsn(), restart_lsn, \
                (SELECT setting FROM pg_settings WHERE name = 'wal_segment_size') \
         FROM pg_replication_slots WHERE slot_name = {}",
        escape_literal(slot)
    ))?;
    let Some(row) = rows.first() else {
        return Err(Error::SlotGone(slot.to_owned()));
    };
    let [inserted, restart, segment_bytes] = row.as_slice() else {
        return Err(no_wal_position(slot));
    };
    let (Some(inserted), Some(restart), Some(segment_bytes)) = (
        inserted.as_deref().and_then(parse_lsn),
        restart.as_deref().and_then(parse_lsn),
        whole_number(segment_bytes.as_deref()),
    ) else {
        return Err(no_wal_position(slot));
    };
    Ok(Wal {
        written: record_end(inserted, segment_bytes),
        applied,
        // A physical slot keeps WAL from a redo point before it was made,
        // which the server recycles at its next checkpoint anyway.
        restart: restart.max(from),
    })
}

/// `text` as a string in a replication command: in quotes, each quote
/// doubled. The grammar of a replication command does not accept the
/// `E'...'` form that `escape_literal` gives for a backslash.
fn replication_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn no_wal_position(slot: &str) -> Error {
    Error::Protocol(Unexpected::NoWalPositionForSlot(slot.to_owned()))
}

/// A column's whole number, if it holds one.
fn whole_number(value: Option<&str>) -> Option<u64> {
    value?.parse().ok()
}

/// The stream, its snapshot's transaction ended. Tables not loaded yet
/// can't load any more.
fn stream(mut snapshot: Snapshot) -> Result<Stream, Error> {
    if !snapshot.unloaded.is_empty() {
        snapshot.connection.execute("COMMIT")?;
    }
    Ok(snapshot.stream)
}

/// `16/B374D848`, as PostgreSQL prints an LSN.
pub(crate) fn parse_lsn(text: &str) -> Option<Lsn> {
    let (high, low) = text.split_once('/')?;
    let high = u32::from_str_radix(high, 16).ok()?;
    let low = u32::from_str_radix(low, 16).ok()?;
    Some(Lsn(u64::from(high) << 32 | u64::from(low)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_lsns() {
        assert_eq!(parse_lsn("16/B374D848"), Some(Lsn(0x16_B374_D848)));
        assert_eq!(parse_lsn("0/0"), Some(Lsn(0)));
        assert_eq!(parse_lsn("FFFFFFFF/FFFFFFFF"), Some(Lsn(u64::MAX)));
        for bad in ["", "16", "16/", "/1", "1/2/3", "G/0", "100000000/0"] {
            assert_eq!(parse_lsn(bad), None, "{bad}");
        }
        let lsn = Lsn(0x16_B374_D848);
        assert_eq!(parse_lsn(&lsn.to_string()), Some(lsn));
    }
}
