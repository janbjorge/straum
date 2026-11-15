//! The values of a row as Straum reads them, the same [`Datum`]s the
//! logical stream reads from pgoutput's text. The row is walked as
//! `heap_deform_tuple` in `src/backend/access/common/heaptuple.c` walks
//! it; layouts from `src/include/varatt.h` and
//! `src/backend/utils/adt/numeric.c`.
//!
//! Values are read little-endian: PostgreSQL writes them in the server's
//! byte order, and the servers Straum meets are x86 or ARM.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;

use straum_domain::{ColumnType, Datum};

use super::Problem;
use super::compress;
use super::heap::HeapTuple;
use crate::value::{scaled, time};

const HAS_NULL: u16 = 0x0001;
const NATTS_MASK: u16 = 0x07FF;

/// A varlena header's first byte, little-endian (`varatt.h`): exactly
/// 0x01 for a TOAST pointer (`VARATT_IS_1B_E`), low bit set for a
/// one-byte header holding the size shifted by one (`VARATT_IS_1B`),
/// else a four-byte header holding it shifted by two, whose second bit
/// says it is compressed (`VARATT_IS_4B_C`).
const VARLENA_EXTERNAL: u8 = 0x01;
const VARLENA_ONE_BYTE: u8 = 0x01;
const VARLENA_COMPRESSED: u8 = 0x02;
const ONE_BYTE_HEADER: usize = 1;
const FOUR_BYTE_HEADER: usize = 4;
/// `varattrib_4b.va_compressed`: the header, then `va_tcinfo`.
const COMPRESSED_HEADER: usize = 8;
const TCINFO: Range<usize> = 4..8;
/// `VARTAG_ONDISK` and the size of the `varatt_external` after it.
const VARTAG_ON_DISK: u8 = 18;
const EXTERNAL_POINTER: usize = 16;
/// `va_tcinfo`: the raw size in 30 bits, the method in the top two
/// (`VARLENA_EXTSIZE_BITS`).
const RAW_SIZE_MASK: u32 = 0x3FFF_FFFF;
const METHOD_SHIFT: u32 = 30;
const METHOD_PGLZ: u32 = 0;
const METHOD_LZ4: u32 = 1;

const NUMERIC_SIGN_MASK: u16 = 0xC000;
const NUMERIC_NEG: u16 = 0x4000;
const NUMERIC_SHORT: u16 = 0x8000;
const NUMERIC_SPECIAL: u16 = 0xC000;
const NUMERIC_SHORT_SIGN: u16 = 0x2000;
const NUMERIC_SHORT_WEIGHT_SIGN: u16 = 0x0040;
const NUMERIC_SHORT_WEIGHT_MASK: u16 = 0x003F;

/// A column of the table as `pg_attribute` has it, dropped ones too:
/// a row holds their bytes until it is rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Attribute {
    /// `attlen`: bytes, -1 for a varlena, -2 for a C string.
    pub(crate) length: i16,
    /// `attalign` in bytes: 1, 2, 4 or 8.
    pub(crate) align: usize,
    pub(crate) kind: Kind,
}

/// How a column's bytes read: as a type Straum reads, as an enum's
/// label, or not at all, only stepped over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Read(ColumnType),
    Enum,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    /// Null too for a column of a [`Kind::Other`] type.
    Datum(Datum),
    /// A value TOAST keeps out of line, which the page doesn't hold.
    External,
    /// A column added after the row was written. Its value is the
    /// column's `attmissingval`, or null if it has none.
    Missing,
}

/// The values of `tuple`, one per attribute. `labels` holds the label of
/// each enum value by its `pg_enum` OID, which is what a row stores.
pub(crate) fn values(
    tuple: &HeapTuple,
    attributes: &[Attribute],
    labels: &HashMap<u32, String>,
) -> Result<Vec<Value>, Problem> {
    let short = Problem::ShortRow;
    let natts = usize::from(tuple.infomask2 & NATTS_MASK);
    if natts > attributes.len() {
        return Err(Problem::TooManyColumns);
    }
    let bitmap = if tuple.infomask & HAS_NULL == 0 {
        None
    } else {
        Some(tuple.body.get(..natts.div_ceil(8)).ok_or(short)?)
    };
    let data = tuple.columns().ok_or(short)?;
    let mut at = 0;
    let mut values = Vec::with_capacity(attributes.len());
    for (index, attribute) in attributes.iter().enumerate() {
        if index >= natts {
            values.push(Value::Missing);
            continue;
        }
        if bitmap.is_some_and(|bits| bits[index / 8] >> (index % 8) & 1 == 0) {
            values.push(Value::Datum(Datum::Null));
            continue;
        }
        // A varlena with a one-byte header isn't aligned, and its first
        // byte is never zero, which padding always is.
        let unaligned = attribute.length == -1 && data.get(at).is_some_and(|byte| *byte != 0);
        if !unaligned {
            at = at.next_multiple_of(attribute.align.max(1));
        }
        let rest = data.get(at..).ok_or(short)?;
        let size = match attribute.length {
            -1 => varlena_size(rest).ok_or(short)?,
            -2 => rest.iter().position(|byte| *byte == 0).ok_or(short)? + 1,
            length => usize::try_from(length).map_err(|_| Problem::UnknownLength)?,
        };
        let bytes = rest.get(..size).ok_or(short)?;
        at += size;
        values.push(value(attribute, bytes, labels)?);
    }
    Ok(values)
}

