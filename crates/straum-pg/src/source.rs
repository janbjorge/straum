//! [`Postgres`], the [`Source`] over PostgreSQL. Logical: a temporary slot,
//! loads in its snapshot on the same connection, then the stream from its
//! consistent point. Physical: a temporary physical slot streams from the
//! last redo point while the loads run in a snapshot taken once every
//! later commit is whole in what it read. A plain connection on the side
//! reads the server's WAL position.

use std::mem;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use postgres_protocol::escape::{escape_identifier, escape_literal};
use straum_app::{Commit, Lsn, Source, SourceError, Wal};
use straum_domain::{Identifier, Spec, Store};

use crate::catalog::{self, Checked, Tables};
use crate::decode::Decoder;
use crate::dsn::Dsn;
use crate::error::Error;
use crate::load;
use crate::physical::{self, Physical};
use crate::stream::{Reader, Stream};
use crate::wal::pipeline::{Pipeline, Sight};
use crate::wal::transactions::{Consistency, Snapshot as Seen};
use crate::wire::Connection;

/// Which WAL Straum streams: logical decoding's, through a publication,
/// or the physical WAL, which `wal_level = replica` writes (ADR-0049).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Replication {
    Logical,
    /// Past `max_page_memory` bytes of rows held, Straum starts over.
    Physical {
        max_page_memory: usize,
    },
}

/// How long a physical stream waits for PostgreSQL to log which
/// transactions run, which the background writer does every 15 seconds
/// while the WAL moves, and `pg_log_standby_snapshot()` at once.
const CONSISTENT_WAIT: Duration = Duration::from_secs(60);
/// How long a physical start waits before it takes the load's snapshot
/// again.
const SNAPSHOT_AGAIN: Duration = Duration::from_millis(20);

/// Slots made by this process, for unique names.
static SLOTS: AtomicU64 = AtomicU64::new(0);

/// Straum's [`Source`] over PostgreSQL 17 and newer with
/// `wal_level = logical`, or `replica` from 19 (ADR-0046).
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
    state: State,
    /// The slot of the last [`Source::open`].
    slot: String,
    /// Reads the server's WAL, since the stream's connection is busy
    /// streaming. Connects again after an error.
    plain: Option<Connection>,
}

enum State {
    Closed,
    /// Every dim and fact loads in the snapshot of the slot the stream
    /// starts from.
    Loading(Snapshot),
    Streaming(Stream),
}

/// A connection in a read-only, repeatable-read transaction that sees the
/// database as of where the stream starts: the logical slot's own
/// connection, or a plain one beside a physical stream already reading.
struct Snapshot {
    conn: Connection,
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
        replication: Replication,
    ) -> Postgres {
        Postgres {
            dsn,
            publication: publication.to_owned(),
            max_transaction_rows,
            replication,
            consistent_wait: CONSISTENT_WAIT,
            state: State::Closed,
            slot: String::new(),
            plain: None,
        }
    }
}

/// A new temporary slot, and a transaction on its connection that sees
/// the database as of the slot's consistent point.
fn snapshot(dsn: &Dsn, spec: &Spec, publication: &str) -> Result<Snapshot, Error> {
    let mut conn = Connection::connect(dsn, true)?;
    conn.execute("BEGIN READ ONLY ISOLATION LEVEL REPEATABLE READ")?;
    let slot = slot_name();
    let created = match conn.rows(&format!(
        "CREATE_REPLICATION_SLOT {slot} TEMPORARY LOGICAL pgoutput (SNAPSHOT 'use')"
    )) {
        Ok(created) => created,
        // Without the wal_level logical decoding needs, PostgreSQL's error
        // names no fix; the setup check says what to change.
        Err(e) => {
            conn.execute("ROLLBACK")?;
            catalog::read(&mut conn, spec, publication, Replication::Logical)?;
            return Err(e);
        }
    };
    let at = created
        .first()
        .and_then(|row| row.get(1))
        .and_then(|point| parse_lsn(point.as_deref()?))
        .ok_or_else(|| Error::protocol("a slot without a consistent point"))?;
    let tables = catalog::read(&mut conn, spec, publication, Replication::Logical)?;
    let unloaded = unloaded(&tables);
    Ok(Snapshot {
        conn,
        slot,
        at,
        tables,
        unloaded,
        stream: None,
    })
}

