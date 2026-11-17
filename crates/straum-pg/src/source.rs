//! [`Postgres`], the [`Source`] over PostgreSQL, where straum-pg meets
//! `straum-app`. Logical: a temporary replication slot (PostgreSQL's
//! bookmark in its WAL), loads in its snapshot on the same connection, then
//! the stream from its consistent point. Physical: a temporary physical
//! slot streams from the last redo point while the loads run in a snapshot
//! taken once every later commit is whole in what it read. A plain
//! connection on the side reads the server's WAL position.

use std::mem;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use postgres_protocol::escape::{escape_identifier, escape_literal};
use straum_app::{Commit, Lsn, Source, SourceError, Wal};
use straum_domain::{Identifier, Spec, Store};

use crate::catalog::{self, Checked, EnumLabel, Followed, Tables};
use crate::decode::Decoder;
use crate::dsn::Dsn;
use crate::error::Error;
use crate::load;
use crate::physical::{self, Physical};
use crate::problem::{Problem, TableNames, TableProblem, WaitProblem};
use crate::stream::{Reader, Stream};
use crate::unexpected::Unexpected;
use crate::wal::pipeline::{Pipeline, Sight};
use crate::wal::record_end;
use crate::wal::transactions::{Consistency, TransactionSnapshot};
use crate::watchdog::{SlotGuard, UNWATCHED, Watching};
use crate::wire::{Connection, ConnectionKind, first_row_value, timed_out};

/// Which WAL Straum streams: logical decoding's, through a publication,
/// or the physical WAL, which `wal_level = replica` writes (ADR-0008).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replication {
    /// Logical decoding's changes, through the publication.
    Logical,
    /// Past `max_page_memory` bytes of rows held, Straum starts over.
    Physical { max_page_memory: usize },
}

/// How long a physical stream waits for PostgreSQL to log which
/// transactions run, which the background writer does every 15 seconds
/// while the WAL moves, and `pg_log_standby_snapshot()` at once.
const CONSISTENT_WAIT: Duration = Duration::from_secs(60);
/// How long a reading of the WAL position waits for its answer: as long
/// as the watchdog gives the slot's.
pub(crate) const WAL_READ_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a physical start waits before it takes the load's snapshot
/// again.
const SNAPSHOT_AGAIN: Duration = Duration::from_millis(20);

/// Slots made by this process, for unique names.
static SLOTS: AtomicU64 = AtomicU64::new(0);

/// Straum's [`Source`] over PostgreSQL 17 and newer with
/// `wal_level = logical`, or `replica` from 19 (ADR-0007).
pub struct Postgres {
    dsn: Dsn,
    publication: String,
    /// The most rows one transaction may change in the spec's tables; past
    /// it Straum starts over, so a bulk update costs a load and not the
    /// write lock for all of it.
    max_transaction_rows: usize,
    replication: Replication,
    /// How long a physical start waits for a consistent point.
    pub(crate) consistent_wait: Duration,
    /// Keeps the slot from holding more WAL than `--max-retained-wal`,
    /// whatever the stream and the writer are doing.
    pub(crate) guard: SlotGuard,
    state: State,
    /// The slot of the last [`Source::open`].
    slot: String,
    /// The spec's tables as the last [`Source::open`] checked them, which
    /// each reading of the WAL checks are still those tables.
    followed: Vec<Checked>,
    /// The labels of their enums, as loaded.
    labels: Vec<EnumLabel>,
    /// Reads the server's WAL, since the stream's connection is busy
    /// streaming. Connects again after an error.
    plain: Option<Connection>,
}

/// Where the source is between `open` and the stream.
enum State {
    Closed,
    /// Every dim and fact loads in the snapshot of the slot the stream
    /// starts from. Boxed: it outweighs the other states many times.
    Loading(Box<Snapshot>),
    Streaming(Stream),
}

/// A connection in a read-only, repeatable-read transaction that sees the
/// database as of where the stream starts: the logical slot's own
/// connection, or a plain one beside a physical stream already reading.
struct Snapshot {
    connection: Connection,
    slot: String,
    at: Lsn,
    tables: Tables,
    /// Tables not loaded yet. The transaction ends with the last load, so
    /// it pins xmin and blocks DDL on the tables no longer than it must.
    unloaded: Vec<Identifier>,
    /// The physical stream, which reads on while the tables load.
    stream: Option<Stream>,
}

