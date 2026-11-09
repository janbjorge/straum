use std::fmt;
use std::io;

use weir_domain::RowError;

use crate::dsn::DsnError;
use crate::pgoutput::DecodeError;
use crate::value::ValueError;

/// Whatever stops weir-pg. weir starts over from a new stream after one.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// An error PostgreSQL sent.
    Server {
        code: String,
        message: String,
    },
    /// PostgreSQL sent something weir didn't expect.
    Protocol(String),
    Decode(DecodeError),
    Value(ValueError),
    /// A loaded row that doesn't fit its table in weir.
    Row(RowError),
    Dsn(DsnError),
    /// The setup doesn't fit the spec; one line per problem, saying what to
    /// fix.
    Setup(Vec<String>),
    /// The thread reading the stream has stopped.
    Stopped,
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

impl From<DsnError> for Error {
    fn from(e: DsnError) -> Error {
        Error::Dsn(e)
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
            Error::Dsn(e) => write!(f, "{e}"),
            Error::Setup(problems) => f.write_str(&problems.join("\n")),
            Error::Stopped => f.write_str("the replication stream stopped"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::Kind;

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
                Error::from(Kind::Bool.read("x").unwrap_err()),
                r#"can't read "x" as a boolean"#,
            ),
            (
                Error::Row(RowError::Missing),
                "loaded removed row was never added",
            ),
            (Error::from(DsnError::Host), "DSN must name a host"),
            (Error::Setup(vec!["one".into(), "two".into()]), "one\ntwo"),
            (Error::Stopped, "the replication stream stopped"),
        ];
        for (err, msg) in cases {
            assert_eq!(err.to_string(), msg);
        }
    }
}
