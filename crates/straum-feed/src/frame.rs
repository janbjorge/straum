//! What a feed carries: frames of a tag byte, a length and a payload.
//! Numbers are big-endian, and text is its length and its UTF-8.

use std::io::{Read, Write};

use straum_domain::{ColumnType, Datum, Identifier, Table};

use crate::error::Error;

/// What a feeder opens with, so a Straum of another protocol refuses it
/// before any table moves.
const HELLO: &[u8] = b"straum feed 1";

/// The largest payload read. A batch of rows is far below it; a length
/// past it is no frame, and must not be allocated.
pub(crate) const MAX_PAYLOAD: usize = 64 << 20;

const TAG_HELLO: u8 = 1;
const TAG_LOAD: u8 = 2;
const TAG_TYPES: u8 = 3;
const TAG_ROWS: u8 = 4;
const TAG_END: u8 = 5;
const TAG_DONE: u8 = 6;
const TAG_FAILED: u8 = 7;

const TYPE_INT: u8 = 0;
const TYPE_NUMERIC: u8 = 1;
const TYPE_BOOL: u8 = 2;
const TYPE_TEXT: u8 = 3;
const TYPE_TIMESTAMPTZ: u8 = 4;
const TYPE_TIMESTAMP: u8 = 5;

const VALUE_NULL: u8 = 0;
const VALUE_FALSE: u8 = 1;
const VALUE_TRUE: u8 = 2;
const VALUE_INT: u8 = 3;
const VALUE_TEXT: u8 = 4;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Frame {
    /// The feeder's first frame.
    Hello,
    /// Straum asks for a table's rows, each holding these columns in this
    /// order.
    Load {
        table: Table,
        columns: Vec<Identifier>,
    },
    /// The feeder's answer to a load: the type of each column asked for.
    Types(Vec<ColumnType>),
    /// Rows of the table asked for, laid end to end.
    Rows(Vec<Datum>),
    /// The table asked for has no more rows.
    End,
    /// Straum has every table.
    Done,
    /// Why the sender gives up. It closes after.
    Failed(String),
}

impl Frame {
    fn tag(&self) -> u8 {
        match self {
            Frame::Hello => TAG_HELLO,
            Frame::Load { .. } => TAG_LOAD,
            Frame::Types(_) => TAG_TYPES,
            Frame::Rows(_) => TAG_ROWS,
            Frame::End => TAG_END,
            Frame::Done => TAG_DONE,
            Frame::Failed(_) => TAG_FAILED,
        }
    }

    fn payload(&self, out: &mut Vec<u8>) {
        match self {
            Frame::Hello => out.extend_from_slice(HELLO),
            Frame::Load { table, columns } => {
                text(out, table.schema.as_str());
                text(out, table.name.as_str());
                for column in columns {
                    text(out, column.as_str());
                }
            }
            Frame::Types(types) => {
                for column_type in types {
                    match column_type {
                        ColumnType::Int => out.push(TYPE_INT),
                        ColumnType::Numeric { scale } => {
                            out.push(TYPE_NUMERIC);
                            out.extend_from_slice(&scale.to_be_bytes());
                        }
                        ColumnType::Bool => out.push(TYPE_BOOL),
                        ColumnType::Text => out.push(TYPE_TEXT),
                        ColumnType::Time { zoned: true } => out.push(TYPE_TIMESTAMPTZ),
                        ColumnType::Time { zoned: false } => out.push(TYPE_TIMESTAMP),
                    }
                }
            }
            Frame::Rows(rows) => rows_payload(rows, out),
            Frame::End | Frame::Done => {}
            Frame::Failed(why) => out.extend_from_slice(why.as_bytes()),
        }
    }
}