impl Postgres {
    /// Nothing connects until [`Source::open`].
    pub fn new(
        dsn: Dsn,
        publication: &str,
        max_transaction_rows: usize,
        max_retained_wal: u64,
        replication: Replication,
    ) -> Postgres {
        Postgres {
            dsn,
            publication: publication.to_owned(),
            max_transaction_rows,
            replication,
            consistent_wait: CONSISTENT_WAIT,
            guard: SlotGuard::new(Watching {
                max_retained_wal,
                unwatched: UNWATCHED,
            }),
            state: State::Closed,
            slot: String::new(),
            followed: Vec::new(),
            labels: Vec::new(),
            plain: None,
        }
    }
}

/// A new temporary slot, and a transaction on its connection that sees
/// the database as of the slot's consistent point.
fn snapshot(
    dsn: &Dsn,
    spec: &Spec,
    publication: &str,
    wait: Duration,
    guard: &mut SlotGuard,
) -> Result<Snapshot, Error> {
    let mut connection = Connection::connect(dsn, ConnectionKind::Replication)?;
    connection.execute("BEGIN READ ONLY ISOLATION LEVEL REPEATABLE READ")?;
    let slot = slot_name();
    // PostgreSQL makes the slot once every transaction that writes and
    // was open when it began has ended: at most `wait`, then the start
    // names them, as a physical start does.
    connection.set_read_timeout(wait)?;
    let created = connection.rows(&format!(
        "CREATE_REPLICATION_SLOT {slot} TEMPORARY LOGICAL pgoutput (SNAPSHOT 'use')"
    ));
    if let Err(Error::Io(error)) = &created
        && timed_out(error)
    {
        return Err(waited_for(wait.as_secs(), slot_wait(dsn)?));
    }
    connection.clear_read_timeout()?;
    let created = match created {
        Ok(created) => created,
        // Without the wal_level logical decoding needs, PostgreSQL's error
        // names no fix; the setup check says what to change.
        Err(error) => {
            connection.execute("ROLLBACK")?;
            catalog::read(&mut connection, spec, publication, Replication::Logical)?;
            return Err(error);
        }
    };
    // The loads run on this connection, so nothing reads the slot until
    // they end; the watchdog bounds the WAL it holds meanwhile.
    guard.watch(dsn, &slot, vec![connection.closer()?])?;
    let at = (first_row_value(&created, 1).and_then(parse_lsn))
        .ok_or(Error::Protocol(Unexpected::SlotWithoutConsistentPoint))?;
    let tables = catalog::read(&mut connection, spec, publication, Replication::Logical)?;
    hold_tables(&mut connection, &tables)?;
    let unloaded = unloaded(&tables);
    Ok(Snapshot {
        connection,
        slot,
        at,
        tables,
        unloaded,
        stream: None,
    })
}

