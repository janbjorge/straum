//! What PostgreSQL can send that Straum doesn't expect: a message out of
//! order, a reply cut short, a result of the wrong shape. Each one is a
//! case of `Unexpected`; `Error::Protocol` carries it, and its words
//! finish the sentence "PostgreSQL sent ...".

use std::fmt;

use postgres_protocol::authentication::sasl::SCRAM_SHA_256;

/// One thing PostgreSQL sent that Straum didn't expect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unexpected {
    SslAnswer(u8),
    SaslMechanisms(Vec<String>),
    PasswordRequestWithoutPassword,
    SaslContinue,
    SaslFinal,
    /// `AuthenticationOk` in a SCRAM exchange the server never finished:
    /// it signed Straum in without proving it knows the password.
    ScramUnproven,
    /// `ReadyForQuery` while signing in, before `AuthenticationOk`.
    ReadyBeforeSignIn,
    AuthenticationRequest,
    MessageDuringQuery,
    MessageDuringCopy,
    NoCopyBothResponse,
    DataAfterCopy,
    CopyValueCutShort,
    CopyWithoutEnd,
    CopyNotBinary,
    CopyHeaderCutShort,
    CopyRowCutShort,
    CopyRowWidth {
        fields: i16,
        columns: usize,
    },
    TupleWidth {
        columns: usize,
        relation_columns: usize,
    },
    StreamEnded,
    SlotWithoutRestartLsn,
    NoWalSegmentSize,
    NoServerVersion,
    NoWalFlushPosition,
    WalNeverFlushed,
    LayoutShape,
    SlotWithoutConsistentPoint,
    SnapshotShape,
    NoWalPosition,
    NoWalPositionForSlot(String),
    CommitBeforeSnapshot,
    /// A catalog query, named, whose rows don't read as Straum asked.
    CatalogShape(&'static str),
}

impl fmt::Display for Unexpected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Unexpected::SslAnswer(answer) => {
                write!(formatter, "{answer:#04x} as its answer to SSLRequest")
            }
            Unexpected::SaslMechanisms(offered) => write!(
                formatter,
                "SASL mechanisms {offered:?}; Straum needs {SCRAM_SHA_256}"
            ),
            Unexpected::PasswordRequestWithoutPassword => {
                formatter.write_str("a password request; the DSN has none")
            }
            Unexpected::SaslContinue => formatter.write_str("SASL continue"),
            Unexpected::SaslFinal => formatter.write_str("SASL final"),
            Unexpected::ScramUnproven => formatter
                .write_str("AuthenticationOk before its SCRAM proof that it knows the password"),
            Unexpected::ReadyBeforeSignIn => {
                formatter.write_str("ReadyForQuery before AuthenticationOk")
            }
            Unexpected::AuthenticationRequest => formatter.write_str(
                "an authentication request Straum doesn't support; \
                 Straum signs in with SCRAM-SHA-256 or trust",
            ),
            Unexpected::MessageDuringQuery => formatter.write_str("a message during a query"),
            Unexpected::MessageDuringCopy => formatter.write_str("a message during a copy"),
            Unexpected::NoCopyBothResponse => {
                formatter.write_str("a message instead of CopyBothResponse")
            }
            Unexpected::DataAfterCopy => formatter.write_str("data after the end of a copy"),
            Unexpected::CopyValueCutShort => formatter.write_str("a copy value cut short"),
            Unexpected::CopyWithoutEnd => formatter.write_str("a copy without its end"),
            Unexpected::CopyNotBinary => formatter.write_str("a copy that isn't binary"),
            Unexpected::CopyHeaderCutShort => formatter.write_str("a copy header cut short"),
            Unexpected::CopyRowCutShort => formatter.write_str("a copy row cut short"),
            Unexpected::CopyRowWidth { fields, columns } => {
                write!(formatter, "a copy row of {fields} columns, not {columns}")
            }
            Unexpected::TupleWidth {
                columns,
                relation_columns,
            } => write!(
                formatter,
                "a tuple of {columns} columns for a relation of {relation_columns}"
            ),
            Unexpected::StreamEnded => formatter.write_str("a message that ended the stream"),
            Unexpected::SlotWithoutRestartLsn => {
                formatter.write_str("a physical slot without a restart LSN")
            }
            Unexpected::NoWalSegmentSize => formatter.write_str("no wal_segment_size"),
            Unexpected::NoServerVersion => formatter.write_str("no server_version_num"),
            Unexpected::NoWalFlushPosition => formatter.write_str("no WAL flush position"),
            Unexpected::WalNeverFlushed => {
                formatter.write_str("PostgreSQL never flushed the WAL up to the redo point")
            }
            Unexpected::LayoutShape => formatter.write_str("a table layout of an unknown shape"),
            Unexpected::SlotWithoutConsistentPoint => {
                formatter.write_str("a slot without a consistent point")
            }
            Unexpected::SnapshotShape => formatter.write_str("a snapshot of an unknown shape"),
            Unexpected::NoWalPosition => formatter.write_str("no WAL position"),
            Unexpected::NoWalPositionForSlot(slot) => {
                write!(formatter, "no WAL position for slot {slot}")
            }
            Unexpected::CommitBeforeSnapshot => {
                formatter.write_str("a commit before the load's snapshot")
            }
            Unexpected::CatalogShape(query) => {
                write!(formatter, "a catalog {query} result of an unexpected shape")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_says_what_postgresql_sent() {
        let cases = [
            (
                Unexpected::SslAnswer(b'X'),
                "0x58 as its answer to SSLRequest",
            ),
            (
                Unexpected::SaslMechanisms(vec!["PLAIN".into()]),
                "SASL mechanisms [\"PLAIN\"]; Straum needs SCRAM-SHA-256",
            ),
            (
                Unexpected::CopyRowWidth {
                    fields: 3,
                    columns: 2,
                },
                "a copy row of 3 columns, not 2",
            ),
            (
                Unexpected::TupleWidth {
                    columns: 1,
                    relation_columns: 4,
                },
                "a tuple of 1 columns for a relation of 4",
            ),
            (
                Unexpected::NoWalPositionForSlot("straum_1".into()),
                "no WAL position for slot straum_1",
            ),
            (
                Unexpected::CatalogShape("settings"),
                "a catalog settings result of an unexpected shape",
            ),
            (Unexpected::StreamEnded, "a message that ended the stream"),
        ];
        for (unexpected, said) in cases {
            assert_eq!(unexpected.to_string(), said);
        }
    }
}
