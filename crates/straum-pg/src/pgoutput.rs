//! pgoutput protocol version 1, the logical replication messages inside
//! XLogData. Text tuples only: straum never asks for `binary`.
//! <https://www.postgresql.org/docs/18/protocol-logicalrep-message-formats.html>

use std::fmt;
use std::str;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// PostgreSQL's epoch, 2000-01-01 UTC, in Unix seconds.
const POSTGRES_EPOCH: u64 = 946_684_800;

/// One pgoutput message, borrowing its text from the received bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Message<'a> {
    Begin,
    Commit {
        /// The end of the commit record.
        end_lsn: u64,
        time: SystemTime,
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
    /// Origin, Type and logical decoding messages, which straum doesn't use.
    Other,
}

/// A table's shape, sent before its first change and again after it changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Relation {
    pub(crate) id: u32,
    pub(crate) schema: String,
    pub(crate) name: String,
    pub(crate) columns: Vec<Column>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Column {
    pub(crate) name: String,
    pub(crate) type_oid: u32,
    pub(crate) typmod: i32,
}

/// The row before an update or delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Old<'a> {
    /// The replica identity columns; the others are null.
    Key(Tuple<'a>),
    /// The whole row, under `REPLICA IDENTITY FULL`.
    Row(Tuple<'a>),
}

pub(crate) type Tuple<'a> = Vec<Field<'a>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Field<'a> {
    Null,
    /// A TOASTed value the change left alone, so PostgreSQL didn't send it.
    Unchanged,
    Text(&'a str),
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Message<'_>, DecodeError> {
    let mut r = Reader { bytes };
    let message = match r.u8()? {
        b'B' => {
            // Final LSN, commit time, xid.
            r.skip(8 + 8 + 4)?;
            Message::Begin
        }
        b'C' => {
            // Flags and the commit record's start, then its end, then the
            // commit time.
            r.skip(1 + 8)?;
            let end_lsn = r.u64()?;
            let time = r.time()?;
            Message::Commit { end_lsn, time }
        }
        b'R' => {
            let id = r.u32()?;
            let schema = r.cstr()?.to_owned();
            let name = r.cstr()?.to_owned();
            // Replica identity setting.
            r.skip(1)?;
            let count = r.u16()?;
            let mut columns = Vec::with_capacity(count.into());
            for _ in 0..count {
                // Flags: part of the key or not.
                r.skip(1)?;
                columns.push(Column {
                    name: r.cstr()?.to_owned(),
                    type_oid: r.u32()?,
                    typmod: r.i32()?,
                });
            }
            Message::Relation(Relation {
                id,
                schema,
                name,
                columns,
            })
        }
        b'I' => {
            let relation = r.u32()?;
            r.expect(b'N')?;
            Message::Insert {
                relation,
                new: r.tuple()?,
            }
        }
        b'U' => {
            let relation = r.u32()?;
            let old = match r.u8()? {
                b'K' => Some(Old::Key(r.tuple()?)),
                b'O' => Some(Old::Row(r.tuple()?)),
                b'N' => None,
                other => return Err(DecodeError::Tag(other)),
            };
            if old.is_some() {
                r.expect(b'N')?;
            }
            Message::Update {
                relation,
                old,
                new: r.tuple()?,
            }
        }
        b'D' => {
            let relation = r.u32()?;
            let old = match r.u8()? {
                b'K' => Old::Key(r.tuple()?),
                b'O' => Old::Row(r.tuple()?),
                other => return Err(DecodeError::Tag(other)),
            };
            Message::Delete { relation, old }
        }
        b'T' => {
            let count = r.u32()?;
            // Options: cascade, restart identity.
            r.skip(1)?;
            let relations = (0..count).map(|_| r.u32()).collect::<Result<_, _>>()?;
            Message::Truncate { relations }
        }
        b'O' | b'Y' | b'M' => return Ok(Message::Other),
        other => return Err(DecodeError::Tag(other)),
    };
    if !r.bytes.is_empty() {
        return Err(DecodeError::Trailing);
    }
    Ok(message)
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        if self.bytes.len() < n {
            return Err(DecodeError::Short);
        }
        let (head, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Ok(head)
    }

    fn skip(&mut self, n: usize) -> Result<(), DecodeError> {
        self.take(n).map(drop)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], DecodeError> {
        Ok(self.take(N)?.try_into().expect("took N bytes"))
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

    /// Microseconds since [`POSTGRES_EPOCH`].
    fn time(&mut self) -> Result<SystemTime, DecodeError> {
        let micros = self.array().map(i64::from_be_bytes)?;
        let epoch = UNIX_EPOCH + Duration::from_secs(POSTGRES_EPOCH);
        let offset = Duration::from_micros(micros.unsigned_abs());
        Ok(if micros < 0 {
            epoch - offset
        } else {
            epoch + offset
        })
    }

    fn expect(&mut self, tag: u8) -> Result<(), DecodeError> {
        match self.u8()? {
            t if t == tag => Ok(()),
            other => Err(DecodeError::Tag(other)),
        }
    }

    fn text(bytes: &[u8]) -> Result<&str, DecodeError> {
        str::from_utf8(bytes).map_err(|_| DecodeError::Utf8)
    }

    /// A null-terminated string.
    fn cstr(&mut self) -> Result<&'a str, DecodeError> {
        let end = self
            .bytes
            .iter()
            .position(|&b| b == 0)
            .ok_or(DecodeError::Short)?;
        let s = Reader::text(self.take(end)?)?;
        self.skip(1)?;
        Ok(s)
    }

    fn tuple(&mut self) -> Result<Tuple<'a>, DecodeError> {
        let count = self.u16()?;
        let mut fields = Vec::with_capacity(count.into());
        for _ in 0..count {
            let field = match self.u8()? {
                b'n' => Field::Null,
                b'u' => Field::Unchanged,
                b't' => {
                    let len = self.u32()?;
                    let bytes = self.take(usize::try_from(len).map_err(|_| DecodeError::Short)?)?;
                    Field::Text(Reader::text(bytes)?)
                }
                other => return Err(DecodeError::Tag(other)),
            };
            fields.push(field);
        }
        Ok(fields)
    }
}

