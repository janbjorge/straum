//! The port to PostgreSQL: tables read at a snapshot, and the stream of
//! commits after it. One port, since the first load reads the snapshot of the
//! slot the stream comes from, on the same connection.

use std::error;
use std::fmt;
use std::time::SystemTime;

use weir_domain::{Datum, Dim, DimTable, Fact, Ident, Rollup, Spec};

/// Whatever the source can't work around: the connection dropped, the setup
/// is missing, a table no longer fits the spec. weir starts over after one.
pub type SourceError = Box<dyn error::Error + Send + Sync>;

/// A position in PostgreSQL's write-ahead log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lsn(pub u64);

pub trait Source {
    /// Checks the setup against the spec and starts a new stream, dropping
    /// any stream before it. Returns where it starts: commits up to there
    /// are in the snapshot, later ones are streamed.
    fn open(&mut self, spec: &Spec) -> Result<Lsn, SourceError>;

    /// Adds the fact's rows to an empty rollup and returns the LSN they
    /// reflect: every commit up to it and none after. That is at or past
    /// every commit streamed so far.
    fn load_fact(&mut self, fact: &Fact, rollup: &mut Rollup) -> Result<Lsn, SourceError>;

    /// As [`Source::load_fact`], for a dim's rows.
    fn load_dim(&mut self, dim: &Dim, table: &mut DimTable) -> Result<Lsn, SourceError>;

    /// The next commit, waiting for one. Asking means the last one is
    /// applied, so PostgreSQL may recycle the WAL before it.
    fn next_commit(&mut self) -> Result<Commit, SourceError>;
}

/// One transaction's changes to the spec's tables, in the order made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// The end of the commit record, which pgoutput reports as the
    /// transaction's end. Loads and the stream are ordered by it.
    pub lsn: Lsn,
    /// When the transaction committed, by the server's clock.
    pub time: SystemTime,
    pub changes: Vec<Change>,
}

/// A change to a table, named as in the spec. Fact rows hold
/// [`Rollup::columns`]; dim rows hold the key, then the attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Insert {
        table: Ident,
        row: Vec<Datum>,
    },
    Update {
        table: Ident,
        old: Old,
        new: Vec<Datum>,
    },
    Delete {
        table: Ident,
        old: Old,
    },
    /// Every row is gone.
    Truncate {
        table: Ident,
    },
    /// The table changed shape, so rows already read may no longer read the
    /// same; a column's type changed, for example. Also a change the source
    /// can't read whole, such as an update that left out an unchanged
    /// TOASTed value with no old row to take it from.
    Altered {
        table: Ident,
    },
}

/// What PostgreSQL sends of a row before an update or delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Old {
    /// The whole row, under `REPLICA IDENTITY FULL`.
    Row(Vec<Datum>),
    /// The key only: a delete, or an update that changed the key.
    Key(Datum),
    /// Nothing: an update that kept the key.
    SameKey,
}

impl Change {
    pub(crate) fn table(&self) -> &Ident {
        match self {
            Change::Insert { table, .. }
            | Change::Update { table, .. }
            | Change::Delete { table, .. }
            | Change::Truncate { table }
            | Change::Altered { table } => table,
        }
    }
}

impl Old {
    /// The key a dim row had; a dim row starts with its key.
    pub(crate) fn dim_key(self) -> Option<Datum> {
        match self {
            Old::Row(row) => row.into_iter().next(),
            Old::Key(key) => Some(key),
            Old::SameKey => None,
        }
    }
}

/// As PostgreSQL prints it: the high and low 32 bits in hex.
impl fmt::Display for Lsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:X}/{:X}", self.0 >> 32, self.0 & 0xFFFF_FFFF)
    }
}
