//! The setup check's problems: each thing wrong with PostgreSQL's setup
//! for the spec, and what to change. `catalog.rs` finds most of them in
//! PostgreSQL's catalog (its system tables), the decoder one more as the
//! stream runs, and a physical start the ones about waiting. Each prints as
//! one line of `Error::Setup`.

use std::fmt;

use straum_domain::{ColumnType, Identifier};

use crate::catalog::Purpose;

/// A transaction id, as PostgreSQL numbers transactions in its WAL.
pub(crate) type Xid = u32;

/// One line of a failed setup check, naming what is wrong and what to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Problem {
    Server(ServerProblem),
    Table {
        table: TableNames,
        problem: TableProblem,
    },
    Column {
        table: TableNames,
        column: Identifier,
        problem: ColumnProblem,
    },
    View {
        view: Identifier,
        problem: ViewProblem,
    },
    /// A physical start waited this long and gave up.
    Wait {
        waited_seconds: u64,
        problem: WaitProblem,
    },
}

/// How a problem names a table: the spec's name, then `schema.table`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TableNames {
    pub(crate) name: Identifier,
    pub(crate) qualified: String,
}

impl TableNames {
    /// `problem` with this table as a whole.
    pub(crate) fn table_problem(&self, problem: TableProblem) -> Problem {
        Problem::Table {
            table: self.clone(),
            problem,
        }
    }
}

/// What is wrong with the server's version, settings or publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ServerProblem {
    /// Older than 17 (ADR-0007).
    TooOld {
        major: u32,
    },
    /// Below what the replication needs; `enough` lists what would do.
    WalLevel {
        wal_level: String,
        enough: &'static [&'static str],
    },
    NoPublication {
        publication: String,
    },
    /// The publication leaves out inserts, updates, deletes or truncates.
    PublicationActions {
        publication: String,
    },
    /// The physical stream needs a page whole after each checkpoint.
    FullPageWrites {
        setting: String,
    },
    /// The physical stream can't read pages compressed with zstd.
    WalCompressionZstd,
}

/// What is wrong with a spec table as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TableProblem {
    Missing,
    NotPlain,
    /// Another table inherits from it, whose rows a query of it reads.
    Children,
    NoSelect,
    /// Row-level security filters what Straum's role, `role`, reads of
    /// it.
    RowSecurity {
        role: String,
    },
    RowFilter {
        publication: String,
    },
    NotPublished {
        publication: String,
    },
    /// No `REPLICA IDENTITY FULL`; `kind` is "fact" or "dim".
    ReplicaIdentity {
        kind: &'static str,
    },
}

/// What is wrong with one column of a spec table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ColumnProblem {
    Missing,
    NotPublished {
        publication: String,
    },
    /// A type the spec can't read the column as. `type_name` is as
    /// `format_type` prints it.
    Type {
        type_name: String,
        fix: ColumnFix,
    },
    /// Dropped or changed type while the stream ran.
    Changed,
}

/// What to change about a column's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ColumnFix {
    /// Written `::text` but no enum.
    DropText,
    /// An enum not written `::text`.
    WriteText,
    /// A type the column's purpose doesn't allow.
    Needs(Purpose),
}

/// What is wrong with a view's joins or filters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ViewProblem {
    /// A join whose two sides read as different types. `via` is the column
    /// the join reads its key from, `alias.column` for a chain.
    JoinTypes {
        join: Identifier,
        via: String,
        via_type: ColumnType,
        key: Identifier,
        key_type: ColumnType,
    },
    /// A filter literal, as SQL writes it, that doesn't fit its column.
    FilterType {
        filter: Filter,
        key: Identifier,
        column_type: ColumnType,
        literal: String,
    },
    /// A filter text that is no label of its enum column's type.
    NotALabel {
        filter: Filter,
        key: Identifier,
        literal: String,
        type_name: String,
    },
}

/// Which filter of a view a problem is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Filter {
    Measure(Identifier),
    Where,
}

/// What a physical start waited for in vain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WaitProblem {
    /// PostgreSQL to log which transactions run.
    NotLogged,
    /// These transactions, open before the start, to end.
    OpenTransactions(Vec<Xid>),
    /// These transactions, whose commits the stream read, to show in a
    /// snapshot: the load's, or that of the layouts read after DDL.
    InvisibleCommits(Vec<Xid>),
}

impl fmt::Display for Problem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Problem::Server(problem) => write!(formatter, "{problem}"),
            Problem::Table { table, problem } => write_table(table, problem, formatter),
            Problem::Column {
                table,
                column,
                problem,
            } => write_column(table, column, problem, formatter),
            Problem::View { view, problem } => write!(formatter, "view {view}: {problem}"),
            Problem::Wait {
                waited_seconds,
                problem,
            } => write_wait(*waited_seconds, problem, formatter),
        }
    }
}

impl fmt::Display for ServerProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ServerProblem::TooOld { major } => write!(
                formatter,
                "PostgreSQL {major} is too old; Straum needs 17 or newer"
            ),
            ServerProblem::WalLevel { wal_level, enough } => write!(
                formatter,
                "wal_level is {wal_level}; Straum needs {}, which takes a restart",
                enough.join(" or ")
            ),
            ServerProblem::NoPublication { publication } => write!(
                formatter,
                "publication {publication} does not exist; run the SQL straum plan prints"
            ),
            ServerProblem::PublicationActions { publication } => write!(
                formatter,
                "publication {publication} must publish insert, update, delete and truncate"
            ),
            ServerProblem::FullPageWrites { setting } => write!(
                formatter,
                "full_page_writes is {setting}; --wal physical needs on"
            ),
            ServerProblem::WalCompressionZstd => formatter
                .write_str("wal_compression is zstd; --wal physical reads off, pglz and lz4"),
        }
    }
}