/// A physical stream reading from the last redo point, and a transaction
/// on a plain connection whose snapshot is taken once every transaction
/// committing after it is whole in the WAL the stream read (ADR-0049).
fn physical_snapshot(
    dsn: &Dsn,
    spec: &Spec,
    publication: &str,
    limit: usize,
    max_page_memory: usize,
    wait: Duration,
) -> Result<Snapshot, Error> {
    let physical = Replication::Physical { max_page_memory };
    let mut conn = Connection::connect(dsn, false)?;
    let tables = catalog::read(&mut conn, spec, publication, physical)?;
    let checked = checked(tables);
    // After the slot: a rewrite between the two would be in neither the
    // layouts nor the WAL read.
    let (stream, start) = Physical::start(dsn)?;
    let layouts = physical::tables(&mut conn, &checked)?;
    let slot = stream.slot().to_owned();
    let pipeline = Pipeline::new(
        checked.clone(),
        layouts,
        stream.major(),
        limit,
        max_page_memory,
    );
    let layouts_conn = Connection::connect(dsn, false)?;
    let (replication, replay) = stream.into_replay(pipeline, layouts_conn, checked);
    let mut stream = Stream::start(replication, Reader::Physical(Box::new(replay)), start)?;
    // Logs which transactions run now, rather than at the next checkpoint
    // or background writer round. Superusers, and roles granted it, may.
    let _ = conn.execute("SELECT pg_log_standby_snapshot()");
    let waited = wait.as_secs();
    match stream.wait_consistent(wait)? {
        Consistency::Consistent => {}
        Consistency::Unknown => {
            return Err(Error::Setup(vec![format!(
                "--wal physical waited {waited} s for PostgreSQL to log which transactions run; \
                 GRANT EXECUTE ON FUNCTION pg_log_standby_snapshot() TO Straum's role"
            )]));
        }
        Consistency::Waiting(xids) => {
            let xids: Vec<String> = xids.iter().map(u32::to_string).collect();
            return Err(Error::Setup(vec![format!(
                "--wal physical waited {waited} s for transactions open before it started to end: \
                 xid {}; a long or prepared one (pg_prepared_xacts) holds the start back",
                xids.join(", ")
            )]));
        }
    }
    // PostgreSQL logs a commit before snapshots see it, and waits for
    // synchronous standbys in between: a snapshot blind to one the stream
    // read as before its start would lose it, so another is taken.
    let deadline = Instant::now() + wait;
    let at = loop {
        conn.execute("BEGIN READ ONLY ISOLATION LEVEL REPEATABLE READ")?;
        let taken = conn.rows("SELECT pg_current_snapshot(), pg_current_wal_insert_lsn()")?;
        let field = |i: usize| taken.first().and_then(|row| row.get(i)?.as_deref());
        let seen = field(0)
            .and_then(Seen::parse)
            .ok_or_else(|| Error::protocol("a snapshot of an unknown shape"))?;
        let written = field(1)
            .and_then(parse_lsn)
            .ok_or_else(|| Error::protocol("no WAL position"))?;
        match stream.see(seen, written)? {
            Sight::Through(at) => break at,
            Sight::Blind(xids) => {
                conn.execute("ROLLBACK")?;
                if Instant::now() >= deadline {
                    let xids: Vec<String> = xids.iter().map(u32::to_string).collect();
                    return Err(Error::Setup(vec![format!(
                        "--wal physical waited {waited} s for transactions it read the commits of \
                         to end: xid {}; a synchronous standby that doesn't answer holds them",
                        xids.join(", ")
                    )]));
                }
                thread::sleep(SNAPSHOT_AGAIN);
            }
        }
    };
    let tables = catalog::read(&mut conn, spec, publication, physical)?;
    let unloaded = unloaded(&tables);
    Ok(Snapshot {
        conn,
        slot,
        at,
        tables,
        unloaded,
        stream: Some(stream),
    })
}

fn unloaded(tables: &Tables) -> Vec<Identifier> {
    (tables.dims.iter().chain(&tables.facts))
        .map(|t| t.name.clone())
        .collect()
}

/// The facts, then the dims, as the decoder takes them.
fn checked(tables: Tables) -> Vec<Checked> {
    tables.facts.into_iter().chain(tables.dims).collect()
}

/// A slot name no other Straum process or slot of this one has.
pub(crate) fn slot_name() -> String {
    format!(
        "straum_{}_{}_{}",
        process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
        SLOTS.fetch_add(1, Ordering::Relaxed)
    )
}

