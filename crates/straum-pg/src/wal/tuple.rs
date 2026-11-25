//! The values of a row as Straum reads them, the same [`Datum`]s the load
//! reads: a heap change's row goes in, one value per column comes out, for
//! `messages.rs`. The row is walked as `heap_deform_tuple` in `heaptuple.c`
//! walks it. A variable-length value is a varlena, with a header holding
//! its length; TOAST may compress it (pglz or lz4) or keep it out of line
//! in another table, which the page doesn't hold. Layouts from `varatt.h`
//! and `numeric.c`. Values are read little-endian: PostgreSQL writes them
//! in the server's byte order, and the servers Straum meets are x86 or ARM.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;

use straum_domain::{ColumnType, Datum};

use super::compress;
use super::heap::HeapTuple;
use super::{Oid, Problem, u32_at};
use crate::pgoutput::zero_at;
use crate::value::{scaled, time};

/// `HEAP_HASNULL` in `t_infomask`: the row has a null bitmap.
const HAS_NULL: u16 = 0x0001;
/// `HEAP_COLUMN_COUNT_MASK`: the bits of `t_infomask2` that count the columns.
const COLUMN_COUNT_MASK: u16 = 0x07FF;
/// The null bitmap holds a bit per column, eight to a byte.
const BITS_PER_BYTE: usize = 8;
/// `attlen` of a varlena, a value that carries its own length.
pub(crate) const VARLENA_LENGTH: i16 = -1;
/// `attlen` of a C string, a value that ends at a zero byte.
pub(crate) const C_STRING_LENGTH: i16 = -2;

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
/// How far each header shifts the size it holds, past its flag bits.
const ONE_BYTE_SIZE_SHIFT: u8 = 1;
const FOUR_BYTE_SIZE_SHIFT: u32 = 2;
/// `varattrib_4b.va_compressed`: the header, then `va_tcinfo`.
const COMPRESSED_HEADER: usize = 8;
const TCINFO: Range<usize> = 4..8;
/// A TOAST pointer: `varattrib_1b_e`'s header byte and `va_tag`, then,
/// for `VARTAG_ONDISK`, a `varatt_external`.
const EXTERNAL_HEADER: usize = 2;
const VARTAG: usize = 1;
const VARTAG_ON_DISK: u8 = 18;
pub(super) const EXTERNAL_POINTER: usize = 16;
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
/// `NumericData`: `n_header`, then for a short numeric the digits, for a
/// long one `n_weight` and then the digits, each a two-byte `NumericDigit`.
const NUMERIC_HEADER: usize = 2;
const NUMERIC_LONG_WEIGHT: usize = 2;
const NUMERIC_LONG_DIGITS: usize = 4;
const NUMERIC_DIGIT_SIZE: usize = 2;

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

/// How a column's bytes read: as a type Straum reads, as a label of the
/// enum whose type OID it holds, or not at all, only stepped over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Read(ColumnType),
    Enum(Oid),
    Other,
}

/// One label of an enum, as `pg_enum` has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Label {
    /// The type OID of the enum it belongs to.
    pub(crate) enum_type: Oid,
    pub(crate) text: String,
}

/// One column's value in a row.
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
#[cfg(test)]
pub(crate) fn values(
    tuple: &HeapTuple,
    attributes: &[Attribute],
    labels: &HashMap<Oid, Label>,
) -> Result<Vec<Value>, Problem> {
    let mut values = Vec::new();
    values_into(tuple, attributes, labels, &mut values)?;
    Ok(values)
}