/// pgoutput sent bytes straum can't read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    /// The message ends early.
    Short,
    /// Bytes are left after the message.
    Trailing,
    /// A message, tuple or field tag straum doesn't know.
    Tag(u8),
    Utf8,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Short => f.write_str("pgoutput message ends early"),
            DecodeError::Trailing => f.write_str("pgoutput message has trailing bytes"),
            DecodeError::Tag(tag) => write!(f, "unknown pgoutput tag {:?}", char::from(*tag)),
            DecodeError::Utf8 => f.write_str("pgoutput text is not UTF-8"),
        }
    }
}

impl std::error::Error for DecodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds messages the way pgoutput writes them.
    #[derive(Default)]
    struct Bytes(Vec<u8>);

    impl Bytes {
        fn tag(mut self, t: u8) -> Bytes {
            self.0.push(t);
            self
        }

        fn u16(mut self, v: u16) -> Bytes {
            self.0.extend(v.to_be_bytes());
            self
        }

        fn u32(mut self, v: u32) -> Bytes {
            self.0.extend(v.to_be_bytes());
            self
        }

        fn i32(mut self, v: i32) -> Bytes {
            self.0.extend(v.to_be_bytes());
            self
        }

        fn u64(mut self, v: u64) -> Bytes {
            self.0.extend(v.to_be_bytes());
            self
        }

        fn i64(mut self, v: i64) -> Bytes {
            self.0.extend(v.to_be_bytes());
            self
        }

        fn cstr(mut self, s: &str) -> Bytes {
            self.0.extend(s.as_bytes());
            self.0.push(0);
            self
        }

        fn text(self, s: &str) -> Bytes {
            let len = u32::try_from(s.len()).unwrap();
            let mut b = self.tag(b't').u32(len);
            b.0.extend(s.as_bytes());
            b
        }

        /// `id`, null, unchanged.
        fn tuple(self, id: &str) -> Bytes {
            self.u16(3).text(id).tag(b'n').tag(b'u')
        }
    }

    fn tuple(id: &str) -> Tuple<'_> {
        vec![Field::Text(id), Field::Null, Field::Unchanged]
    }

    #[test]
    fn decodes_messages() {
        let relation = Bytes::default()
            .tag(b'R')
            .u32(16_384)
            .cstr("public")
            .cstr("zones")
            .tag(b'd')
            .u16(2)
            .tag(1)
            .cstr("id")
            .u32(23)
            .i32(-1)
            .tag(0)
            .cstr("fare")
            .u32(1700)
            .i32(786_438);
        let cases = [
            (
                Bytes::default().tag(b'B').u64(1).u64(2).u32(3),
                Message::Begin,
            ),
            (
                Bytes::default().tag(b'C').tag(0).u64(10).u64(20).u64(30),
                Message::Commit {
                    end_lsn: 20,
                    time: UNIX_EPOCH + Duration::from_micros(946_684_800_000_030),
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
                    time: UNIX_EPOCH + Duration::from_secs(946_684_799),
                },
            ),
            (
                relation,
                Message::Relation(Relation {
                    id: 16_384,
                    schema: "public".into(),
                    name: "zones".into(),
                    columns: vec![
                        Column {
                            name: "id".into(),
                            type_oid: 23,
                            typmod: -1,
                        },
                        Column {
                            name: "fare".into(),
                            type_oid: 1700,
                            typmod: 786_438,
                        },
                    ],
                }),
            ),
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
            (
                Bytes::default().tag(b'T').u32(2).tag(0).u32(7).u32(8),
                Message::Truncate {
                    relations: vec![7, 8],
                },
            ),
            (Bytes::default().tag(b'O').u64(1).cstr("x"), Message::Other),
            (Bytes::default().tag(b'Y'), Message::Other),
            (Bytes::default().tag(b'M'), Message::Other),
        ];
        for (bytes, message) in cases {
            assert_eq!(decode(&bytes.0), Ok(message));
        }
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
        for (bytes, err) in cases {
            assert_eq!(decode(&bytes.0), Err(err), "{:?}", bytes.0);
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
    fn error_display() {
        let cases = [
            (DecodeError::Short, "pgoutput message ends early"),
            (DecodeError::Trailing, "pgoutput message has trailing bytes"),
            (DecodeError::Tag(b'X'), "unknown pgoutput tag 'X'"),
            (DecodeError::Utf8, "pgoutput text is not UTF-8"),
        ];
        for (err, msg) in cases {
            assert_eq!(err.to_string(), msg);
        }
    }
}
