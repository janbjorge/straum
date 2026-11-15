//! pgoutput protocol version 1, the logical replication messages inside
//! `XLogData`. Text tuples only: Straum never asks for `binary`.
//! <https://www.postgresql.org/docs/18/protocol-logicalrep-message-formats.html>

use std::fmt;
use std::str;
use std::time::SystemTime;

use straum_domain::Datum;

use crate::value;

/// The sizes of fields Straum steps over.
const LSN_SIZE: usize = 8;
const TIMESTAMP_SIZE: usize = 8;
const XID_SIZE: usize = 4;
const FLAGS_SIZE: usize = 1;

/// One pgoutput message, borrowing its text from the received bytes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Message<'a> {
    Begin {
        /// The LSN of the transaction's commit record, which names it.
        final_lsn: u64,
    },
    Commit {
        /// The end of the commit record.
        end_lsn: u64,
        committed: SystemTime,
    },
    Relation(Relation),
    Insert {
        relation: u32,
        new: Tuple<'a>,
    },
    Update {
        relation: u32,
        old: Option<Old<'a>>,
        new: Tuple<'a>,
    },
    Delete {
        relation: u32,
        old: Old<'a>,
    },
    Truncate {
        relations: Vec<u32>,
    },
    /// Origin, Type and logical decoding messages, which Straum doesn't use.
    Other,
}

/// A table's shape, sent before its first change and again after it changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Relation {
    pub(crate) id: u32,
    pub(crate) schema: String,
    pub(crate) name: String,
    pub(crate) columns: Vec<RelationColumn>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelationColumn {
    pub(crate) name: String,
    pub(crate) type_oid: u32,
    pub(crate) typmod: i32,
}