fn write_table(
    table: &TableNames,
    problem: &TableProblem,
    formatter: &mut fmt::Formatter<'_>,
) -> fmt::Result {
    let TableNames { name, qualified } = table;
    match problem {
        TableProblem::Missing => write!(formatter, "{name}: table {qualified} does not exist"),
        TableProblem::NotPlain => write!(formatter, "{name}: {qualified} is not a plain table"),
        TableProblem::Children => write!(
            formatter,
            "{name}: tables inherit from {qualified}; Straum reads a table without children"
        ),
        TableProblem::NoSelect => write!(
            formatter,
            "{name}: Straum's role can't read {qualified}: GRANT SELECT ON {qualified}"
        ),
        TableProblem::RowSecurity { role } => write!(
            formatter,
            "{name}: row-level security filters what {role} reads of {qualified}; \
             Straum needs every row: ALTER ROLE {role} BYPASSRLS, as a superuser"
        ),
        TableProblem::RowFilter { publication } => write!(
            formatter,
            "{name}: publication {publication} filters the rows of {qualified}; \
             Straum needs every row"
        ),
        TableProblem::NotPublished { publication } => write!(
            formatter,
            "{name}: publication {publication} leaves out {qualified}: \
             ALTER PUBLICATION {publication} ADD TABLE {qualified}"
        ),
        TableProblem::ReplicaIdentity { kind } => write!(
            formatter,
            "{kind} {name}: Straum needs the whole old row of every update and delete: \
             ALTER TABLE {qualified} REPLICA IDENTITY FULL"
        ),
    }
}

fn write_column(
    table: &TableNames,
    column: &Identifier,
    problem: &ColumnProblem,
    formatter: &mut fmt::Formatter<'_>,
) -> fmt::Result {
    let TableNames { name, qualified } = table;
    match problem {
        ColumnProblem::Missing => write!(formatter, "{name}: {qualified} has no column {column}"),
        ColumnProblem::NotPublished { publication } => write!(
            formatter,
            "{name}: publication {publication} leaves out column {column} of {qualified}"
        ),
        ColumnProblem::Type { type_name, fix } => {
            let fix = match fix {
                ColumnFix::DropText => ", not an enum; drop the ::text".to_owned(),
                ColumnFix::WriteText => format!(", an enum; write {column}::text"),
                ColumnFix::Needs(purpose) => format!("; {}", purpose.needs()),
            };
            write!(
                formatter,
                "{name}: column {column} of {qualified} is {type_name}{fix}"
            )
        }
        ColumnProblem::Changed => write!(
            formatter,
            "{name}: column {column} of {qualified} was dropped or changed type"
        ),
    }
}

impl fmt::Display for ViewProblem {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ViewProblem::JoinTypes {
                join,
                via,
                via_type,
                key,
                key_type,
            } => write!(
                formatter,
                "join {join} joins {via} ({via_type}) to {key} ({key_type}); they must read alike"
            ),
            ViewProblem::FilterType {
                filter,
                key,
                column_type,
                literal,
            } => write!(
                formatter,
                "{filter} compares {key} ({column_type}) with {literal}"
            ),
            ViewProblem::NotALabel {
                filter,
                key,
                literal,
                type_name,
            } => write!(
                formatter,
                "{filter} compares {key} with {literal}, which is no label of {type_name}"
            ),
        }
    }
}

impl fmt::Display for Filter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Filter::Measure(name) => write!(formatter, "measure {name}"),
            Filter::Where => formatter.write_str("WHERE"),
        }
    }
}

fn write_wait(
    waited_seconds: u64,
    problem: &WaitProblem,
    formatter: &mut fmt::Formatter<'_>,
) -> fmt::Result {
    match problem {
        WaitProblem::NotLogged => write!(
            formatter,
            "--wal physical waited {waited_seconds} s for PostgreSQL to log which transactions \
             run; GRANT EXECUTE ON FUNCTION pg_log_standby_snapshot() TO Straum's role"
        ),
        WaitProblem::OpenTransactions(xids) => write!(
            formatter,
            "--wal physical waited {waited_seconds} s for transactions open before it started \
             to end: xid {}; a long or prepared one (pg_prepared_xacts) holds the start back",
            xid_list(xids)
        ),
        WaitProblem::InvisibleCommits(xids) => write!(
            formatter,
            "--wal physical waited {waited_seconds} s for transactions it read the commits of \
             to end: xid {}; a synchronous standby that doesn't answer holds them",
            xid_list(xids)
        ),
    }
}

/// Each problem as the line it prints, for tests that compare text.
#[cfg(test)]
pub(crate) fn lines(problems: &[Problem]) -> Vec<String> {
    let mut lines = Vec::new();
    for problem in problems {
        lines.push(problem.to_string());
    }
    lines
}

/// Transaction ids, comma-separated.
fn xid_list(xids: &[Xid]) -> String {
    let mut texts = Vec::new();
    for xid in xids {
        texts.push(xid.to_string());
    }
    texts.join(", ")
}
