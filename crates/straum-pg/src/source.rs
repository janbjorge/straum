//! [`Pg`], the [`Source`] over PostgreSQL: a temporary slot, loads in its
//! snapshot on the same connection, then the stream from its consistent
//! point.

use std::mem;
use std::process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use postgres_protocol::escape::{escape_identifier, escape_literal};
use straum_app::{Commit, Lsn, Source, SourceError};
use straum_domain::{Dim, DimTable, Fact, Ident, Rollup, Spec};

use crate::catalog::{self, Table};
use crate::dsn::Dsn;
use crate::error::Error;
use crate::load;
use crate::stream::{Decoder, Stream};
use crate::wire::Conn;

/// Slots made by this process, for unique names.
static SLOTS: AtomicU64 = AtomicU64::new(0);

/// straum's [`Source`] over PostgreSQL 18 with `wal_level = logical`.
pub struct Pg {
    dsn: Dsn,
    publication: String,
    state: State,
}

enum State {
    Closed,
    /// Every table loads in the snapshot of the slot the stream starts
    /// from.
    Loading(Snapshot),
    Streaming(Stream),
}

/// A replication connection in a read-only, repeatable-read transaction
/// that sees the database as of a new temporary slot's consistent point.
struct Snapshot {
    conn: Conn,
    slot: String,
    at: Lsn,
    tables: Vec<Table>,
    /// Tables not loaded yet. The transaction ends with the last load, so
    /// it pins xmin and blocks DDL on the tables no longer than it must.
    unloaded: Vec<Ident>,
}

impl Pg {
    /// Nothing connects until [`Source::open`].
    pub fn new(dsn: Dsn, publication: &str) -> Pg {
        Pg {
            dsn,
            publication: publication.to_owned(),
            state: State::Closed,
        }
    }

    /// Loads into `target` from the snapshot the stream starts from.
    fn load(&mut self, name: &Ident, target: Target) -> Result<(), Error> {
        let State::Loading(snapshot) = &mut self.state else {
            return Err(Error::Stopped);
        };
        let table = snapshot
            .tables
            .iter()
            .find(|t| t.name == *name)
            .expect("the spec's tables are checked");
        match target {
            Target::Fact(fact, rollup) => load::fact(&mut snapshot.conn, fact, table, rollup)?,
            Target::Dim(rows) => load::dim(&mut snapshot.conn, table, rows)?,
        }
        snapshot.unloaded.retain(|t| t != name);
        if snapshot.unloaded.is_empty() {
            snapshot.conn.execute("COMMIT")?;
        }
        Ok(())
    }
}

/// A new temporary slot, and a transaction on its connection that sees
/// the database as of the slot's consistent point.
fn snapshot(dsn: &Dsn, spec: &Spec, publication: &str) -> Result<Snapshot, Error> {
    let mut conn = Conn::connect(dsn, true)?;
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
    let unloaded = spec
        .dims()
        .iter()
        .map(|d| &d.name)
        .chain(spec.facts().iter().map(|f| &f.name))
        .cloned()
        .collect();
    Ok(Snapshot {
        conn,
        slot,
        at,
        tables,
        unloaded,
    })
}

enum Target<'a> {
    Fact(&'a Fact, &'a mut Rollup),
    Dim(&'a mut DimTable),
}

impl Source for Pg {
    fn open(&mut self, spec: &Spec) -> Result<Lsn, SourceError> {
        // The old stream goes first, and its slot with it.
        self.state = State::Closed;
        let snapshot = snapshot(&self.dsn, spec, &self.publication)?;
        let at = snapshot.at;
        self.state = State::Loading(snapshot);
        Ok(at)
    }

    fn load_fact(&mut self, fact: &Fact, rollup: &mut Rollup) -> Result<(), SourceError> {
        Ok(self.load(&fact.name, Target::Fact(fact, rollup))?)
    }

    fn load_dim(&mut self, dim: &Dim, table: &mut DimTable) -> Result<(), SourceError> {
        Ok(self.load(&dim.name, Target::Dim(table))?)
    }

    fn next_commit(&mut self) -> Result<Commit, SourceError> {
        self.state = match mem::replace(&mut self.state, State::Closed) {
            State::Loading(snapshot) => State::Streaming(stream(snapshot, &self.publication)?),
            other => other,
        };
        match &mut self.state {
            State::Streaming(stream) => Ok(stream.next()?),
            _ => Err(Error::Stopped.into()),
        }
    }
}

/// Streams from the snapshot's consistent point. Tables not loaded yet
/// can't load any more.
fn stream(mut snapshot: Snapshot, publication: &str) -> Result<Stream, Error> {
    if !snapshot.unloaded.is_empty() {
        snapshot.conn.execute("COMMIT")?;
    }
    let names = escape_literal(&escape_identifier(publication));
    snapshot.conn.copy_both(&format!(
        "START_REPLICATION SLOT {} LOGICAL {} (proto_version '1', publication_names {names})",
        snapshot.slot, snapshot.at
    ))?;
    Stream::start(snapshot.conn, Decoder::new(snapshot.tables), snapshot.at)
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
