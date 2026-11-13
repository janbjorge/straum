use std::fmt;
use std::io;

use straum_domain::RowError;

/// Whatever stops a feed, on either end of it.
#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The other end sent something the feed doesn't have.
    Protocol(String),
    /// The other end closed the feed without saying why.
    Closed,
    /// The feeder said why it can't feed a table.
    Feeder(String),
    /// Straum said why it can't take what the feeder sent.
    Straum(String),
    /// A fed row that doesn't fit its table in Straum.
    Row(RowError),
    /// The types the feeder gave a table don't fit the spec.
    Types(String),
    /// A load with no feeder connected.
    NotLoading,
}

impl Error {
    pub(crate) fn protocol(what: &str) -> Error {
        Error::Protocol(what.to_owned())
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Error {
        // The other end going away reads as a short read, and writes as
        // a broken pipe or a reset.
        match e.kind() {
            io::ErrorKind::UnexpectedEof
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset => Error::Closed,
            _ => Error::Io(e),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "the feed: {e}"),
            Error::Protocol(what) => write!(f, "the feed carried {what}"),
            Error::Closed => f.write_str("the feed closed before every table was loaded"),
            Error::Feeder(why) => write!(f, "the feeder: {why}"),
            Error::Straum(why) => write!(f, "Straum: {why}"),
            Error::Row(e) => write!(f, "fed {e}"),
            Error::Types(why) => f.write_str(why),
            Error::NotLoading => f.write_str("no feeder is connected"),
        }
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_say_which_end_failed() {
        let shown = [
            Error::Io(io::Error::other("reset")),
            Error::protocol("an unknown frame 9"),
            Error::Closed,
            Error::Feeder("no table public.trips".into()),
            Error::Straum("fed row has 2 columns, expected 3".into()),
            Error::Row(RowError::Arity {
                expected: 3,
                found: 2,
            }),
            Error::Types("trips: wrong".into()),
            Error::NotLoading,
        ]
        .map(|e| e.to_string());
        assert_eq!(
            shown,
            [
                "the feed: reset",
                "the feed carried an unknown frame 9",
                "the feed closed before every table was loaded",
                "the feeder: no table public.trips",
                "Straum: fed row has 2 columns, expected 3",
                "fed row has 2 columns, expected 3",
                "trips: wrong",
                "no feeder is connected",
            ]
        );
    }

    #[test]
    fn the_other_end_going_away_is_a_closed_feed() {
        for gone in [
            io::ErrorKind::UnexpectedEof,
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::ConnectionReset,
        ] {
            assert!(matches!(Error::from(io::Error::from(gone)), Error::Closed));
        }
        let other = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(matches!(Error::from(other), Error::Io(_)));
    }
}
