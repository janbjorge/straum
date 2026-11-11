//! The port to PostgreSQL: facts and dims read at a snapshot, and the
//! stream of commits after it. One port, since the first load reads the
//! snapshot of the slot the stream comes from, on the same connection. A
//! row comes as its table holds it: a fact's columns, or a dim's key and
//! the columns the views read.

use std::error;
use std::fmt;
use std::time::{Duration, SystemTime};

use straum_domain::{Datum, Ident, Spec, Store};

/// Whatever the source can't work around: the connection dropped, the setup
/// is missing, a table no longer fits the spec. Straum starts over after one.
pub type SourceError = Box<dyn error::Error + Send + Sync>;

/// A position in PostgreSQL's write-ahead log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lsn(pub u64);

pub trait Source {
    /// Checks the setup against the spec and starts a new stream, dropping
    /// any stream before it. Returns where it starts: commits up to there
    /// are in the snapshot, later ones are streamed.
    fn open(&mut self, spec: &Spec) -> Result<Lsn, SourceError>;

    /// Adds the table's rows to the store, which holds none of it yet, as
    /// of the LSN [`Source::open`] returned: every commit up to it and none
    /// after. Rows hold [`Fact::columns`] or [`Dim::columns`] and go
    /// through [`Store::add_rows`].
    fn load(&mut self, table: &Ident, store: &mut Store) -> Result<(), SourceError>;

    /// The next commit, waiting at most `wait` for one. Asking means the
    /// last one is applied, so PostgreSQL may recycle the WAL before it.
    fn next_commit(&mut self, wait: Duration) -> Result<Option<Commit>, SourceError>;

    /// Reads how far the server has written WAL and how much of it the
    /// stream's slot holds, from [`Source::open`] on.
    fn wal(&mut self) -> Result<Wal, SourceError>;
}

/// The server's WAL and the stream's slot, read at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wal {
    /// How far the server has written WAL.
    pub server: Lsn,
    /// Every commit up to here is applied, as far as the server knows: the
    /// slot's `confirmed_flush_lsn`.
    pub confirmed: Lsn,
    /// The server keeps WAL from here on for the slot: its `restart_lsn`.
    pub restart: Lsn,
}

impl Wal {
    /// Bytes of WAL the server keeps for the slot.
    pub fn retained(&self) -> u64 {
        self.server.0.saturating_sub(self.restart.0)
    }
}

/// One transaction's changes to the spec's facts and dims, in the order
/// made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// The end of the commit record, which pgoutput reports as the
    /// transaction's end. Loads and the stream are ordered by it.
    pub lsn: Lsn,
    /// When the transaction committed, by the server's clock.
    pub time: SystemTime,
    pub changes: Vec<Change>,
}

/// A change to a fact or a dim, named as in the spec. Rows hold
/// [`Fact::columns`] or [`Dim::columns`], as a load's do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Insert {
        table: Ident,
        row: Vec<Datum>,
    },
    /// With the whole old row, under `REPLICA IDENTITY FULL`.
    Update {
        table: Ident,
        old: Vec<Datum>,
        new: Vec<Datum>,
    },
    Delete {
        table: Ident,
        old: Vec<Datum>,
    },
    /// Every row is gone.
    Truncate {
        table: Ident,
    },
    /// The table changed shape, so rows already read may no longer read the
    /// same; a column's type changed, for example. Also a change the source
    /// can't read whole: an update or delete without the old row, or one
    /// that left out an unchanged TOASTed value with no old row to take it
    /// from.
    Unfit {
        table: Ident,
    },
}

impl Change {
    pub fn table(&self) -> &Ident {
        match self {
            Change::Insert { table, .. }
            | Change::Update { table, .. }
            | Change::Delete { table, .. }
            | Change::Truncate { table }
            | Change::Unfit { table } => table,
        }
    }
}

/// As PostgreSQL prints it: the high and low 32 bits in hex.
impl fmt::Display for Lsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:X}/{:X}", self.0 >> 32, self.0 & 0xFFFF_FFFF)
    }
}
