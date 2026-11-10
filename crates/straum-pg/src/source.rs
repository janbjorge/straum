//! [`Pg`], the [`Source`] over PostgreSQL: a temporary slot, loads in its
//! snapshot on the same connection, then the stream from its consistent
//! point. A plain connection on the side reads the server's WAL position
//! and resolves the refs of each commit's fact rows.

use std::mem;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use postgres_protocol::escape::{escape_identifier, escape_literal};
use straum_app::{Commit, Lsn, Source, SourceError, Wal};
use straum_domain::{Fact, Ident, Spec, Store};

use crate::catalog::{self, Tables};
use crate::decode::Decoder;
use crate::dsn::Dsn;
use crate::error::Error;
use crate::load;
use crate::resolve::Resolver;
use crate::stream::Stream;
use crate::wire::Conn;

/// Slots made by this process, for unique names.
static SLOTS: AtomicU64 = AtomicU64::new(0);

/// straum's [`Source`] over PostgreSQL 18 with `wal_level = logical`.
pub struct Pg {
    dsn: Dsn,
    publication: String,
    /// The most rows one transaction may change in the spec's tables; past
    /// it straum starts over, so a bulk update costs a load and not the
    /// write lock for all of it.
    max_transaction_rows: usize,
    state: State,
    /// The slot of the last [`Source::open`].
    slot: String,
    /// Reads the server's WAL and looks dim rows up, since the stream's
    /// connection is busy streaming. Connects again after an error.
    side: Option<Conn>,
}

enum State {
    Closed,
    /// Every fact loads in the snapshot of the slot the stream starts
    /// from.
    Loading(Snapshot),
    Streaming {
        stream: Stream,
        resolver: Resolver,
    },
}

/// A replication connection in a read-only, repeatable-read transaction
/// that sees the database as of a new temporary slot's consistent point.
struct Snapshot {
    conn: Conn,
    slot: String,
    at: Lsn,
    spec: Spec,
    tables: Tables,
    /// Facts not loaded yet. The transaction ends with the last load, so
    /// it pins xmin and blocks DDL on the tables no longer than it must.
    unloaded: Vec<Ident>,
}

impl Pg {
    /// Nothing connects until [`Source::open`].
    pub fn new(dsn: Dsn, publication: &str, max_transaction_rows: usize) -> Pg {
        Pg {
            dsn,
            publication: publication.to_owned(),
            max_transaction_rows,
            state: State::Closed,
            slot: String::new(),
            side: None,
        }
    }
}

/// A new temporary slot, and a transaction on its connection that sees
/// the database as of the slot's consistent point.
fn snapshot(dsn: &Dsn, spec: &Spec, publication: &str) -> Result<Snapshot, Error> {
    let mut conn = Conn::connect(dsn, true)?;
    // Without the wal_level logical decoding needs, the slot can't be made
    // and PostgreSQL's error names no fix, so the setup is checked first too.
    catalog::read(&mut conn, spec, publication)?;
    conn.execute("BEGIN READ ONLY ISOLATION LEVEL REPEATABLE READ")?;
    let slot = format!(
        "straum_{}_{}_{}",
        process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
        SLOTS.fetch_add(1, Ordering::Relaxed)
    );
    let created = conn.rows(&format!(
        "CREATE_REPLICATION_SLOT {slot} TEMPORARY LOGICAL pgoutput (SNAPSHOT 'use')"
    ))?;
    let at = created
        .first()
        .and_then(|row| row.get(1))
        .and_then(|point| parse_lsn(point.as_deref()?))
        .ok_or_else(|| Error::Protocol("a slot without a consistent point".into()))?;
    let tables = catalog::read(&mut conn, spec, publication)?;
    let unloaded = spec.facts().iter().map(|f| f.name.clone()).collect();
    Ok(Snapshot {
        conn,
        slot,
        at,
        spec: spec.clone(),
        tables,
        unloaded,
    })
}

/// The side connection, made on first use and after a failure.
fn side<'c>(side: &'c mut Option<Conn>, dsn: &Dsn) -> Result<&'c mut Conn, Error> {
    if side.is_none() {
        *side = Some(Conn::connect(dsn, false)?);
    }
    Ok(side.as_mut().expect("connected just now"))
}