/// The values of `tuple` into `values`, emptied first, as [`values`]
/// reads them. The caller keeps the list from row to row.
pub(crate) fn values_into(
    tuple: &HeapTuple,
    attributes: &[Attribute],
    labels: &HashMap<Oid, Label>,
    values: &mut Vec<Value>,
) -> Result<(), Problem> {
    values.clear();
    let short = Problem::ShortRow;
    let column_count = usize::from(tuple.infomask2 & COLUMN_COUNT_MASK);
    if column_count > attributes.len() {
        return Err(Problem::TooManyColumns);
    }
    let bitmap = if tuple.infomask & HAS_NULL == 0 {
        None
    } else {
        Some(
            tuple
                .body
                .get(..column_count.div_ceil(BITS_PER_BYTE))
                .ok_or(short)?,
        )
    };
    let data = tuple.columns().ok_or(short)?;
    let mut at = 0;
    for (index, attribute) in attributes.iter().enumerate() {
        if index >= column_count {
            values.push(Value::Missing);
            continue;
        }
        if let Some(bits) = bitmap
            && bits[index / BITS_PER_BYTE] >> (index % BITS_PER_BYTE) & 1 == 0
        {
            values.push(Value::Datum(Datum::Null));
            continue;
        }
        // A varlena with a one-byte header isn't aligned, and its first
        // byte is never zero, which padding always is.
        let unaligned =
            attribute.length == VARLENA_LENGTH && matches!(data.get(at), Some(byte) if *byte != 0);
        if !unaligned {
            at = align_up(at, attribute.align);
        }
        let rest = data.get(at..).ok_or(short)?;
        let size = match attribute.length {
            VARLENA_LENGTH => varlena_size(rest).ok_or(short)?,
            // A C string's size with its closing zero byte.
            C_STRING_LENGTH => zero_at(rest).ok_or(short)? + 1,
            length => match usize::try_from(length) {
                Ok(length) => length,
                Err(_) => return Err(Problem::UnknownLength),
            },
        };
        let bytes = rest.get(..size).ok_or(short)?;
        at += size;
        values.push(value(attribute, bytes, labels)?);
    }
    Ok(())
}

/// `at` rounded up to `align`, 1, 2, 4 or 8, by mask: rounding by
/// division was a tenth of walking a row (`physical_wal_bench`).
fn align_up(at: usize, align: usize) -> usize {
    let mask = align.max(1) - 1;
    (at + mask) & !mask
}

/// A varlena's size with its header: 1 + 16 bytes for a TOAST pointer,
/// else the length its one- or four-byte header holds.
fn varlena_size(bytes: &[u8]) -> Option<usize> {
    let first = *bytes.first()?;
    if first == VARLENA_EXTERNAL {
        // The header, then `va_tag`, then the pointer.
        if *bytes.get(VARTAG)? != VARTAG_ON_DISK {
            return None;
        }
        return Some(EXTERNAL_HEADER + EXTERNAL_POINTER);
    }
    let (size, header) = if first & VARLENA_ONE_BYTE != 0 {
        (usize::from(first >> ONE_BYTE_SIZE_SHIFT), ONE_BYTE_HEADER)
    } else {
        let four: [u8; FOUR_BYTE_HEADER] = *bytes.first_chunk()?;
        let size = usize::try_from(u32::from_le_bytes(four) >> FOUR_BYTE_SIZE_SHIFT).ok()?;
        (size, FOUR_BYTE_HEADER)
    };
    if size < header {
        return None;
    }
    Some(size)
}

/// What a varlena holds: its bytes, decompressed, or `None` for a value
/// TOAST keeps out of line.
// `Cow`, borrowed or owned: a plain value is read where it lies in the
// row, and only a compressed one is copied out, decompressed.
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
    let Some(size_and_method) = u32_at(bytes, TCINFO.start) else {
        return Err(Problem::CompressedWithoutSize);
    };
    let Ok(size) = usize::try_from(size_and_method & RAW_SIZE_MASK) else {
        return Err(Problem::ValueTooBig);
    };
    let stored = &bytes[COMPRESSED_HEADER..];
    let raw = match size_and_method >> METHOD_SHIFT {
        METHOD_PGLZ => compress::pglz(stored, size),
        METHOD_LZ4 => compress::lz4(stored, size),
        _ => return Err(Problem::UnknownCompression),
    };
    match raw {
        Some(raw) => Ok(Some(Cow::Owned(raw))),
        None => Err(Problem::ValueNotDecompressing),
    }
}