/// A varlena's size with its header: 1 + 16 bytes for a TOAST pointer,
/// else the length its one- or four-byte header holds.
fn varlena_size(bytes: &[u8]) -> Option<usize> {
    let first = *bytes.first()?;
    if first == VARLENA_EXTERNAL {
        // The header, then `va_tag`, then the pointer.
        return (*bytes.get(1)? == VARTAG_ON_DISK).then_some(2 + EXTERNAL_POINTER);
    }
    let (size, header) = if first & VARLENA_ONE_BYTE != 0 {
        (usize::from(first >> 1), ONE_BYTE_HEADER)
    } else {
        let four: [u8; FOUR_BYTE_HEADER] = *bytes.first_chunk()?;
        let size = usize::try_from(u32::from_le_bytes(four) >> 2).ok()?;
        (size, FOUR_BYTE_HEADER)
    };
    (size >= header).then_some(size)
}

/// What a varlena holds: its bytes, decompressed, or `None` for a value
/// TOAST keeps out of line. Only a compressed one is copied.
fn detoast(bytes: &[u8]) -> Result<Option<Cow<'_, [u8]>>, Problem> {
    let first = bytes[0];
    if first == VARLENA_EXTERNAL {
        return Ok(None);
    }
    if first & VARLENA_ONE_BYTE != 0 {
        return Ok(Some(Cow::Borrowed(&bytes[ONE_BYTE_HEADER..])));
    }
    if first & VARLENA_COMPRESSED == 0 {
        return Ok(Some(Cow::Borrowed(&bytes[FOUR_BYTE_HEADER..])));
    }
    let Some(info) = bytes.get(TCINFO) else {
        return Err(Problem::CompressedWithoutSize);
    };
    let info = u32::from_le_bytes([info[0], info[1], info[2], info[3]]);
    let size = usize::try_from(info & RAW_SIZE_MASK).map_err(|_| Problem::ValueTooBig)?;
    let stored = &bytes[COMPRESSED_HEADER..];
    let raw = match info >> METHOD_SHIFT {
        METHOD_PGLZ => compress::pglz(stored, size),
        METHOD_LZ4 => compress::lz4(stored, size),
        _ => return Err(Problem::UnknownCompression),
    };
    raw.map(|raw| Some(Cow::Owned(raw)))
        .ok_or(Problem::ValueNotDecompressing)
}

fn value(
    attribute: &Attribute,
    bytes: &[u8],
    labels: &HashMap<u32, String>,
) -> Result<Value, Problem> {
    let wrong = Problem::WrongLength;
    let column_type = match attribute.kind {
        Kind::Read(column_type) => column_type,
        Kind::Other => return Ok(Value::Datum(Datum::Null)),
        Kind::Enum => {
            let oid = u32::from_le_bytes(bytes.try_into().map_err(|_| wrong)?);
            let label = labels.get(&oid).ok_or(Problem::UnlabelledEnum)?;
            return Ok(Value::Datum(Datum::Text(label.as_str().into())));
        }
    };
    let datum = match column_type {
        ColumnType::Int => Datum::Int(integer(bytes).ok_or(wrong)?),
        ColumnType::Bool => match bytes {
            [byte] => Datum::Bool(*byte != 0),
            _ => return Err(wrong),
        },
        ColumnType::Time { .. } => time(i64::from_le_bytes(bytes.try_into().map_err(|_| wrong)?)),
        ColumnType::Numeric { .. } | ColumnType::Text => {
            if attribute.length != -1 {
                return Err(wrong);
            }
            let Some(raw) = detoast(bytes)? else {
                return Ok(Value::External);
            };
            match column_type {
                ColumnType::Numeric { scale } => {
                    Datum::Int(numeric(&raw, scale).ok_or(Problem::UnreadableNumeric)?)
                }
                _ => Datum::Text(
                    std::str::from_utf8(&raw)
                        .map_err(|_| Problem::NotUtf8)?
                        .into(),
                ),
            }
        }
    };
    Ok(Value::Datum(datum))
}