/// A physical stream reading from the last redo point, and a transaction
/// on a plain connection whose snapshot is taken once every transaction
/// committing after it is whole in the WAL the stream read (ADR-0008).
fn physical_snapshot(
    dsn: &Dsn,
    spec: &Spec,
    publication: &str,
    limit: usize,
    max_page_memory: usize,
    wait: Duration,
    guard: &mut SlotGuard,
) -> Result<Snapshot, Error> {
    let physical = Replication::Physical { max_page_memory };
    let mut connection = Connection::connect(dsn, ConnectionKind::Sql)?;
    let tables = catalog::read(&mut connection, spec, publication, physical)?;
    let checked = checked(tables);
    // After the slot: a rewrite between the two would be in neither the
    // layouts nor the WAL read.
    let (stream, start) = Physical::start(dsn)?;
    // The stream thread reads layouts on this one after DDL: a server
    // that falls silent there must not hold it, nor a stop that joins it.
    let layouts_connection = Connection::connect(dsn, ConnectionKind::Sql)?;
    layouts_connection.set_read_timeout(physical::DDL_WAIT)?;
    let layouts_closer = layouts_connection.closer()?;
    guard.watch(
        dsn,
        stream.slot(),
        vec![
            stream.closer()?,
            connection.closer()?,
            layouts_connection.closer()?,
        ],
    )?;
    let layouts = physical::read_layouts(&mut connection, &checked)?;
    let slot = stream.slot().to_owned();
    let pipeline = Pipeline::new(
        checked.clone(),
        layouts,
        stream.major(),
        limit,
        max_page_memory,
    );
    let (replication, replay) = stream.into_replay(pipeline, layouts_connection, checked);
    let mut stream = Stream::start(replication, Reader::Physical(Box::new(replay)), start)?;
    stream.close_too(layouts_closer);
    // Logs which transactions run now, rather than at the next checkpoint
    // or background writer round. Superusers, and roles granted it, may.
    let _ = connection.execute("SELECT pg_log_standby_snapshot()");
    wait_consistent(&mut stream, wait)?;
    let waited = wait.as_secs();
    // PostgreSQL logs a commit before snapshots see it, and waits for
    // synchronous standbys in between: a snapshot blind to one the stream
    // read as before its start would lose it, so another is taken.
    let deadline = Instant::now() + wait;
    let at = loop {
        connection.execute("BEGIN READ ONLY ISOLATION LEVEL REPEATABLE READ")?;
        let taken = connection.rows("SELECT pg_current_snapshot(), pg_current_wal_insert_lsn()")?;
        let seen = (first_row_value(&taken, 0).and_then(TransactionSnapshot::parse))
            .ok_or(Error::Protocol(Unexpected::SnapshotShape))?;
        let written = (first_row_value(&taken, 1).and_then(parse_lsn))
            .ok_or(Error::Protocol(Unexpected::NoWalPosition))?;
        match stream.see(seen, written)? {
            Sight::Through(at) => break at,
            Sight::Blind(xids) => {
                connection.execute("ROLLBACK")?;
                if Instant::now() >= deadline {
                    return Err(waited_for(waited, WaitProblem::InvisibleCommits(xids)));
                }
                thread::sleep(SNAPSHOT_AGAIN);
            }
        }
    };
    let tables = catalog::read(&mut connection, spec, publication, physical)?;
    hold_tables(&mut connection, &tables)?;
    let unloaded = unloaded(&tables);
    Ok(Snapshot {
        connection,
        slot,
        at,
        tables,
        unloaded,
        stream: Some(stream),
    })
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
                let names = TableNames {
                    name: table.name.clone(),
                    qualified: table.qualified(),
                };
                problems.push(names.table_problem(TableProblem::Rewritten));
            }
        }
    }
    if problems.is_empty() {
        return Ok(());
    }
    Err(Error::Setup(problems))
}

/// What a logical slot waited for in vain: the transactions open with an
/// xid, prepared ones included.
fn slot_wait(dsn: &Dsn) -> Result<WaitProblem, Error> {
    let mut connection = Connection::connect(dsn, ConnectionKind::Sql)?;
    let rows = connection.rows(
        "SELECT backend_xid::text FROM pg_stat_activity WHERE backend_xid IS NOT NULL
         UNION SELECT transaction::text FROM pg_prepared_xacts",
    )?;
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

/// The setup error of a physical start that waited `waited_seconds` in vain.
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

/// The facts, then the dims, as the decoder takes them.
fn checked(tables: Tables) -> Vec<Checked> {
    tables.facts.into_iter().chain(tables.dims).collect()
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

/// The plain connection, made on first use and after a failure.
// 'c: the connection returned is the one held in `plain`.
pub(crate) fn plain<'c>(
    plain: &'c mut Option<Connection>,
    dsn: &Dsn,
) -> Result<&'c mut Connection, Error> {
    if let Some(connection) = plain.take() {
        return Ok(plain.insert(connection));
    }
    let connection = Connection::connect(dsn, ConnectionKind::Sql)?;
    // A server that stops answering must fail the reading, so the lag it
    // no longer moves can grow.
    connection.set_read_timeout(WAL_READ_TIMEOUT)?;
    Ok(plain.insert(connection))
}

impl Source for Postgres {
    fn open(&mut self, spec: &Spec) -> Result<Lsn, SourceError> {
        // The old stream goes first, and its slot with it.
        self.guard.stop();
        self.state = State::Closed;
        self.plain = None;
        let opened = match self.replication {
            Replication::Logical => snapshot(
                &self.dsn,
                spec,
                &self.publication,
                self.consistent_wait,
                &mut self.guard,
            ),
            Replication::Physical { max_page_memory } => physical_snapshot(
                &self.dsn,
                spec,
                &self.publication,
                self.max_transaction_rows,
                max_page_memory,
                self.consistent_wait,
                &mut self.guard,
            ),
        };
        let snapshot = match opened {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let error = self.guard.blame(error);
                self.guard.stop();
                return Err(error.into());
            }
        };
        let at = snapshot.at;
        self.slot.clone_from(&snapshot.slot);
        self.followed = followed(&snapshot.tables);
        self.labels.clone_from(&snapshot.tables.labels);
        self.state = State::Loading(Box::new(snapshot));
        Ok(at)
    }

    fn load(&mut self, table: &Identifier, store: &mut Store) -> Result<(), SourceError> {
        let State::Loading(snapshot) = &mut self.state else {
            return Err(Error::Stopped.into());
        };
        match load_table(snapshot, table, store) {
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
        // before Straum starts over for it.
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
        let followed = Followed {
            tables: &self.followed,
            labels: &self.labels,
            publication: &self.publication,
            replication: self.replication,
        };
        let wal = match plain(&mut self.plain, &self.dsn) {
            Ok(connection) => read_wal_and_check(connection, &self.slot, &followed),
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
            State::Loading(snapshot) => State::Streaming(stream(
                *snapshot,
                &self.publication,
                self.max_transaction_rows,
            )?),
            other => other,
        };
        let State::Streaming(stream) = &mut self.state else {
            return Err(Error::Stopped);
        };
        stream.next(wait)
    }
}

