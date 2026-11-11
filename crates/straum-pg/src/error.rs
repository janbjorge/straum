use std::fmt;
use std::io;

use straum_app::Lsn;
use straum_domain::RowError;

use crate::pgoutput::DecodeError;
use crate::value::ValueError;

/// Whatever stops straum-pg. Straum starts over from a new stream after one.
#[derive(Debug)]
pub enum Error {
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
    /// The thread reading the stream has stopped.
    Stopped,
}

impl Error {
    pub(crate) fn protocol(what: &str) -> Error {
        Error::Protocol(what.to_owned())
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        Error::Io(e)
    }
}

impl From<DecodeError> for Error {
    fn from(e: DecodeError) -> Error {
        Error::Decode(e)
    }
}

impl From<ValueError> for Error {
    fn from(e: ValueError) -> Error {
        Error::Value(e)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "connection to PostgreSQL: {e}"),
            Error::Server { code, message } => write!(f, "PostgreSQL: {message} ({code})"),
            Error::Protocol(what) => write!(f, "PostgreSQL sent {what}"),
            Error::Decode(e) => write!(f, "{e}"),
            Error::Value(e) => write!(f, "{e}"),
            Error::Row(e) => write!(f, "loaded {e}"),
            Error::Tls(e) => write!(f, "TLS to PostgreSQL: {e}"),
            Error::Setup(problems) => f.write_str(&problems.join("\n")),
            Error::Transaction { lsn, limit } => write!(
                f,
                "transaction {lsn} changes more than {limit} rows Straum reads, \
                 the --max-transaction-rows cap; Straum loads again from a snapshot after it"
            ),
            Error::Stopped => f.write_str("the replication stream stopped"),
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
                 the --max-transaction-rows cap; Straum loads again from a snapshot after it",
            ),
            (Error::Stopped, "the replication stream stopped"),
        ];
        for (err, msg) in cases {
            assert_eq!(err.to_string(), msg);
        }
    }
}