fn text(out: &mut Vec<u8>, text: &str) {
    // A text longer than a frame may be never arrives whole anyway.
    let len = u32::try_from(text.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(text.as_bytes());
}

fn rows_payload(rows: &[Datum], out: &mut Vec<u8>) {
    for datum in rows {
        match datum {
            Datum::Null => out.push(VALUE_NULL),
            Datum::Bool(false) => out.push(VALUE_FALSE),
            Datum::Bool(true) => out.push(VALUE_TRUE),
            Datum::Int(n) => {
                out.push(VALUE_INT);
                out.extend_from_slice(&n.to_be_bytes());
            }
            Datum::Text(value) => {
                out.push(VALUE_TEXT);
                text(out, value);
            }
        }
    }
}

/// Writes one frame. `buffer` is scratch, kept by the caller so a batch
/// of rows allocates nothing.
pub(crate) fn write(
    out: &mut impl Write,
    frame: &Frame,
    buffer: &mut Vec<u8>,
) -> Result<(), Error> {
    buffer.clear();
    buffer.extend_from_slice(&[frame.tag(), 0, 0, 0, 0]);
    frame.payload(buffer);
    stamp(buffer)?;
    Ok(out.write_all(buffer)?)
}

/// Writes a frame of rows without taking them into a [`Frame`].
pub(crate) fn write_rows(
    out: &mut impl Write,
    rows: &[Datum],
    buffer: &mut Vec<u8>,
) -> Result<(), Error> {
    buffer.clear();
    buffer.extend_from_slice(&[TAG_ROWS, 0, 0, 0, 0]);
    rows_payload(rows, buffer);
    stamp(buffer)?;
    Ok(out.write_all(buffer)?)
}

/// Sets the length of the frame in `buffer`, after its tag.
fn stamp(buffer: &mut [u8]) -> Result<(), Error> {
    let len = buffer.len() - 5;
    if len > MAX_PAYLOAD {
        return Err(Error::Protocol(format!("a frame of {len} bytes")));
    }
    let len = u32::try_from(len).expect("at most MAX_PAYLOAD");
    buffer[1..5].copy_from_slice(&len.to_be_bytes());
    Ok(())
}

/// Reads one frame. `buffer` is scratch, as in [`write`].
pub(crate) fn read(input: &mut impl Read, buffer: &mut Vec<u8>) -> Result<Frame, Error> {
    let mut head = [0; 5];
    input.read_exact(&mut head)?;
    let [tag, len @ ..] = head;
    let len = usize::try_from(u32::from_be_bytes(len)).unwrap_or(usize::MAX);
    if len > MAX_PAYLOAD {
        return Err(Error::Protocol(format!("a frame of {len} bytes")));
    }
    buffer.resize(len, 0);
    input.read_exact(buffer)?;
    let mut payload = Payload(buffer);
    match tag {
        TAG_HELLO if payload.0 == HELLO => Ok(Frame::Hello),
        TAG_HELLO => Err(Error::protocol("a hello of another protocol")),
        TAG_LOAD => {
            let table = Table {
                schema: payload.identifier()?,
                name: payload.identifier()?,
            };
            let mut columns = Vec::new();
            while !payload.0.is_empty() {
                columns.push(payload.identifier()?);
            }
            Ok(Frame::Load { table, columns })
        }
        TAG_TYPES => {
            let mut types = Vec::new();
            while !payload.0.is_empty() {
                types.push(payload.column_type()?);
            }
            Ok(Frame::Types(types))
        }
        TAG_ROWS => {
            let mut rows = Vec::new();
            while !payload.0.is_empty() {
                rows.push(payload.datum()?);
            }
            Ok(Frame::Rows(rows))
        }
        TAG_END => payload.empty().map(|()| Frame::End),
        TAG_DONE => payload.empty().map(|()| Frame::Done),
        TAG_FAILED => Ok(Frame::Failed(utf8(payload.0)?.to_owned())),
        tag => Err(Error::Protocol(format!("an unknown frame {tag}"))),
    }
}

fn utf8(bytes: &[u8]) -> Result<&str, Error> {
    str::from_utf8(bytes).map_err(|_| Error::protocol("text that is not UTF-8"))
}

/// What is left of a payload.
struct Payload<'a>(&'a [u8]);