/// The value `bytes` hold, read as `attribute` says.
fn value(
    attribute: &Attribute,
    bytes: &[u8],
    labels: &HashMap<Oid, Label>,
) -> Result<Value, Problem> {
    let wrong = Problem::WrongLength;
    let column_type = match attribute.kind {
        Kind::Read(column_type) => column_type,
        Kind::Other => return Ok(Value::Datum(Datum::Null)),
        Kind::Enum(_) => {
            let Ok(oid) = bytes.try_into() else {
                return Err(wrong);
            };
            let oid = u32::from_le_bytes(oid);
            let label = labels.get(&oid).ok_or(Problem::UnlabelledEnum)?;
            return Ok(Value::Datum(Datum::Text(label.text.as_str().into())));
        }
    };
    let datum = match column_type {
        ColumnType::Int => Datum::Int(integer(bytes).ok_or(wrong)?),
        ColumnType::Bool => match bytes {
            [0] => Datum::Bool(false),
            [1] => Datum::Bool(true),
            [_] => return Err(Problem::UnreadableBool),
            _ => return Err(wrong),
        },
        ColumnType::Time { .. } => {
            let Ok(micros) = bytes.try_into() else {
                return Err(wrong);
            };
            time(i64::from_le_bytes(micros))
        }
        // A `DateADT`, an `int32` in the server's byte order.
        ColumnType::Date => Datum::Date(i32::from_le_bytes(bytes.try_into().or(Err(wrong))?)),
        // A `pg_uuid_t`: its bytes as printed, in any byte order.
        ColumnType::Uuid => Datum::Uuid(bytes.try_into().or(Err(wrong))?),
        ColumnType::Numeric { .. } | ColumnType::Text => {
            if attribute.length != VARLENA_LENGTH {
                return Err(wrong);
            }
            let Some(raw) = detoast(bytes)? else {
                return Ok(Value::External);
            };
            match column_type {
                ColumnType::Numeric { scale } => {
                    Datum::Int(numeric(&raw, scale).ok_or(Problem::UnreadableNumeric)?)
                }
                _ => match std::str::from_utf8(&raw) {
                    Ok(text) => Datum::Text(text.into()),
                    Err(_) => return Err(Problem::NotUtf8),
                },
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
    let header = u16::from_le_bytes(*bytes.first_chunk::<NUMERIC_HEADER>()?);
    let (negative, weight, digits) = match header & NUMERIC_SIGN_MASK {
        NUMERIC_SPECIAL => return None,
        NUMERIC_SHORT => {
            let bits = i32::from(header & NUMERIC_SHORT_WEIGHT_MASK);
            let weight = if header & NUMERIC_SHORT_WEIGHT_SIGN == 0 {
                bits
            } else {
                bits - i32::from(NUMERIC_SHORT_WEIGHT_SIGN)
            };
            (
                header & NUMERIC_SHORT_SIGN != 0,
                weight,
                bytes.get(NUMERIC_HEADER..)?,
            )
        }
        sign => {
            let weight = bytes.get(NUMERIC_LONG_WEIGHT..NUMERIC_LONG_DIGITS)?;
            let weight = i16::from_le_bytes(*weight.first_chunk()?);
            (
                sign == NUMERIC_NEG,
                weight.into(),
                bytes.get(NUMERIC_LONG_DIGITS..)?,
            )
        }
    };
    let (digits, odd) = digits.as_chunks::<NUMERIC_DIGIT_SIZE>();
    if !odd.is_empty() {
        return None;
    }
    // `scaled` reads the digits as they come, so none is copied out.
    let digits = digits.iter().copied().map(u16::from_le_bytes);
    scaled(digits, weight, negative, scale)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wal::test_support::{attribute, short_varlena};

    const INT: Kind = Kind::Read(ColumnType::Int);
    const TEXT: Kind = Kind::Read(ColumnType::Text);
    const TIME: Kind = Kind::Read(ColumnType::Time { zoned: true });
    const DATE: Kind = Kind::Read(ColumnType::Date);
    const UUID: Kind = Kind::Read(ColumnType::Uuid);
    /// An enum's type OID.
    const MOOD: Oid = 16_400;
    use crate::wal::heap::TUPLE_HEADER;
    /// `MAXALIGN`: where a row's columns start is a multiple of it.
    const MAXALIGN: usize = 8;
    /// `va_tcinfo`'s method for zstd, which Straum doesn't read.
    const METHOD_ZSTD: u32 = 2;
    /// `VARTAG_INDIRECT`: a TOAST pointer to memory, never on a page.
    const VARTAG_INDIRECT: u8 = 1;
    /// `NUMERIC_PINF`: positive infinity.
    const NUMERIC_POSITIVE_INFINITY: u16 = 0xD000;
    /// A short numeric's header: the display scale in six bits from bit 7,
    /// the sign and the weight in the seven below.
    const NUMERIC_SHORT_SCALE_SHIFT: u16 = 7;
    const NUMERIC_SHORT_WEIGHT_BITS: u16 = 0x7F;

    /// A row of `columns`, nulls where `None`, as `heap_form_tuple` lays
    /// it out.
    fn row(attributes: &[Attribute], columns: &[Option<&[u8]>]) -> HeapTuple {
        let nulls = columns.iter().any(Option::is_none);
        let bitmap = if nulls {
            columns.len().div_ceil(BITS_PER_BYTE)
        } else {
            0
        };
        let hoff = (TUPLE_HEADER + bitmap).next_multiple_of(MAXALIGN);
        let mut body = vec![0; hoff - TUPLE_HEADER];
        for (index, column) in columns.iter().enumerate() {
            if column.is_some() && nulls {
                body[index / BITS_PER_BYTE] |= 1 << (index % BITS_PER_BYTE);
            }
        }
        let mut data = Vec::new();
        for (attribute, column) in attributes.iter().zip(columns) {
            let Some(bytes) = column else { continue };
            let short = attribute.length == VARLENA_LENGTH && bytes[0] & VARLENA_ONE_BYTE != 0;
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
        let size = payload.len() + FOUR_BYTE_HEADER;
        let mut bytes = u32::try_from(size << FOUR_BYTE_SIZE_SHIFT)
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
            attribute(VARLENA_LENGTH, 4, TEXT),
            attribute(8, 8, INT),
            attribute(1, 1, Kind::Read(ColumnType::Bool)),
            attribute(4, 4, INT),
            attribute(VARLENA_LENGTH, 4, TEXT),
            attribute(4, 4, Kind::Other),
            attribute(4, 4, Kind::Enum(MOOD)),
            attribute(VARLENA_LENGTH, 4, TEXT),
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
        let happy = Label {
            enum_type: MOOD,
            text: "happy".to_owned(),
        };
        let labels = HashMap::from([(77, happy)]);
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

    /// A date is four bytes aligned to four (`typalign` 'i'), a uuid
    /// sixteen aligned to one ('c'), so a uuid after a boolean starts at
    /// once and a date after it pads to four.
    #[test]
    fn walks_dates_and_uuids() {
        let attributes = [
            attribute(1, 1, Kind::Read(ColumnType::Bool)),
            attribute(16, 1, UUID),
            attribute(1, 1, Kind::Read(ColumnType::Bool)),
            attribute(4, 4, DATE),
            attribute(4, 4, DATE),
        ];
        let uuid: [u8; 16] = [
            0xa0, 0xee, 0xbc, 0x99, 0x9c, 0x0b, 0x4e, 0xf8, 0, 1, 2, 3, 4, 5, 6, 7,
        ];
        let columns: [Option<&[u8]>; 5] = [
            Some(&[0]),
            Some(&uuid),
            Some(&[1]),
            Some(&(-746_117i32).to_le_bytes()),
            Some(&i32::MAX.to_le_bytes()),
        ];
        let tuple = row(&attributes, &columns);
        // The bool, the uuid at 1, the bool at 17, padding to 20, the dates.
        assert_eq!(
            tuple.body.len() - (usize::from(tuple.hoff) - TUPLE_HEADER),
            28
        );
        let got = values(&tuple, &attributes, &HashMap::new()).unwrap();
        assert_eq!(
            got,
            [
                Value::Datum(Datum::Bool(false)),
                Value::Datum(Datum::Uuid(uuid)),
                Value::Datum(Datum::Bool(true)),
                Value::Datum(Datum::Date(-746_117)),
                Value::Datum(Datum::Date(i32::MAX)),
            ]
        );
    }

    #[test]
    fn walks_c_strings_and_external_pointers() {
        let attributes = [
            attribute(C_STRING_LENGTH, 1, Kind::Other),
            attribute(VARLENA_LENGTH, 4, TEXT),
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
        let attributes = [attribute(VARLENA_LENGTH, 4, TEXT)];
        let plain = b"abcabcabcabc";
        // pglz: one literal group of 3 bytes, then a match of 9 back 3.
        let pglz = [0b0000_1000, b'a', b'b', b'c', 0x06, 0x03];
        // lz4: 3 literals, a match of 9 (4 + 5) back 3, no literals.
        let lz4 = [0x35, b'a', b'b', b'c', 0x03, 0x00, 0x00];
        for (method, stored) in [(METHOD_PGLZ, &pglz[..]), (METHOD_LZ4, &lz4[..])] {
            let size_and_method = u32::try_from(plain.len()).unwrap() | method << METHOD_SHIFT;
            let mut bytes = compressed_header(stored.len()).to_vec();
            bytes.extend(size_and_method.to_le_bytes());
            bytes.extend(stored);
            let got = values(
                &row(&attributes, &[Some(&bytes)]),
                &attributes,
                &HashMap::new(),
            );
            assert_eq!(got, Ok(vec![text("abcabcabcabc")]), "method {method}");
            let mut bad = bytes.clone();
            bad[TCINFO].copy_from_slice(&(size_and_method + 1).to_le_bytes());
            let got = values(
                &row(&attributes, &[Some(&bad)]),
                &attributes,
                &HashMap::new(),
            );
            assert_eq!(got, Err(Problem::ValueNotDecompressing));
        }
        // zstd, and a raw size of 12, from two stored bytes.
        let mut zstd = compressed_header(2).to_vec();
        zstd.extend(((METHOD_ZSTD << METHOD_SHIFT) + 12u32).to_le_bytes());
        zstd.extend([0, 0]);
        let got = values(
            &row(&attributes, &[Some(&zstd)]),
            &attributes,
            &HashMap::new(),
        );
        assert_eq!(got, Err(Problem::UnknownCompression));
    }

    /// A compressed varlena's four-byte header, for `stored` bytes after
    /// `va_tcinfo`.
    fn compressed_header(stored: usize) -> [u8; FOUR_BYTE_HEADER] {
        let size = stored + COMPRESSED_HEADER;
        let header = size << FOUR_BYTE_SIZE_SHIFT | usize::from(VARLENA_COMPRESSED);
        u32::try_from(header).unwrap().to_le_bytes()
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
        let text_column = attribute(VARLENA_LENGTH, 4, TEXT);
        let fits = four_byte_row();
        check(&fits, &[], Problem::TooManyColumns);
        let mut cut = fits.clone();
        cut.body = cut.body[..cut.body.len() - 1].into();
        check(&cut, &[int], Problem::ShortRow);
        let mut no_header = fits.clone();
        no_header.hoff = 8;
        check(&no_header, &[int], Problem::ShortRow);
        let other_tag = [VARLENA_EXTERNAL, VARTAG_INDIRECT, 0, 0];
        check(
            &row(&[text_column], &[Some(&other_tag)]),
            &[text_column],
            Problem::ShortRow,
        );
        let half = [compressed_header(2).as_slice(), &[0, 0]].concat();
        check(
            &row(&[text_column], &[Some(&half)]),
            &[text_column],
            Problem::ShortRow,
        );
        // A four-byte header naming fewer bytes than itself.
        check(
            &row(
                &[text_column],
                &[Some(&1u32.wrapping_shl(FOUR_BYTE_SIZE_SHIFT).to_le_bytes())],
            ),
            &[text_column],
            Problem::ShortRow,
        );
    }

    /// A compressed varlena too short to hold `va_tcinfo`, the size it
    /// comes out at.
    #[test]
    fn rejects_a_compressed_value_without_its_size() {
        let text_column = attribute(VARLENA_LENGTH, 4, TEXT);
        let size = FOUR_BYTE_HEADER + 2;
        let header = size << FOUR_BYTE_SIZE_SHIFT | usize::from(VARLENA_COMPRESSED);
        let header = u32::try_from(header).unwrap().to_le_bytes();
        let value = [header.as_slice(), &[0, 0]].concat();
        check(
            &row(&[text_column], &[Some(&value)]),
            &[text_column],
            Problem::CompressedWithoutSize,
        );
    }

    #[test]
    fn rejects_values_that_dont_fit() {
        let int = attribute(4, 4, INT);
        let text_column = attribute(VARLENA_LENGTH, 4, TEXT);
        let fits = four_byte_row();
        check(&fits, &[attribute(-3, 1, INT)], Problem::UnknownLength);
        check(
            &fits,
            &[attribute(4, 4, Kind::Read(ColumnType::Bool))],
            Problem::WrongLength,
        );
        check(&fits, &[attribute(4, 4, TEXT)], Problem::WrongLength);
        check(&fits, &[attribute(4, 4, TIME)], Problem::WrongLength);
        check(&fits, &[attribute(4, 1, UUID)], Problem::WrongLength);
        let two = row(&[attribute(2, 2, INT)], &[Some(&[1, 2])]);
        check(&two, &[attribute(2, 2, DATE)], Problem::WrongLength);
        check(
            &fits,
            &[attribute(2, 2, Kind::Enum(MOOD))],
            Problem::WrongLength,
        );
        check(
            &fits,
            &[attribute(4, 4, Kind::Enum(MOOD))],
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
        let numeric = attribute(
            VARLENA_LENGTH,
            4,
            Kind::Read(ColumnType::Numeric { scale: 0 }),
        );
        check(
            &row(&[numeric], &[Some(&short_varlena("\u{1}"))]),
            &[numeric],
            Problem::UnreadableNumeric,
        );
    }

    /// PostgreSQL stores a boolean as 0 or 1; any other byte is not one.
    #[test]
    fn rejects_a_boolean_neither_0_nor_1() {
        let boolean = attribute(1, 1, Kind::Read(ColumnType::Bool));
        for byte in [2, 0xFF] {
            check(
                &row(&[boolean], &[Some(&[byte])]),
                &[boolean],
                Problem::UnreadableBool,
            );
        }
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
        let weight_bits = u16::from_le_bytes(weight.to_le_bytes()) & NUMERIC_SHORT_WEIGHT_BITS;
        let header = NUMERIC_SHORT
            | if negative { NUMERIC_SHORT_SIGN } else { 0 }
            | scale << NUMERIC_SHORT_SCALE_SHIFT
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
            &NUMERIC_SPECIAL.to_le_bytes()[..],
            &NUMERIC_POSITIVE_INFINITY.to_le_bytes(),
            &[0],
            &[0, 0, 0],
            &[0, 0, 0, 0, 1],
            &[0, 0x80, 1],
        ] {
            assert_eq!(numeric(bytes, 0), None, "{bytes:?}");
        }
    }
}