/// The plain connection, made on first use and after a failure.
fn plain<'c>(plain: &'c mut Option<Connection>, dsn: &Dsn) -> Result<&'c mut Connection, Error> {
    if plain.is_none() {
        *plain = Some(Connection::connect(dsn, false)?);
    }
    Ok(plain.as_mut().expect("connected just now"))
}

impl Source for Postgres {
    fn open(&mut self, spec: &Spec) -> Result<Lsn, SourceError> {
        // The old stream goes first, and its slot with it.
        self.state = State::Closed;
        self.plain = None;
        let snapshot = match self.replication {
            Replication::Logical => snapshot(&self.dsn, spec, &self.publication)?,
            Replication::Physical { max_page_memory } => physical_snapshot(
                &self.dsn,
                spec,
                &self.publication,
                self.max_transaction_rows,
                max_page_memory,
                self.consistent_wait,
            )?,
        };
        let at = snapshot.at;
        self.slot.clone_from(&snapshot.slot);
        self.state = State::Loading(snapshot);
        Ok(at)
    }

    fn load(&mut self, table: &Identifier, store: &mut Store) -> Result<(), SourceError> {
        let State::Loading(snapshot) = &mut self.state else {
            return Err(Error::Stopped.into());
        };
        load::table(&mut snapshot.conn, snapshot.tables.get(table), store)?;
        loaded(snapshot, table)
    }

    fn next_commit(&mut self, wait: Duration) -> Result<Option<Commit>, SourceError> {
        self.state = match mem::replace(&mut self.state, State::Closed) {
            State::Loading(snapshot) => State::Streaming(stream(
                snapshot,
                &self.publication,
                self.max_transaction_rows,
            )?),
            other => other,
        };
        let State::Streaming(stream) = &mut self.state else {
            return Err(Error::Stopped.into());
        };
        Ok(stream.next(wait)?)
    }

    fn wal(&mut self) -> Result<Wal, SourceError> {
        if matches!(self.state, State::Closed) {
            return Err(Error::Stopped.into());
        }
        let wal = plain(&mut self.plain, &self.dsn).and_then(|conn| read_wal(conn, &self.slot));
        if wal.is_err() {
            self.plain = None;
        }
        Ok(wal?)
    }
}

/// The table is loaded; the snapshot's transaction ends with the last one.
fn loaded(snapshot: &mut Snapshot, table: &Identifier) -> Result<(), SourceError> {
    snapshot.unloaded.retain(|t| t != table);
    if snapshot.unloaded.is_empty() {
        snapshot.conn.execute("COMMIT")?;
    }
    Ok(())
}

/// The server's WAL position, and how far the slot has confirmed and keeps
/// WAL.
pub(crate) fn read_wal(conn: &mut Connection, slot: &str) -> Result<Wal, Error> {
    let rows = conn.rows(&format!(
        "SELECT pg_current_wal_lsn(), coalesce(confirmed_flush_lsn, restart_lsn), restart_lsn \
         FROM pg_replication_slots WHERE slot_name = {}",
        escape_literal(slot)
    ))?;
    let lsns: Vec<Lsn> = (rows.first().into_iter().flatten())
        .filter_map(|lsn| parse_lsn(lsn.as_deref()?))
        .collect();
    match lsns[..] {
        [written, confirmed, restart] => Ok(Wal {
            written,
            confirmed,
            restart,
        }),
        _ => Err(Error::Protocol(format!("no WAL position for slot {slot}"))),
    }
}

/// Streams from the snapshot's consistent point. Tables not loaded yet
/// can't load any more.
fn stream(mut snapshot: Snapshot, publication: &str, limit: usize) -> Result<Stream, Error> {
    if !snapshot.unloaded.is_empty() {
        snapshot.conn.execute("COMMIT")?;
    }
    if let Some(stream) = snapshot.stream {
        return Ok(stream);
    }
    let names = escape_literal(&escape_identifier(publication));
    snapshot.conn.copy_both(&format!(
        "START_REPLICATION SLOT {} LOGICAL {} (proto_version '1', publication_names {names})",
        snapshot.slot, snapshot.at
    ))?;
    let reader = Reader::Logical(Decoder::new(checked(snapshot.tables), limit));
    Stream::start(snapshot.conn, reader, snapshot.at)
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