impl Source for Pg {
    fn open(&mut self, spec: &Spec) -> Result<Lsn, SourceError> {
        // The old stream goes first, and its slot with it.
        self.state = State::Closed;
        self.side = None;
        let snapshot = snapshot(&self.dsn, spec, &self.publication)?;
        let at = snapshot.at;
        self.slot.clone_from(&snapshot.slot);
        self.state = State::Loading(snapshot);
        Ok(at)
    }

    fn load_fact(&mut self, fact: &Fact, store: &mut Store) -> Result<(), SourceError> {
        let State::Loading(snapshot) = &mut self.state else {
            return Err(Error::Stopped.into());
        };
        load::fact(&mut snapshot.conn, &snapshot.tables, fact, store)?;
        snapshot.unloaded.retain(|t| *t != fact.name);
        if snapshot.unloaded.is_empty() {
            snapshot.conn.execute("COMMIT")?;
        }
        Ok(())
    }

    fn next_commit(&mut self, wait: Duration) -> Result<Option<Commit>, SourceError> {
        self.state = match mem::replace(&mut self.state, State::Closed) {
            State::Loading(snapshot) => {
                let resolver = Resolver::new(&snapshot.spec, &snapshot.tables);
                let stream = stream(snapshot, &self.publication, self.max_transaction_rows)?;
                State::Streaming { stream, resolver }
            }
            other => other,
        };
        let State::Streaming { stream, resolver } = &mut self.state else {
            return Err(Error::Stopped.into());
        };
        let Some(mut commit) = stream.next(wait)? else {
            return Ok(None);
        };
        // A failed lookup leaves the connection mid-query, so it goes.
        let resolved =
            side(&mut self.side, &self.dsn).and_then(|conn| resolver.resolve(conn, &mut commit));
        if let Err(e) = resolved {
            self.side = None;
            return Err(e.into());
        }
        Ok(Some(commit))
    }

    fn wal(&mut self) -> Result<Wal, SourceError> {
        if matches!(self.state, State::Closed) {
            return Err(Error::Stopped.into());
        }
        let wal = side(&mut self.side, &self.dsn).and_then(|conn| read_wal(conn, &self.slot));
        if wal.is_err() {
            self.side = None;
        }
        Ok(wal?)
    }
}

/// The server's WAL position, and how far the slot has confirmed and keeps
/// WAL.
pub(crate) fn read_wal(conn: &mut Conn, slot: &str) -> Result<Wal, Error> {
    let rows = conn.rows(&format!(
        "SELECT pg_current_wal_lsn(), confirmed_flush_lsn, restart_lsn \
         FROM pg_replication_slots WHERE slot_name = {}",
        escape_literal(slot)
    ))?;
    let lsns: Vec<Lsn> = (rows.first().into_iter().flatten())
        .filter_map(|lsn| parse_lsn(lsn.as_deref()?))
        .collect();
    match lsns[..] {
        [server, confirmed, restart] => Ok(Wal {
            server,
            confirmed,
            restart,
        }),
        _ => Err(Error::Protocol(format!("no WAL position for slot {slot}"))),
    }
}

/// Streams from the snapshot's consistent point. Facts not loaded yet
/// can't load any more.
fn stream(mut snapshot: Snapshot, publication: &str, max_rows: usize) -> Result<Stream, Error> {
    if !snapshot.unloaded.is_empty() {
        snapshot.conn.execute("COMMIT")?;
    }
    let names = escape_literal(&escape_identifier(publication));
    snapshot.conn.copy_both(&format!(
        "START_REPLICATION SLOT {} LOGICAL {} (proto_version '1', publication_names {names})",
        snapshot.slot, snapshot.at
    ))?;
    Stream::start(
        snapshot.conn,
        Decoder::new(snapshot.tables.facts, max_rows),
        snapshot.at,
    )
}

/// `16/B374D848`, as PostgreSQL prints an LSN.
fn parse_lsn(text: &str) -> Option<Lsn> {
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