/// The row before an update or delete.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Old<'a> {
    /// The replica identity columns; the others are null.
    Key(Tuple<'a>),
    /// The whole row, under `REPLICA IDENTITY FULL`.
    Row(Tuple<'a>),
}

pub(crate) type Tuple<'a> = Vec<Field<'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Field<'a> {
    Null,
    /// A TOAST value the change left alone, so PostgreSQL didn't send it.
    Unchanged,
    Text(&'a str),
    /// A value the physical stream read off a page, already typed.
    Datum(Datum),
}

/// What a `CopyData` on a replication connection holds.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CopyMessage<'a> {
    /// `XLogData`: the bytes of the stream from `start`. On a logical
    /// stream they are one pgoutput message, for [`decode`].
    XLogData { start: u64, bytes: &'a [u8] },
    /// The server's position, and whether it asks for a status update now.
    Keepalive { end: u64, reply: bool },
}

pub(crate) fn copy_message(bytes: &[u8]) -> Result<CopyMessage<'_>, DecodeError> {
    let mut reader = Reader { bytes };
    match reader.u8()? {
        b'w' => {
            let start = reader.u64()?;
            // The WAL end and the send time.
            reader.skip(LSN_SIZE + TIMESTAMP_SIZE)?;
            Ok(CopyMessage::XLogData {
                start,
                bytes: reader.bytes,
            })
        }
        b'k' => {
            let end = reader.u64()?;
            // The send time.
            reader.skip(TIMESTAMP_SIZE)?;
            let reply = reader.u8()? == 1;
            Ok(CopyMessage::Keepalive { end, reply })
        }
        other => Err(DecodeError::Tag(other)),
    }
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Message<'_>, DecodeError> {
    let mut reader = Reader { bytes };
    let message = match reader.u8()? {
        b'B' => {
            let final_lsn = reader.u64()?;
            // Commit time, xid.
            reader.skip(TIMESTAMP_SIZE + XID_SIZE)?;
            Message::Begin { final_lsn }
        }
        b'C' => {
            // Flags and the commit record's start, then its end, then the
            // commit time.
            reader.skip(FLAGS_SIZE + LSN_SIZE)?;
            let end_lsn = reader.u64()?;
            let committed = reader.time()?;
            Message::Commit { end_lsn, committed }
        }
        b'R' => Message::Relation(reader.relation()?),
        b'I' => {
            let relation = reader.u32()?;
            reader.expect(b'N')?;
            Message::Insert {
                relation,
                new: reader.tuple()?,
            }
        }
        b'U' => reader.update()?,
        b'D' => reader.delete()?,
        b'T' => reader.truncate()?,
        b'O' | b'Y' | b'M' => return Ok(Message::Other),
        other => return Err(DecodeError::Tag(other)),
    };
    if !reader.bytes.is_empty() {
        return Err(DecodeError::Trailing);
    }
    Ok(message)
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn relation(&mut self) -> Result<Relation, DecodeError> {
        let id = self.u32()?;
        let schema = self.null_terminated()?.to_owned();
        let name = self.null_terminated()?.to_owned();
        // Replica identity setting.
        self.skip(FLAGS_SIZE)?;
        let count = self.u16()?;
        let mut columns = Vec::with_capacity(count.into());
        for _ in 0..count {
            // Flags: part of the key or not.
            self.skip(FLAGS_SIZE)?;
            columns.push(RelationColumn {
                name: self.null_terminated()?.to_owned(),
                type_oid: self.u32()?,
                typmod: self.i32()?,
            });
        }
        Ok(Relation {
            id,
            schema,
            name,
            columns,
        })
    }

    fn update(&mut self) -> Result<Message<'a>, DecodeError> {
        let relation = self.u32()?;
        let old = match self.u8()? {
            b'K' => Some(Old::Key(self.tuple()?)),
            b'O' => Some(Old::Row(self.tuple()?)),
            b'N' => None,
            other => return Err(DecodeError::Tag(other)),
        };
        if old.is_some() {
            self.expect(b'N')?;
        }
        Ok(Message::Update {
            relation,
            old,
            new: self.tuple()?,
        })
    }

    fn delete(&mut self) -> Result<Message<'a>, DecodeError> {
        let relation = self.u32()?;
        let old = match self.u8()? {
            b'K' => Old::Key(self.tuple()?),
            b'O' => Old::Row(self.tuple()?),
            other => return Err(DecodeError::Tag(other)),
        };
        Ok(Message::Delete { relation, old })
    }

    fn truncate(&mut self) -> Result<Message<'a>, DecodeError> {
        let count = self.u32()?;
        // Options: cascade, restart identity.
        self.skip(FLAGS_SIZE)?;
        let mut relations = Vec::new();
        for _ in 0..count {
            relations.push(self.u32()?);
        }
        Ok(Message::Truncate { relations })
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], DecodeError> {
        if self.bytes.len() < length {
            return Err(DecodeError::Short);
        }
        let (head, rest) = self.bytes.split_at(length);
        self.bytes = rest;
        Ok(head)
    }

    fn skip(&mut self, length: usize) -> Result<(), DecodeError> {
        self.take(length).map(drop)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        let (head, rest) = self.bytes.split_first_chunk().ok_or(DecodeError::Short)?;
        self.bytes = rest;
        Ok(*head)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        self.array().map(u16::from_be_bytes)
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        self.array().map(u32::from_be_bytes)
    }

    fn i32(&mut self) -> Result<i32, DecodeError> {
        self.array().map(i32::from_be_bytes)
    }

    fn u64(&mut self) -> Result<u64, DecodeError> {
        self.array().map(u64::from_be_bytes)
    }

    /// Microseconds since PostgreSQL's epoch.
    fn time(&mut self) -> Result<SystemTime, DecodeError> {
        self.array().map(i64::from_be_bytes).map(value::system_time)
    }

    fn expect(&mut self, tag: u8) -> Result<(), DecodeError> {
        match self.u8()? {
            found if found == tag => Ok(()),
            other => Err(DecodeError::Tag(other)),
        }
    }

    fn text(bytes: &[u8]) -> Result<&str, DecodeError> {
        str::from_utf8(bytes).map_err(|_| DecodeError::Utf8)
    }

    /// A null-terminated string.
    fn null_terminated(&mut self) -> Result<&'a str, DecodeError> {
        let Some(end) = zero_at(self.bytes) else {
            return Err(DecodeError::Short);
        };
        let text = Reader::text(self.take(end)?)?;
        // The terminating zero.
        self.skip(1)?;
        Ok(text)
    }

    fn tuple(&mut self) -> Result<Tuple<'a>, DecodeError> {
        let count = self.u16()?;
        let mut fields = Vec::with_capacity(count.into());
        for _ in 0..count {
            let field = match self.u8()? {
                b'n' => Field::Null,
                b'u' => Field::Unchanged,
                b't' => {
                    let length = self.u32()?;
                    let length = usize::try_from(length).map_err(|_| DecodeError::Short)?;
                    let bytes = self.take(length)?;
                    Field::Text(Reader::text(bytes)?)
                }
                other => return Err(DecodeError::Tag(other)),
            };
            fields.push(field);
        }
        Ok(fields)
    }
}

/// Where the first zero byte is, if there is one.
fn zero_at(bytes: &[u8]) -> Option<usize> {
    for (position, byte) in bytes.iter().enumerate() {
        if *byte == 0 {
            return Some(position);
        }
    }
    None
}

/// pgoutput sent bytes Straum can't read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecodeError {
    /// The message ends early.
    Short,
    /// Bytes are left after the message.
    Trailing,
    /// A message, tuple or field tag Straum doesn't know.
    Tag(u8),
    Utf8,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Short => formatter.write_str("pgoutput message ends early"),
            DecodeError::Trailing => formatter.write_str("pgoutput message has trailing bytes"),
            DecodeError::Tag(tag) => {
                write!(formatter, "unknown pgoutput tag {:?}", char::from(*tag))
            }
            DecodeError::Utf8 => formatter.write_str("pgoutput text is not UTF-8"),
        }
    }
}