/// Loads `table` in the snapshot.
fn load_table(snapshot: &mut Snapshot, table: &Identifier, store: &mut Store) -> Result<(), Error> {
    let checked = snapshot.tables.get(table);
    load::table(
        &mut snapshot.connection,
        checked,
        store,
        snapshot.stream.as_mut(),
    )?;
    loaded(snapshot, table)
}

/// The table is loaded; the snapshot's transaction ends with the last one.
fn loaded(snapshot: &mut Snapshot, table: &Identifier) -> Result<(), Error> {
    snapshot.unloaded.retain(|unloaded| unloaded != table);
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
    followed: &Followed<'_>,
) -> Result<Wal, Error> {
    let wal = read_wal(connection, slot)?;
    catalog::still_followed(connection, followed)?;
    Ok(wal)
}

/// Every table of `tables`, facts then dims.
fn followed(tables: &Tables) -> Vec<Checked> {
    let mut followed = tables.facts.clone();
    followed.extend(tables.dims.iter().cloned());
    followed
}

/// The server's WAL position, and how far the slot has confirmed and keeps
/// WAL.
pub(crate) fn read_wal(connection: &mut Connection, slot: &str) -> Result<Wal, Error> {
    // The insert position, not the write position: a commit is
    // acknowledged once inserted under synchronous_commit = off, and
    // as_of must not lead it (ADR-0005).
    let rows = connection.rows(&format!(
        "SELECT pg_current_wal_insert_lsn(), coalesce(confirmed_flush_lsn, restart_lsn), \
                restart_lsn, (SELECT setting FROM pg_settings WHERE name = 'wal_segment_size') \
         FROM pg_replication_slots WHERE slot_name = {}",
        escape_literal(slot)
    ))?;
    let Some([inserted, confirmed, restart, segment_bytes]) = rows.first().map(Vec::as_slice)
    else {
        return Err(no_wal_position(slot));
    };
    let (Some(inserted), Some(confirmed), Some(restart), Some(segment_bytes)) = (
        lsn_of(inserted.as_deref()),
        lsn_of(confirmed.as_deref()),
        lsn_of(restart.as_deref()),
        whole_number(segment_bytes.as_deref()),
    ) else {
        return Err(no_wal_position(slot));
    };
    Ok(Wal {
        written: record_end(inserted, segment_bytes),
        confirmed,
        restart,
    })
}

fn no_wal_position(slot: &str) -> Error {
    Error::Protocol(Unexpected::NoWalPositionForSlot(slot.to_owned()))
}

/// A column's LSN, if it holds one.
fn lsn_of(value: Option<&str>) -> Option<Lsn> {
    value.and_then(parse_lsn)
}

/// A column's whole number, if it holds one.
fn whole_number(value: Option<&str>) -> Option<u64> {
    value?.parse().ok()
}

/// Streams from the snapshot's consistent point. Tables not loaded yet
/// can't load any more.
fn stream(mut snapshot: Snapshot, publication: &str, limit: usize) -> Result<Stream, Error> {
    if !snapshot.unloaded.is_empty() {
        snapshot.connection.execute("COMMIT")?;
    }
    if let Some(stream) = snapshot.stream {
        return Ok(stream);
    }
    let names = escape_literal(&escape_identifier(publication));
    snapshot.connection.copy_both(&format!(
        "START_REPLICATION SLOT {} LOGICAL {} (proto_version '1', publication_names {names})",
        snapshot.slot, snapshot.at
    ))?;
    let reader = Reader::Logical(Decoder::new(checked(snapshot.tables), limit));
    Stream::start(snapshot.connection, reader, snapshot.at)
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
