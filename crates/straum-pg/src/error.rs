use std::fmt;
use std::io;

use straum_app::Lsn;
use straum_domain::RowError;

use crate::pgoutput::DecodeError;
use crate::value::ValueError;
use crate::wal::WalError;

/// Whatever stops straum-pg. Straum starts over from a new stream after one.
#[derive(Debug)]
pub(crate) enum Error {
    Io(io::Error),
    /// An error PostgreSQL sent.
    Server {
        code: String,
        message: String,
    },
    /// PostgreSQL sent something Straum didn't expect.
    Protocol(String),
    Decode(DecodeError),
    Value(ValueError),
    Wal(WalError),
    /// A loaded row that doesn't fit its table in straum.
    Row(RowError),
    /// TLS didn't start: the server has it off, or Straum doesn't trust its
    /// certificate.
    Tls(String),
    /// The setup doesn't fit the spec; one line per problem, saying what to
    /// fix.
    Setup(Vec<String>),
    /// A transaction changed more rows Straum reads than
    /// `--max-transaction-rows` allows. Starting over is the cheap way
    /// through it: the new slot's snapshot is after the commit, so its rows
    /// come in the bulk load instead of being held and applied under the
    /// write lock.
    Transaction {
        lsn: Lsn,
        limit: usize,
    },
    /// The physical stream holds more rows than `--max-page-memory`
    /// allows: pages, and changes of transactions not ended. Starting over
    /// lets go of them all.
    Memory {
        held: usize,
        limit: usize,
    },
    /// The thread reading the stream has stopped.
    Stopped,
}

impl Error {
    pub(crate) fn protocol(what: &str) -> Error {
        Error::Protocol(what.to_owned())
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Error {
        Error::Io(error)
    }
}

impl From<DecodeError> for Error {
    fn from(error: DecodeError) -> Error {
        Error::Decode(error)
    }
}

impl From<ValueError> for Error {
    fn from(error: ValueError) -> Error {
        Error::Value(error)
    }
}

impl From<WalError> for Error {
    fn from(error: WalError) -> Error {
        Error::Wal(error)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(error) => write!(formatter, "connection to PostgreSQL: {error}"),
            Error::Server { code, message } => write!(formatter, "PostgreSQL: {message} ({code})"),
            Error::Protocol(what) => write!(formatter, "PostgreSQL sent {what}"),
            Error::Decode(error) => write!(formatter, "{error}"),
            Error::Value(error) => write!(formatter, "{error}"),
            Error::Wal(error) => write!(formatter, "{error}"),
            Error::Row(error) => write!(formatter, "loaded {error}"),
            Error::Tls(error) => write!(formatter, "TLS to PostgreSQL: {error}"),
            Error::Setup(problems) => formatter.write_str(&problems.join("\n")),
            Error::Transaction { lsn, limit } => write!(
                formatter,
                "transaction {lsn} changes more than {limit} rows Straum reads, \
                 the --max-transaction-rows limit; Straum starts over from a snapshot after it"
            ),
            Error::Memory { held, limit } => write!(
                formatter,
                "the physical stream holds {held} bytes of rows, past the --max-page-memory \
                 limit of {limit}; Straum starts over"
            ),
            Error::Stopped => formatter.write_str("the replication stream stopped"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;
    use straum_domain::ColumnType;

    #[test]
    fn display() {
        let cases = [
            (
                Error::from(io::Error::new(io::ErrorKind::BrokenPipe, "gone")),
                "connection to PostgreSQL: gone",
            ),
            (
                Error::Server {
                    code: "42P01".into(),
                    message: "relation \"x\" does not exist".into(),
                },
                "PostgreSQL: relation \"x\" does not exist (42P01)",
            ),
            (Error::Protocol("bytes".into()), "PostgreSQL sent bytes"),
            (
                Error::Memory {
                    held: 2048,
                    limit: 1024,
                },
                "the physical stream holds 2048 bytes of rows, past the --max-page-memory \
                 limit of 1024; Straum starts over",
            ),
            (
                Error::from(DecodeError::Short),
                "pgoutput message ends early",
            ),
            (
                Error::from(crate::value::read(ColumnType::Bool, "x").unwrap_err()),
                r#"can't read "x" as a boolean"#,
            ),
            (
                Error::Row(RowError::Missing),
                "loaded removed row was never added",
            ),
            (Error::Tls("no".into()), "TLS to PostgreSQL: no"),
            (Error::Setup(vec!["one".into(), "two".into()]), "one\ntwo"),
            (
                Error::Transaction {
                    lsn: Lsn(0x16_B374_D848),
                    limit: 100_000,
                },
                "transaction 16/B374D848 changes more than 100000 rows Straum reads, \
                 the --max-transaction-rows limit; Straum starts over from a snapshot after it",
            ),
            (Error::Stopped, "the replication stream stopped"),
        ];
        for (error, message) in cases {
            assert_eq!(error.to_string(), message);
        }
    }
}