/// An `int2`, `int4` or `int8`, by its length.
fn integer(bytes: &[u8]) -> Option<i64> {
    if let Ok(two) = bytes.try_into() {
        return Some(i16::from_le_bytes(two).into());
    }
    if let Ok(four) = bytes.try_into() {
        return Some(i32::from_le_bytes(four).into());
    }
    Some(i64::from_le_bytes(bytes.try_into().ok()?))
}

/// A numeric as it lies on a page, short or long header then
/// base-10000 digits, in units of `10^-scale`. `NaN` and the infinities
/// are `None`, as the text reader has them.
fn numeric(bytes: &[u8], scale: u32) -> Option<i64> {
    let header = u16::from_le_bytes([*bytes.first()?, *bytes.get(1)?]);
    let (negative, weight, digits) = match header & NUMERIC_SIGN_MASK {
        NUMERIC_SPECIAL => return None,
        NUMERIC_SHORT => {
            let bits = i32::from(header & NUMERIC_SHORT_WEIGHT_MASK);
            let weight = if header & NUMERIC_SHORT_WEIGHT_SIGN == 0 {
                bits
            } else {
                bits - i32::from(NUMERIC_SHORT_WEIGHT_SIGN)
            };
            (header & NUMERIC_SHORT_SIGN != 0, weight, bytes.get(2..)?)
        }
        sign => {
            let weight = i16::from_le_bytes([*bytes.get(2)?, *bytes.get(3)?]);
            (sign == NUMERIC_NEG, weight.into(), bytes.get(4..)?)
        }
    };
    let (digits, odd) = digits.as_chunks::<2>();
    if !odd.is_empty() {
        return None;
    }
    let digits = digits.iter().map(|pair| u16::from_le_bytes(*pair));
    scaled(digits, weight, negative, scale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::test_support::{attribute, short_varlena};

    const INT: Kind = Kind::Read(ColumnType::Int);
    const TEXT: Kind = Kind::Read(ColumnType::Text);
    const TIME: Kind = Kind::Read(ColumnType::Time { zoned: true });

    /// A row of `columns`, nulls where `None`, as `heap_form_tuple` lays
    /// it out with a 24-byte header.
    fn row(attributes: &[Attribute], columns: &[Option<&[u8]>]) -> HeapTuple {
        let nulls = columns.iter().any(Option::is_none);
        let bitmap = if nulls { columns.len().div_ceil(8) } else { 0 };
        let hoff = (23 + bitmap).next_multiple_of(8);
        let mut body = vec![0; hoff - 23];
        for (index, column) in columns.iter().enumerate() {
            if column.is_some() && nulls {
                body[index / 8] |= 1 << (index % 8);
            }
        }
        let mut data = Vec::new();
        for (attribute, column) in attributes.iter().zip(columns) {
            let Some(bytes) = column else { continue };
            let short = attribute.length == -1 && bytes[0] & 0x01 == 1;
            if !short {
                data.resize(data.len().next_multiple_of(attribute.align), 0);
            }
            data.extend_from_slice(bytes);
        }
        body.extend(data);
        HeapTuple {
            infomask2: u16::try_from(columns.len()).unwrap(),
            infomask: u16::from(nulls),
            hoff: u8::try_from(hoff).unwrap(),
            body: body.into(),
        }
    }

    fn long_varlena(payload: &[u8]) -> Vec<u8> {
        let mut bytes = u32::try_from((payload.len() + 4) << 2)
            .unwrap()
            .to_le_bytes()
            .to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    fn text(value: &str) -> Value {
        Value::Datum(Datum::Text(value.into()))
    }

    fn int(value: i64) -> Value {
        Value::Datum(Datum::Int(value))
    }

    const NULL: Value = Value::Datum(Datum::Null);

    #[test]
    fn walks_aligned_columns_and_nulls() {
        let attributes = [
            attribute(2, 2, INT),
            attribute(-1, 4, TEXT),
            attribute(8, 8, INT),
            attribute(1, 1, Kind::Read(ColumnType::Bool)),
            attribute(4, 4, INT),
            attribute(-1, 4, TEXT),
            attribute(4, 4, Kind::Other),
            attribute(4, 4, Kind::Enum),
            attribute(-1, 4, TEXT),
            attribute(8, 8, TIME),
        ];
        let long = "l".repeat(200);
        let columns: [Option<&[u8]>; 10] = [
            Some(&(-7i16).to_le_bytes()),
            Some(&short_varlena("abc")),
            Some(&(-9_000_000_000i64).to_le_bytes()),
            Some(&[1]),
            None,
            Some(&long_varlena(long.as_bytes())),
            Some(&[9, 9, 9, 9]),
            Some(&77u32.to_le_bytes()),
            Some(&short_varlena("")),
            Some(&(-1_500_000i64).to_le_bytes()),
        ];
        let labels = HashMap::from([(77, "happy".to_owned())]);
        let got = values(&row(&attributes, &columns), &attributes, &labels).unwrap();
        assert_eq!(
            got,
            [
                int(-7),
                text("abc"),
                int(-9_000_000_000),
                Value::Datum(Datum::Bool(true)),
                NULL,
                text(&long),
                NULL,
                text("happy"),
                text(""),
                // Unix seconds, rounded down.
                int(946_684_798),
            ]
        );
        // A table with a column the row predates.
        let mut more = attributes.to_vec();
        more.push(attribute(4, 4, INT));
        let got = values(&row(&attributes, &columns), &more, &labels).unwrap();
        assert_eq!(got.last(), Some(&Value::Missing));
    }

    #[test]
    fn walks_c_strings_and_external_pointers() {
        let attributes = [
            attribute(-2, 1, Kind::Other),
            attribute(-1, 4, TEXT),
            attribute(4, 4, INT),
        ];
        let mut pointer = vec![0x01, VARTAG_ON_DISK];
        pointer.extend([0; EXTERNAL_POINTER]);
        let columns: [Option<&[u8]>; 3] =
            [Some(b"name\0"), Some(&pointer), Some(&5i32.to_le_bytes())];
        let got = values(&row(&attributes, &columns), &attributes, &HashMap::new()).unwrap();
        assert_eq!(got, [NULL, Value::External, int(5)]);
    }

    #[test]
    fn reads_compressed_values() {
        let attributes = [attribute(-1, 4, TEXT)];
        let plain = b"abcabcabcabc";
        // pglz: one literal group of 3 bytes, then a match of 9 back 3.
        let pglz = [0b0000_1000, b'a', b'b', b'c', 0x06, 0x03];
        // lz4: 3 literals, a match of 9 (4 + 5) back 3, no literals.
        let lz4 = [0x35, b'a', b'b', b'c', 0x03, 0x00, 0x00];
        for (method, stored) in [(METHOD_PGLZ, &pglz[..]), (METHOD_LZ4, &lz4[..])] {
            let info = u32::try_from(plain.len()).unwrap() | method << METHOD_SHIFT;
            let mut bytes = u32::try_from((stored.len() + 8) << 2 | 2)
                .unwrap()
                .to_le_bytes()
                .to_vec();
            bytes.extend(info.to_le_bytes());
            bytes.extend(stored);
            let got = values(
                &row(&attributes, &[Some(&bytes)]),
                &attributes,
                &HashMap::new(),
            );
            assert_eq!(got, Ok(vec![text("abcabcabcabc")]), "method {method}");
            let mut bad = bytes.clone();
            bad[4..8].copy_from_slice(&(info + 1).to_le_bytes());
            let got = values(
                &row(&attributes, &[Some(&bad)]),
                &attributes,
                &HashMap::new(),
            );
            assert_eq!(got, Err(Problem::ValueNotDecompressing));
        }
        let mut zstd = u32::try_from(10 << 2 | 2).unwrap().to_le_bytes().to_vec();
        // Method 2, zstd, and a raw size of 12.
        zstd.extend(((0b10 << METHOD_SHIFT) + 12u32).to_le_bytes());
        zstd.extend([0, 0]);
        let got = values(
            &row(&attributes, &[Some(&zstd)]),
            &attributes,
            &HashMap::new(),
        );
        assert_eq!(got, Err(Problem::UnknownCompression));
    }

    fn check(tuple: &HeapTuple, attributes: &[Attribute], problem: Problem) {
        assert_eq!(values(tuple, attributes, &HashMap::new()), Err(problem));
    }

    fn four_byte_row() -> HeapTuple {
        row(&[attribute(4, 4, INT)], &[Some(&1i32.to_le_bytes())])
    }

    #[test]
    fn rejects_rows_that_dont_fit() {
        let int = attribute(4, 4, INT);
        let text_column = attribute(-1, 4, TEXT);
        let fits = four_byte_row();
        check(&fits, &[], Problem::TooManyColumns);
        let mut cut = fits.clone();
        cut.body = cut.body[..cut.body.len() - 1].into();
        check(&cut, &[int], Problem::ShortRow);
        let mut no_header = fits.clone();
        no_header.hoff = 8;
        check(&no_header, &[int], Problem::ShortRow);
        let other_tag = [0x01, 1, 0, 0];
        check(
            &row(&[text_column], &[Some(&other_tag)]),
            &[text_column],
            Problem::ShortRow,
        );
        let half = [10 << 2 | 2, 0, 0, 0, 0, 0];
        check(
            &row(&[text_column], &[Some(&half)]),
            &[text_column],
            Problem::ShortRow,
        );
        // A four-byte header naming fewer bytes than itself.
        check(
            &row(&[text_column], &[Some(&[0x04, 0, 0, 0])]),
            &[text_column],
            Problem::ShortRow,
        );
    }

    #[test]
    fn rejects_values_that_dont_fit() {
        let int = attribute(4, 4, INT);
        let text_column = attribute(-1, 4, TEXT);
        let fits = four_byte_row();
        check(&fits, &[attribute(-3, 1, INT)], Problem::UnknownLength);
        check(
            &fits,
            &[attribute(4, 4, Kind::Read(ColumnType::Bool))],
            Problem::WrongLength,
        );
        check(&fits, &[attribute(4, 4, TEXT)], Problem::WrongLength);
        check(&fits, &[attribute(4, 4, TIME)], Problem::WrongLength);
        check(&fits, &[attribute(2, 2, Kind::Enum)], Problem::WrongLength);
        check(
            &fits,
            &[attribute(4, 4, Kind::Enum)],
            Problem::UnlabelledEnum,
        );
        let three = row(&[int], &[Some(&[1, 2, 3])]);
        check(&three, &[attribute(3, 1, INT)], Problem::WrongLength);
        let bad_utf8 = short_varlena("ab");
        let bad_utf8 = [bad_utf8[0], 0xFF, 0xFE];
        check(
            &row(&[text_column], &[Some(&bad_utf8)]),
            &[text_column],
            Problem::NotUtf8,
        );
        let numeric = attribute(-1, 4, Kind::Read(ColumnType::Numeric { scale: 0 }));
        check(
            &row(&[numeric], &[Some(&short_varlena("\u{1}"))]),
            &[numeric],
            Problem::UnreadableNumeric,
        );
    }

    fn long_numeric(negative: bool, scale: u16, weight: i16, digits: &[i16]) -> Vec<u8> {
        let mut payload = (scale | if negative { NUMERIC_NEG } else { 0 })
            .to_le_bytes()
            .to_vec();
        payload.extend(weight.to_le_bytes());
        for digit in digits {
            payload.extend(digit.to_le_bytes());
        }
        payload
    }

    fn short_numeric(negative: bool, scale: u16, weight: i16, digits: &[i16]) -> Vec<u8> {
        let weight_bits = u16::from_le_bytes(weight.to_le_bytes()) & 0x7F;
        let header = NUMERIC_SHORT
            | if negative { NUMERIC_SHORT_SIGN } else { 0 }
            | scale << 7
            | weight_bits;
        let mut payload = header.to_le_bytes().to_vec();
        for digit in digits {
            payload.extend(digit.to_le_bytes());
        }
        payload
    }

    #[test]
    fn reads_numerics_in_units_of_the_scale() {
        // Sign, weight, digits, scale and the integer read.
        let cases = [
            (false, 0, vec![], 0, Some(0_i64)),
            (false, 0, vec![], 2, Some(0)),
            (true, 0, vec![12, 3400], 4, Some(-123_400)),
            (false, -1, vec![1], 4, Some(1)),
            (false, -2, vec![5000], 6, Some(50)),
            (false, 1, vec![1, 7], 2, Some(1_000_700)),
            (false, 2, vec![9], 0, Some(900_000_000)),
            (true, 1, vec![12, 3456, 7890], 3, Some(-123_456_789)),
            // A digit below the scale, and past an i64.
            (false, 0, vec![12, 3400], 1, None),
            (false, 5, vec![9999], 0, None),
        ];
        for (negative, weight, digits, scale, expected) in cases {
            for payload in [
                long_numeric(negative, scale, weight, &digits),
                short_numeric(negative, scale, weight, &digits),
            ] {
                assert_eq!(numeric(&payload, scale.into()), expected, "{payload:?}");
            }
        }
        // NaN, the infinities and bytes that aren't a numeric.
        for bytes in [
            &0xC000u16.to_le_bytes()[..],
            &0xD000u16.to_le_bytes(),
            &[0],
            &[0, 0, 0],
            &[0, 0, 0, 0, 1],
            &[0, 0x80, 1],
        ] {
            assert_eq!(numeric(bytes, 0), None, "{bytes:?}");
        }
    }
}
