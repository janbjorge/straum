//! The port to PostgreSQL, and the types that cross it. A source first
//! loads every fact and dim as of one snapshot, then streams the commits
//! made after it; the writer reads both. One port, since the load and
//! the stream meet at one position, which only the source knows how to
//! hold. A row comes as its table holds it: a fact's columns, or a dim's
//! key and the columns the views read.

use std::error;
use std::fmt;
use std::sync::atomic::{AtomicU64, AtomicUsize};
use std::time::{Duration, SystemTime};

use straum_domain::{Datum, Identifier, Spec, Store};

/// Whatever the source can't work around: the connection dropped, the setup
/// is missing, a table no longer fits the spec. Straum stops after one.
pub type SourceError = Box<dyn error::Error + Send + Sync>;

/// A position in PostgreSQL's write-ahead log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Lsn(pub u64);

/// Where the rows come from: PostgreSQL in the binary, a fake in tests.
pub trait Source {
    /// Checks the setup against the spec and starts a new stream, dropping
    /// any stream before it. Returns where it starts: commits up to there
    /// are in the snapshot, later ones are streamed.
    fn open(&mut self, spec: &Spec) -> Result<Opened, SourceError>;

    /// Guesses how big the table `tables` read is, as [`Source::load`]
    /// takes them, without reading its rows.
    fn estimate(&mut self, tables: &[Identifier]) -> Result<Estimate, SourceError>;

    /// Adds the rows of `tables` to the store, which holds none of them
    /// yet: one dim, or the views that read one fact table, which the
    /// source reads once for all of them. The rows are as of the LSN
    /// [`Source::open`] returned: every commit up to it and none after.
    /// They hold [`Fact::columns`] or [`Dim::columns`] and go through
    /// [`Store::add_rows`]. The source counts into `counts` as it reads.
    fn load(
        &mut self,
        tables: &[Identifier],
        store: &mut Store,
        counts: &LoadCounts,
    ) -> Result<(), SourceError>;

    /// The next commit, waiting at most `wait` for one. Asking means the
    /// last one is applied.
    fn next_commit(&mut self, wait: Duration) -> Result<Option<Commit>, SourceError>;

    /// Reads how far the server has written WAL, how much of it the
    /// stream's slot holds, and how far Straum has applied, from
    /// [`Source::open`] on.
    fn wal(&mut self) -> Result<Wal, SourceError>;
}

/// Where a new stream starts, and when its snapshot was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Opened {
    /// Commits up to here are in the snapshot, later ones are streamed.
    pub start: Lsn,
    /// The snapshot's time on the server's clock, which a commit's
    /// `committed` reads too: the store keeps narrow buckets back from it.
    pub at: SystemTime,
}

/// How big a table is before it loads, as the source guesses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Estimate {
    /// `None` when the source can't guess.
    pub rows: Option<u64>,
    /// The table's size where the source keeps it.
    pub bytes: u64,
}

/// What a load has read so far. The source counts as it reads; the status
/// reads it from another thread meanwhile.
#[derive(Debug, Default)]
pub struct LoadCounts {
    /// Rows read of the table loading now.
    pub rows: AtomicU64,
    /// Bytes of commits the stream holds while the tables load.
    pub held_bytes: AtomicUsize,
}

/// The server's WAL and the stream's slot, read at one moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wal {
    /// How far the server has written WAL.
    pub written: Lsn,
    /// Every commit up to here is applied by Straum.
    pub applied: Lsn,
    /// The server keeps WAL from here on for the slot: its `restart_lsn`.
    pub restart: Lsn,
}

impl Wal {
    /// Bytes of WAL the server keeps for the slot.
    pub fn retained(&self) -> u64 {
        self.written.0.saturating_sub(self.restart.0)
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
    pub committed: SystemTime,
    pub changes: Vec<Change>,
}

/// A change to a fact or a dim, named as in the spec. Rows hold
/// [`Fact::columns`] or [`Dim::columns`], as a load's do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    Insert {
        table: Identifier,
        row: Vec<Datum>,
    },
    /// With the whole old row, under `REPLICA IDENTITY FULL`.
    Update {
        table: Identifier,
        old: Vec<Datum>,
        new: Vec<Datum>,
    },
    Delete {
        table: Identifier,
        old: Vec<Datum>,
    },
    /// Every row is gone.
    Truncate {
        table: Identifier,
    },
    /// The table changed shape, so rows already read may no longer read the
    /// same; a column's type changed, for example. Also a change the source
    /// can't read whole: an update or delete without the old row, one
    /// that left out an unchanged `TOASTed` value with no old row to take it
    /// from, or a row whose value is stored out of line.
    Unfit {
        table: Identifier,
        drift: Drift,
    },
}

/// Why a table no longer reads as Straum holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drift {
    /// Altered or rewritten, or a change came without its old row.
    Altered,
    /// A value a column reads was renamed, so the rows held carry the old
    /// one and the rows to come the new.
    Renamed,
    /// A value a column reads is stored out of line, apart from its row,
    /// and the change carries only a pointer to it, which the source
    /// can't follow.
    OutOfLine,
}

impl Change {
    /// The table the change is to.
    pub fn table(&self) -> &Identifier {
        match self {
            Change::Insert { table, .. }
            | Change::Update { table, .. }
            | Change::Delete { table, .. }
            | Change::Truncate { table }
            | Change::Unfit { table, .. } => table,
        }
    }
}

impl fmt::Display for Drift {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Drift::Altered => formatter.write_str(
                "the stream no longer carries what Straum reads of it: the table was altered \
                 or rewritten, or a change came without its old row",
            ),
            Drift::Renamed => formatter.write_str(
                "a value of a column Straum reads was renamed, and the rows held carry the \
                 old one",
            ),
            Drift::OutOfLine => formatter.write_str(
                "a value of a column Straum reads is stored out of line, and the stream \
                 holds only a pointer to it, not the value",
            ),
        }
    }
}

/// As PostgreSQL prints it: the high and low 32 bits in hex.
impl fmt::Display for Lsn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:X}/{:X}", self.0 >> 32, self.0 & 0xFFFF_FFFF)
    }
}