impl std::error::Error for DecodeError {}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    /// Builds messages the way pgoutput writes them.
    #[derive(Default)]
    struct Bytes(Vec<u8>);

    impl Bytes {
        fn tag(mut self, tag: u8) -> Bytes {
            self.0.push(tag);
            self
        }

        fn u16(mut self, value: u16) -> Bytes {
            self.0.extend(value.to_be_bytes());
            self
        }

        fn u32(mut self, value: u32) -> Bytes {
            self.0.extend(value.to_be_bytes());
            self
        }

        fn i32(mut self, value: i32) -> Bytes {
            self.0.extend(value.to_be_bytes());
            self
        }

        fn u64(mut self, value: u64) -> Bytes {
            self.0.extend(value.to_be_bytes());
            self
        }

        fn i64(mut self, value: i64) -> Bytes {
            self.0.extend(value.to_be_bytes());
            self
        }

        fn null_terminated(mut self, text: &str) -> Bytes {
            self.0.extend(text.as_bytes());
            self.0.push(0);
            self
        }

        fn text(self, text: &str) -> Bytes {
            let length = u32::try_from(text.len()).unwrap();
            let mut bytes = self.tag(b't').u32(length);
            bytes.0.extend(text.as_bytes());
            bytes
        }

        /// `id`, null, unchanged.
        fn tuple(self, id: &str) -> Bytes {
            self.u16(3).text(id).tag(b'n').tag(b'u')
        }
    }

    fn tuple(id: &str) -> Tuple<'_> {
        vec![Field::Text(id), Field::Null, Field::Unchanged]
    }

    fn assert_decodes(cases: Vec<(Bytes, Message)>) {
        for (bytes, message) in cases {
            assert_eq!(decode(&bytes.0), Ok(message));
        }
    }

    #[test]
    fn decodes_begin_and_commit() {
        assert_decodes(vec![
            (
                Bytes::default().tag(b'B').u64(1).u64(2).u32(3),
                Message::Begin { final_lsn: 1 },
            ),
            (
                Bytes::default().tag(b'C').tag(0).u64(10).u64(20).u64(30),
                Message::Commit {
                    end_lsn: 20,
                    committed: UNIX_EPOCH + Duration::from_micros(946_684_800_000_030),
                },
            ),
            (
                // 1999-12-31 23:59:59 UTC.
                Bytes::default()
                    .tag(b'C')
                    .tag(0)
                    .u64(10)
                    .u64(20)
                    .i64(-1_000_000),
                Message::Commit {
                    end_lsn: 20,
                    committed: UNIX_EPOCH + Duration::from_secs(946_684_799),
                },
            ),
        ]);
    }

    #[test]
    fn decodes_a_relation() {
        let relation = Bytes::default()
            .tag(b'R')
            .u32(16_384)
            .null_terminated("public")
            .null_terminated("zones")
            .tag(b'd')
            .u16(2)
            .tag(1)
            .null_terminated("id")
            .u32(23)
            .i32(-1)
            .tag(0)
            .null_terminated("fare")
            .u32(1700)
            .i32(786_438);
        assert_decodes(vec![(
            relation,
            Message::Relation(Relation {
                id: 16_384,
                schema: "public".into(),
                name: "zones".into(),
                columns: vec![
                    RelationColumn {
                        name: "id".into(),
                        type_oid: 23,
                        typmod: -1,
                    },
                    RelationColumn {
                        name: "fare".into(),
                        type_oid: 1700,
                        typmod: 786_438,
                    },
                ],
            }),
        )]);
    }

    #[test]
    fn decodes_row_changes() {
        assert_decodes(vec![
            (
                Bytes::default().tag(b'I').u32(7).tag(b'N').tuple("1"),
                Message::Insert {
                    relation: 7,
                    new: tuple("1"),
                },
            ),
            (
                Bytes::default().tag(b'U').u32(7).tag(b'N').tuple("2"),
                Message::Update {
                    relation: 7,
                    old: None,
                    new: tuple("2"),
                },
            ),
            (
                Bytes::default()
                    .tag(b'U')
                    .u32(7)
                    .tag(b'K')
                    .tuple("1")
                    .tag(b'N')
                    .tuple("2"),
                Message::Update {
                    relation: 7,
                    old: Some(Old::Key(tuple("1"))),
                    new: tuple("2"),
                },
            ),
            (
                Bytes::default()
                    .tag(b'U')
                    .u32(7)
                    .tag(b'O')
                    .tuple("1")
                    .tag(b'N')
                    .tuple("2"),
                Message::Update {
                    relation: 7,
                    old: Some(Old::Row(tuple("1"))),
                    new: tuple("2"),
                },
            ),
            (
                Bytes::default().tag(b'D').u32(7).tag(b'K').tuple("1"),
                Message::Delete {
                    relation: 7,
                    old: Old::Key(tuple("1")),
                },
            ),
            (
                Bytes::default().tag(b'D').u32(7).tag(b'O').tuple("1"),
                Message::Delete {
                    relation: 7,
                    old: Old::Row(tuple("1")),
                },
            ),
        ]);
    }

    #[test]
    fn decodes_truncates_and_skips_the_rest() {
        assert_decodes(vec![
            (
                Bytes::default().tag(b'T').u32(2).tag(0).u32(7).u32(8),
                Message::Truncate {
                    relations: vec![7, 8],
                },
            ),
            (
                Bytes::default().tag(b'O').u64(1).null_terminated("x"),
                Message::Other,
            ),
            (Bytes::default().tag(b'Y'), Message::Other),
            (Bytes::default().tag(b'M'), Message::Other),
        ]);
    }

    #[test]
    fn rejects_bad_bytes() {
        let cases = [
            (Bytes::default(), DecodeError::Short),
            (Bytes::default().tag(b'B').u64(1), DecodeError::Short),
            (
                Bytes::default().tag(b'B').u64(1).u64(2).u32(3).tag(0),
                DecodeError::Trailing,
            ),
            (Bytes::default().tag(b'S'), DecodeError::Tag(b'S')),
            (
                Bytes::default().tag(b'I').u32(7).tag(b'X'),
                DecodeError::Tag(b'X'),
            ),
            (
                Bytes::default().tag(b'U').u32(7).tag(b'X'),
                DecodeError::Tag(b'X'),
            ),
            (
                Bytes::default()
                    .tag(b'U')
                    .u32(7)
                    .tag(b'K')
                    .tuple("1")
                    .tag(b'X'),
                DecodeError::Tag(b'X'),
            ),
            (
                Bytes::default().tag(b'D').u32(7).tag(b'N').tuple("1"),
                DecodeError::Tag(b'N'),
            ),
            (
                Bytes::default().tag(b'I').u32(7).tag(b'N').u16(1).tag(b'b'),
                DecodeError::Tag(b'b'),
            ),
            (
                Bytes::default()
                    .tag(b'I')
                    .u32(7)
                    .tag(b'N')
                    .u16(1)
                    .tag(b't')
                    .u32(9),
                DecodeError::Short,
            ),
            (
                Bytes::default().tag(b'R').u32(1).tag(b'p'),
                DecodeError::Short,
            ),
        ];
        for (bytes, error) in cases {
            assert_eq!(decode(&bytes.0), Err(error), "{:?}", bytes.0);
        }
        let mut invalid = Bytes::default().tag(b'R').u32(1).0;
        invalid.extend([0xff, 0]);
        assert_eq!(decode(&invalid), Err(DecodeError::Utf8));
        let mut invalid = Bytes::default()
            .tag(b'I')
            .u32(7)
            .tag(b'N')
            .u16(1)
            .tag(b't')
            .u32(1)
            .0;
        invalid.push(0xff);
        assert_eq!(decode(&invalid), Err(DecodeError::Utf8));
    }

    #[test]
    fn reads_copy_messages() {
        let xlog = Bytes::default().tag(b'w').u64(1).u64(2).i64(3).tag(b'B');
        assert_eq!(
            copy_message(&xlog.0),
            Ok(CopyMessage::XLogData {
                start: 1,
                bytes: b"B"
            })
        );
        for (reply, asks) in [(0, false), (1, true)] {
            let keepalive = Bytes::default().tag(b'k').u64(7).i64(3).tag(reply);
            assert_eq!(
                copy_message(&keepalive.0),
                Ok(CopyMessage::Keepalive {
                    end: 7,
                    reply: asks
                })
            );
        }
        let cases = [
            (Bytes::default(), DecodeError::Short),
            (Bytes::default().tag(b'w').u64(1), DecodeError::Short),
            (Bytes::default().tag(b'k').u64(7).i64(3), DecodeError::Short),
            (Bytes::default().tag(b'x'), DecodeError::Tag(b'x')),
        ];
        for (bytes, error) in cases {
            assert_eq!(copy_message(&bytes.0), Err(error), "{:?}", bytes.0);
        }
    }

    #[test]
    fn error_display() {
        let cases = [
            (DecodeError::Short, "pgoutput message ends early"),
            (DecodeError::Trailing, "pgoutput message has trailing bytes"),
            (DecodeError::Tag(b'X'), "unknown pgoutput tag 'X'"),
            (DecodeError::Utf8, "pgoutput text is not UTF-8"),
        ];
        for (error, message) in cases {
            assert_eq!(error.to_string(), message);
        }
    }
}