impl<'a> Payload<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        let (head, rest) = (self.0)
            .split_at_checked(len)
            .ok_or_else(|| Error::protocol("a frame cut short"))?;
        self.0 = rest;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let bytes = self.take(N)?;
        Ok(bytes.try_into().expect("N bytes taken"))
    }

    fn byte(&mut self) -> Result<u8, Error> {
        self.array().map(|[byte]| byte)
    }

    fn text(&mut self) -> Result<&'a str, Error> {
        let len = u32::from_be_bytes(self.array()?);
        utf8(self.take(usize::try_from(len).unwrap_or(usize::MAX))?)
    }

    fn identifier(&mut self) -> Result<Identifier, Error> {
        let text = self.text()?;
        Identifier::new(text).ok_or_else(|| Error::Protocol(format!("the name {text:?}")))
    }

    fn column_type(&mut self) -> Result<ColumnType, Error> {
        match self.byte()? {
            TYPE_INT => Ok(ColumnType::Int),
            TYPE_NUMERIC => Ok(ColumnType::Numeric {
                scale: u32::from_be_bytes(self.array()?),
            }),
            TYPE_BOOL => Ok(ColumnType::Bool),
            TYPE_TEXT => Ok(ColumnType::Text),
            TYPE_TIMESTAMPTZ => Ok(ColumnType::Time { zoned: true }),
            TYPE_TIMESTAMP => Ok(ColumnType::Time { zoned: false }),
            other => Err(Error::Protocol(format!("an unknown column type {other}"))),
        }
    }

    fn datum(&mut self) -> Result<Datum, Error> {
        match self.byte()? {
            VALUE_NULL => Ok(Datum::Null),
            VALUE_FALSE => Ok(Datum::Bool(false)),
            VALUE_TRUE => Ok(Datum::Bool(true)),
            VALUE_INT => Ok(Datum::Int(i64::from_be_bytes(self.array()?))),
            VALUE_TEXT => Ok(Datum::Text(self.text()?.into())),
            other => Err(Error::Protocol(format!("an unknown value {other}"))),
        }
    }

    fn empty(&self) -> Result<(), Error> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(Error::protocol("bytes after a frame's end"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(name: &str) -> Identifier {
        Identifier::new(name).unwrap()
    }

    fn bytes(frame: &Frame) -> Vec<u8> {
        let mut out = Vec::new();
        write(&mut out, frame, &mut Vec::new()).unwrap();
        out
    }

    fn parse(mut bytes: &[u8]) -> Result<Frame, Error> {
        let frame = read(&mut bytes, &mut Vec::new())?;
        assert!(bytes.is_empty(), "a read takes one whole frame");
        Ok(frame)
    }

    fn refused(bytes: &[u8]) -> String {
        match parse(bytes) {
            Err(e) => e.to_string(),
            Ok(frame) => panic!("{bytes:?} read as {frame:?}"),
        }
    }

    /// splitmix64; the tests need no crate for randomness.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn text(&mut self) -> String {
            let alphabet = ['a', 'z', '_', ' ', '"', ',', '\n', 'é', '日', '\0'];
            (0..self.below(6))
                .map(|_| alphabet[self.below(alphabet.len())])
                .collect()
        }

        fn name(&mut self) -> Identifier {
            let len = 1 + self.below(8);
            let name: String = (0..len).map(|_| ['a', 'q', '_'][self.below(3)]).collect();
            id(&name)
        }

        fn datum(&mut self) -> Datum {
            match self.below(5) {
                0 => Datum::Null,
                1 => Datum::Bool(self.below(2) == 0),
                2 => Datum::Int(self.next() as i64),
                3 => Datum::Int(self.below(100) as i64 - 50),
                _ => Datum::Text(self.text().into()),
            }
        }

        fn column_type(&mut self) -> ColumnType {
            match self.below(6) {
                0 => ColumnType::Int,
                1 => ColumnType::Numeric {
                    scale: self.next() as u32,
                },
                2 => ColumnType::Bool,
                3 => ColumnType::Text,
                4 => ColumnType::Time { zoned: true },
                _ => ColumnType::Time { zoned: false },
            }
        }

        fn frame(&mut self) -> Frame {
            match self.below(7) {
                0 => Frame::Hello,
                1 => Frame::Load {
                    table: Table {
                        schema: self.name(),
                        name: self.name(),
                    },
                    columns: (0..self.below(5)).map(|_| self.name()).collect(),
                },
                2 => Frame::Types((0..self.below(5)).map(|_| self.column_type()).collect()),
                3 => Frame::Rows((0..self.below(40)).map(|_| self.datum()).collect()),
                4 => Frame::End,
                5 => Frame::Done,
                _ => Frame::Failed(self.text()),
            }
        }
    }

    #[test]
    fn frames_have_a_tag_a_length_and_a_payload() {
        assert_eq!(bytes(&Frame::End), [TAG_END, 0, 0, 0, 0]);
        assert_eq!(bytes(&Frame::Done), [TAG_DONE, 0, 0, 0, 0]);
        assert_eq!(
            bytes(&Frame::Hello),
            [&[TAG_HELLO, 0, 0, 0, 13][..], b"straum feed 1"].concat()
        );
        assert_eq!(
            bytes(&Frame::Failed("no".into())),
            [TAG_FAILED, 0, 0, 0, 2, b'n', b'o']
        );
        let load = Frame::Load {
            table: Table {
                schema: id("s"),
                name: id("t"),
            },
            columns: vec![id("c")],
        };
        assert_eq!(
            bytes(&load),
            [
                TAG_LOAD, 0, 0, 0, 15, 0, 0, 0, 1, b's', 0, 0, 0, 1, b't', 0, 0, 0, 1, b'c'
            ]
        );
        let types = Frame::Types(vec![ColumnType::Int, ColumnType::Numeric { scale: 2 }]);
        assert_eq!(bytes(&types), [TAG_TYPES, 0, 0, 0, 6, 0, 1, 0, 0, 0, 2]);
        let rows = Frame::Rows(vec![
            Datum::Null,
            Datum::Bool(true),
            Datum::Int(-2),
            Datum::Text("é".into()),
        ]);
        assert_eq!(
            bytes(&rows),
            [
                &[TAG_ROWS, 0, 0, 0, 18, VALUE_NULL, VALUE_TRUE, VALUE_INT][..],
                &(-2_i64).to_be_bytes(),
                &[VALUE_TEXT, 0, 0, 0, 2, 0xc3, 0xa9],
            ]
            .concat()
        );
    }

    /// The reference is the frame itself: whatever is written reads back
    /// equal, and a stream of frames reads back in order.
    #[test]
    fn every_frame_reads_back_as_written() {
        let mut rng = Rng(7);
        let frames: Vec<Frame> = (0..2_000).map(|_| rng.frame()).collect();
        let mut stream = Vec::new();
        let mut buffer = Vec::new();
        for frame in &frames {
            write(&mut stream, frame, &mut buffer).unwrap();
        }
        let mut input = stream.as_slice();
        for frame in &frames {
            assert_eq!(&read(&mut input, &mut buffer).unwrap(), frame);
        }
        assert!(input.is_empty());
    }

    #[test]
    fn rows_written_bare_read_as_a_frame_of_rows() {
        let mut rng = Rng(11);
        let rows: Vec<Datum> = (0..300).map(|_| rng.datum()).collect();
        let mut bare = Vec::new();
        write_rows(&mut bare, &rows, &mut Vec::new()).unwrap();
        assert_eq!(bare, bytes(&Frame::Rows(rows.clone())));
        assert_eq!(parse(&bare).unwrap(), Frame::Rows(rows));
    }

    /// A frame cut anywhere is a closed feed, never a frame and never a
    /// panic.
    #[test]
    fn a_cut_frame_is_a_closed_feed() {
        let mut rng = Rng(3);
        for _ in 0..200 {
            let whole = bytes(&rng.frame());
            for cut in 0..whole.len() {
                let mut input = &whole[..cut];
                let read = read(&mut input, &mut Vec::new());
                assert!(matches!(read, Err(Error::Closed)), "{whole:?} at {cut}");
            }
        }
    }

    /// Any byte changed gives a frame or an error, never a panic.
    #[test]
    fn a_damaged_frame_never_panics() {
        let mut rng = Rng(5);
        for _ in 0..2_000 {
            let mut damaged = bytes(&rng.frame());
            let at = rng.below(damaged.len());
            damaged[at] = rng.next() as u8;
            let _ = read(&mut damaged.as_slice(), &mut Vec::new());
        }
    }

    #[test]
    fn refuses_what_is_no_frame() {
        let cases: [(&[u8], &str); 12] = [
            (&[9, 0, 0, 0, 0], "an unknown frame 9"),
            (&[0, 0, 0, 0, 0], "an unknown frame 0"),
            (
                &[TAG_HELLO, 0, 0, 0, 1, b'x'],
                "a hello of another protocol",
            ),
            (&[TAG_HELLO, 0, 0, 0, 0], "a hello of another protocol"),
            (&[TAG_END, 0, 0, 0, 1, 0], "bytes after a frame's end"),
            (&[TAG_DONE, 0, 0, 0, 1, 0], "bytes after a frame's end"),
            (&[TAG_TYPES, 0, 0, 0, 1, 6], "an unknown column type 6"),
            (&[TAG_TYPES, 0, 0, 0, 2, 1, 0], "a frame cut short"),
            (&[TAG_ROWS, 0, 0, 0, 1, 5], "an unknown value 5"),
            (&[TAG_ROWS, 0, 0, 0, 2, 3, 0], "a frame cut short"),
            (&[TAG_ROWS, 0, 0, 0, 5, 4, 0, 0, 0, 9], "a frame cut short"),
            (&[TAG_FAILED, 0, 0, 0, 1, 0xff], "text that is not UTF-8"),
        ];
        for (bytes, what) in cases {
            assert_eq!(
                refused(bytes),
                format!("the feed carried {what}"),
                "{bytes:?}"
            );
        }
        let bad_text = [TAG_ROWS, 0, 0, 0, 6, 4, 0, 0, 0, 1, 0xff];
        assert_eq!(
            refused(&bad_text),
            "the feed carried text that is not UTF-8"
        );
        let bad_name = [TAG_LOAD, 0, 0, 0, 5, 0, 0, 0, 1, b'A'];
        assert_eq!(refused(&bad_name), "the feed carried the name \"A\"");
        let no_table = [TAG_LOAD, 0, 0, 0, 5, 0, 0, 0, 1, b's'];
        assert_eq!(refused(&no_table), "the feed carried a frame cut short");
    }

    #[test]
    fn a_frame_past_the_limit_is_refused_both_ways() {
        let len = u32::try_from(MAX_PAYLOAD + 1).unwrap().to_be_bytes();
        let head = [TAG_ROWS, len[0], len[1], len[2], len[3]];
        // The payload is never read, so it need not be there.
        let mut buffer = Vec::new();
        let read = read(&mut head.as_slice(), &mut buffer).unwrap_err();
        assert_eq!(
            read.to_string(),
            "the feed carried a frame of 67108865 bytes"
        );
        assert!(buffer.is_empty());

        let long = Frame::Failed("x".repeat(MAX_PAYLOAD + 1));
        let mut out = Vec::new();
        let written = write(&mut out, &long, &mut Vec::new()).unwrap_err();
        assert_eq!(
            written.to_string(),
            "the feed carried a frame of 67108865 bytes"
        );
        assert!(out.is_empty());
        let at_limit = Frame::Failed("x".repeat(MAX_PAYLOAD));
        write(&mut out, &at_limit, &mut Vec::new()).unwrap();
        assert_eq!(parse(&out).unwrap(), at_limit);
    }
}
