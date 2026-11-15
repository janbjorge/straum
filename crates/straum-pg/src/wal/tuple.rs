//! The values of a row as the text PostgreSQL's output functions print
//! under `DateStyle = ISO` and `TimeZone = UTC`, which is what pgoutput
//! sends. The row is walked as `heap_deform_tuple` in
//! `src/backend/access/common/heaptuple.c` walks it; layouts from
//! `src/include/varatt.h` and `src/backend/utils/adt/numeric.c`.
//!
//! Values are read little-endian: PostgreSQL writes them in the server's
//! byte order, and the servers Straum meets are x86 or ARM.

use std::collections::HashMap;
use std::fmt::Write;

use super::compress;
use super::heap::Tuple;

const HAS_NULL: u16 = 0x0001;
const NATTS_MASK: u16 = 0x07FF;

/// `VARTAG_ONDISK` and the size of the `varatt_external` after it.
const VARTAG_ON_DISK: u8 = 18;
const EXTERNAL_POINTER: usize = 16;
const RAW_SIZE_MASK: u32 = 0x3FFF_FFFF;
const METHOD_PGLZ: u32 = 0;
const METHOD_LZ4: u32 = 1;

const NUMERIC_SIGN_MASK: u16 = 0xC000;
const NUMERIC_NEG: u16 = 0x4000;
const NUMERIC_SHORT: u16 = 0x8000;
const NUMERIC_SPECIAL: u16 = 0xC000;
const NUMERIC_NAN: u16 = 0xC000;
const NUMERIC_PINF: u16 = 0xD000;
const NUMERIC_NINF: u16 = 0xF000;
const NUMERIC_EXT_SIGN_MASK: u16 = 0xF000;
const NUMERIC_DSCALE_MASK: u16 = 0x3FFF;
const NUMERIC_SHORT_SIGN: u16 = 0x2000;
const NUMERIC_SHORT_DSCALE_MASK: u16 = 0x1F80;
const NUMERIC_SHORT_DSCALE_SHIFT: u16 = 7;
const NUMERIC_SHORT_WEIGHT_SIGN: u16 = 0x0040;
const NUMERIC_SHORT_WEIGHT_MASK: u16 = 0x003F;
/// Decimal digits in one base-10000 numeric digit.
const DECIMAL_DIGITS: usize = 4;

const MICROS_PER_SECOND: i64 = 1_000_000;
const SECONDS_PER_DAY: i64 = 86_400;
/// Days from 1970-01-01 to 2000-01-01, PostgreSQL's epoch.
const PG_EPOCH_DAYS: i64 = 10_957;
/// Days from 0000-03-01 to 1970-01-01 in the proleptic Gregorian calendar.
const UNIX_EPOCH_DAYS: i64 = 719_468;
const DAYS_PER_ERA: i64 = 146_097;

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

/// How a column's bytes print: the types Straum reads, and the rest,
/// whose bytes are only stepped over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Int,
    Numeric,
    Bool,
    Text,
    Timestamp,
    Timestamptz,
    Enum,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    Null,
    Text(String),
    /// A column of a [`Kind::Other`] type.
    Skipped,
    /// A value TOASTed out of line, which the page doesn't hold.
    External,
    /// A column added after the row was written. Its value is the
    /// column's `attmissingval`, or null if it has none.
    Missing,
}

/// The values of `tuple`, one per attribute. `labels` holds the label of
/// each enum value by its `pg_enum` OID, which is what a row stores.
pub(crate) fn values(
    tuple: &Tuple,
    attributes: &[Attribute],
    labels: &HashMap<u32, String>,
) -> Result<Vec<Value>, &'static str> {
    let short = "a row shorter than its columns";
    let natts = usize::from(tuple.infomask2 & NATTS_MASK);
    if natts > attributes.len() {
        return Err("a row with more columns than its table");
    }
    let bitmap = if tuple.infomask & HAS_NULL == 0 {
        None
    } else {
        Some(tuple.body.get(..natts.div_ceil(8)).ok_or(short)?)
    };
    let data = tuple.columns().ok_or(short)?;
    let mut at = 0;
    let mut values = Vec::with_capacity(attributes.len());
    for (i, attribute) in attributes.iter().enumerate() {
        if i >= natts {
            values.push(Value::Missing);
            continue;
        }
        if bitmap.is_some_and(|bits| bits[i / 8] >> (i % 8) & 1 == 0) {
            values.push(Value::Null);
            continue;
        }
        // A varlena with a one-byte header isn't aligned, and its first
        // byte is never zero, which padding always is.
        let unaligned = attribute.length == -1 && data.get(at).is_some_and(|b| *b != 0);
        if !unaligned {
            at = at.next_multiple_of(attribute.align.max(1));
        }
        let rest = data.get(at..).ok_or(short)?;
        let size = match attribute.length {
            -1 => varlena_size(rest).ok_or(short)?,
            -2 => rest.iter().position(|b| *b == 0).ok_or(short)? + 1,
            length => usize::try_from(length).map_err(|_| "a column of unknown length")?,
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
    if first == 0x01 {
        return (*bytes.get(1)? == VARTAG_ON_DISK).then_some(2 + EXTERNAL_POINTER);
    }
    let size = if first & 0x01 == 1 {
        usize::from(first >> 1)
    } else {
        usize::try_from(u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?) >> 2).ok()?
    };
    let header = if first & 0x01 == 1 { 1 } else { 4 };
    (size >= header).then_some(size)
}

/// What a varlena holds: its bytes, decompressed, or `None` for a value
/// TOASTed out of line.
fn detoast(bytes: &[u8]) -> Result<Option<Vec<u8>>, &'static str> {
    let first = bytes[0];
    if first == 0x01 {
        return Ok(None);
    }
    if first & 0x01 == 1 {
        return Ok(Some(bytes[1..].to_vec()));
    }
    if first & 0x03 == 0 {
        return Ok(Some(bytes[4..].to_vec()));
    }
    let info = bytes
        .get(4..8)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        .ok_or("a compressed value without its size")?;
    let size = usize::try_from(info & RAW_SIZE_MASK).map_err(|_| "a value too big")?;
    let stored = &bytes[8..];
    let raw = match info >> 30 {
        METHOD_PGLZ => compress::pglz(stored, size),
        METHOD_LZ4 => compress::lz4(stored, size),
        _ => return Err("a value compressed in a way Straum can't read"),
    };
    raw.map(Some).ok_or("a value that doesn't decompress")
}

fn value(
    attribute: &Attribute,
    bytes: &[u8],
    labels: &HashMap<u32, String>,
) -> Result<Value, &'static str> {
    let wrong = "a value of the wrong length for its type";
    let text = match attribute.kind {
        Kind::Other => return Ok(Value::Skipped),
        Kind::Int => match *bytes {
            [a, b] => i16::from_le_bytes([a, b]).to_string(),
            [a, b, c, d] => i32::from_le_bytes([a, b, c, d]).to_string(),
            [a, b, c, d, e, f, g, h] => i64::from_le_bytes([a, b, c, d, e, f, g, h]).to_string(),
            _ => return Err(wrong),
        },
        Kind::Bool => match bytes {
            [0] => "f".to_owned(),
            [_] => "t".to_owned(),
            _ => return Err(wrong),
        },
        Kind::Timestamp | Kind::Timestamptz => {
            let micros = i64::from_le_bytes(bytes.try_into().map_err(|_| wrong)?);
            timestamp(micros, attribute.kind == Kind::Timestamptz)
        }
        Kind::Enum => {
            let oid = u32::from_le_bytes(bytes.try_into().map_err(|_| wrong)?);
            labels
                .get(&oid)
                .ok_or("an enum value Straum has no label for")?
                .clone()
        }
        Kind::Text | Kind::Numeric => {
            if attribute.length != -1 {
                return Err(wrong);
            }
            let Some(raw) = detoast(bytes)? else {
                return Ok(Value::External);
            };
            if attribute.kind == Kind::Numeric {
                numeric(&raw).ok_or("a numeric Straum can't read")?
            } else {
                String::from_utf8(raw).map_err(|_| "text that isn't UTF-8")?
            }
        }
    };
    Ok(Value::Text(text))
}

/// A numeric as `numeric_out` prints it: `NaN`, `Infinity`, or digits
/// with exactly its display scale after the point.
fn numeric(bytes: &[u8]) -> Option<String> {
    let header = u16::from_le_bytes([*bytes.first()?, *bytes.get(1)?]);
    let (negative, scale, weight, digits) = if header & NUMERIC_SIGN_MASK == NUMERIC_SPECIAL {
        return match header & NUMERIC_EXT_SIGN_MASK {
            NUMERIC_NAN => Some("NaN".to_owned()),
            NUMERIC_PINF => Some("Infinity".to_owned()),
            NUMERIC_NINF => Some("-Infinity".to_owned()),
            _ => None,
        };
    } else if header & NUMERIC_SIGN_MASK == NUMERIC_SHORT {
        let bits = header & NUMERIC_SHORT_WEIGHT_MASK;
        let weight = if header & NUMERIC_SHORT_WEIGHT_SIGN == 0 {
            i32::from(bits)
        } else {
            i32::from(bits) - 64
        };
        (
            header & NUMERIC_SHORT_SIGN != 0,
            (header & NUMERIC_SHORT_DSCALE_MASK) >> NUMERIC_SHORT_DSCALE_SHIFT,
            weight,
            bytes.get(2..)?,
        )
    } else {
        let weight = i16::from_le_bytes([*bytes.get(2)?, *bytes.get(3)?]);
        (
            header & NUMERIC_SIGN_MASK == NUMERIC_NEG,
            header & NUMERIC_DSCALE_MASK,
            i32::from(weight),
            bytes.get(4..)?,
        )
    };
    let (digits, odd) = digits.as_chunks::<2>();
    if !odd.is_empty() {
        return None;
    }
    let digit = |d: i32| -> i16 {
        usize::try_from(d)
            .ok()
            .and_then(|d| digits.get(d))
            .map_or(0, |pair| i16::from_le_bytes(*pair))
    };
    let mut text = String::new();
    if negative {
        text.push('-');
    }
    if weight < 0 {
        text.push('0');
    } else {
        write!(text, "{}", digit(0)).ok()?;
        for d in 1..=weight {
            write!(text, "{:04}", digit(d)).ok()?;
        }
    }
    let scale = usize::from(scale);
    if scale > 0 {
        let mut fraction = String::with_capacity(scale + DECIMAL_DIGITS);
        let mut d = weight + 1;
        while fraction.len() < scale {
            write!(fraction, "{:04}", digit(d)).ok()?;
            d += 1;
        }
        fraction.truncate(scale);
        text.push('.');
        text.push_str(&fraction);
    }
    Some(text)
}

/// A timestamp as `timestamp_out` and `timestamptz_out` print it in ISO
/// style and UTC: `2024-01-15 10:30:00.5+00`, a year before 1 as `BC`.
pub(crate) fn timestamp(micros: i64, zoned: bool) -> String {
    match micros {
        i64::MAX => return "infinity".to_owned(),
        i64::MIN => return "-infinity".to_owned(),
        _ => {}
    }
    let seconds = micros.div_euclid(MICROS_PER_SECOND);
    let fraction = micros.rem_euclid(MICROS_PER_SECOND);
    let days = seconds.div_euclid(SECONDS_PER_DAY) + PG_EPOCH_DAYS;
    let clock = seconds.rem_euclid(SECONDS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let (year, era) = if year > 0 {
        (year, "")
    } else {
        (1 - year, " BC")
    };
    let mut text = format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        clock / 3_600,
        clock / 60 % 60,
        clock % 60
    );
    if fraction != 0 {
        let digits = format!("{fraction:06}");
        text.push('.');
        text.push_str(digits.trim_end_matches('0'));
    }
    if zoned {
        text.push_str("+00");
    }
    text.push_str(era);
    text
}

/// Year, month and day of a day counted from 1970-01-01, after Howard
/// Hinnant's `civil_from_days`.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + UNIX_EPOCH_DAYS;
    let era = days.div_euclid(DAYS_PER_ERA);
    let day_of_era = days - era * DAYS_PER_ERA;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attribute(length: i16, align: usize, kind: Kind) -> Attribute {
        Attribute {
            length,
            align,
            kind,
        }
    }

    /// A row of `columns`, nulls where `None`, as `heap_form_tuple` lays
    /// it out with a 24-byte header.
    fn row(attributes: &[Attribute], columns: &[Option<&[u8]>]) -> Tuple {
        let nulls = columns.iter().any(Option::is_none);
        let bitmap = if nulls { columns.len().div_ceil(8) } else { 0 };
        let hoff = (23 + bitmap).next_multiple_of(8);
        let mut body = vec![0; hoff - 23];
        for (i, column) in columns.iter().enumerate() {
            if column.is_some() && nulls {
                body[i / 8] |= 1 << (i % 8);
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
        Tuple {
            infomask2: u16::try_from(columns.len()).unwrap(),
            infomask: u16::from(nulls),
            hoff: u8::try_from(hoff).unwrap(),
            body,
        }
    }

    fn short_varlena(text: &str) -> Vec<u8> {
        let mut bytes = vec![u8::try_from((text.len() + 1) << 1 | 1).unwrap()];
        bytes.extend_from_slice(text.as_bytes());
        bytes
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
        Value::Text(value.to_owned())
    }

    #[test]
    fn walks_aligned_columns_and_nulls() {
        let attributes = [
            attribute(2, 2, Kind::Int),
            attribute(-1, 4, Kind::Text),
            attribute(8, 8, Kind::Int),
            attribute(1, 1, Kind::Bool),
            attribute(4, 4, Kind::Int),
            attribute(-1, 4, Kind::Text),
            attribute(4, 4, Kind::Other),
            attribute(4, 4, Kind::Enum),
            attribute(-1, 4, Kind::Text),
        ];
        let long = "l".repeat(200);
        let columns: [Option<&[u8]>; 9] = [
            Some(&(-7i16).to_le_bytes()),
            Some(&short_varlena("abc")),
            Some(&(-9_000_000_000i64).to_le_bytes()),
            Some(&[1]),
            None,
            Some(&long_varlena(long.as_bytes())),
            Some(&[9, 9, 9, 9]),
            Some(&77u32.to_le_bytes()),
            Some(&short_varlena("")),
        ];
        let labels = HashMap::from([(77, "happy".to_owned())]);
        let got = values(&row(&attributes, &columns), &attributes, &labels).unwrap();
        assert_eq!(
            got,
            [
                text("-7"),
                text("abc"),
                text("-9000000000"),
                text("t"),
                Value::Null,
                text(&long),
                Value::Skipped,
                text("happy"),
                text(""),
            ]
        );
        // A table with a column the row predates.
        let mut more = attributes.to_vec();
        more.push(attribute(4, 4, Kind::Int));
        let got = values(&row(&attributes, &columns), &more, &labels).unwrap();
        assert_eq!(got.last(), Some(&Value::Missing));
    }

    #[test]
    fn walks_c_strings_and_external_pointers() {
        let attributes = [
            attribute(-2, 1, Kind::Other),
            attribute(-1, 4, Kind::Text),
            attribute(4, 4, Kind::Int),
        ];
        let mut pointer = vec![0x01, VARTAG_ON_DISK];
        pointer.extend([0; EXTERNAL_POINTER]);
        let columns: [Option<&[u8]>; 3] =
            [Some(b"name\0"), Some(&pointer), Some(&5i32.to_le_bytes())];
        let got = values(&row(&attributes, &columns), &attributes, &HashMap::new()).unwrap();
        assert_eq!(got, [Value::Skipped, Value::External, text("5")]);
    }

    #[test]
    fn reads_compressed_values() {
        let attributes = [attribute(-1, 4, Kind::Text)];
        let plain = b"abcabcabcabc";
        // pglz: one literal group of 3 bytes, then a match of 9 back 3.
        let pglz = [0b0000_1000, b'a', b'b', b'c', 0x06, 0x03];
        // lz4: 3 literals, a match of 9 (4 + 5) back 3, no literals.
        let lz4 = [0x35, b'a', b'b', b'c', 0x03, 0x00, 0x00];
        for (method, stored) in [(METHOD_PGLZ, &pglz[..]), (METHOD_LZ4, &lz4[..])] {
            let info = u32::try_from(plain.len()).unwrap() | method << 30;
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
            assert_eq!(got, Err("a value that doesn't decompress"));
        }
        let mut zstd = u32::try_from(10 << 2 | 2).unwrap().to_le_bytes().to_vec();
        zstd.extend((12u32 | 2 << 30).to_le_bytes());
        zstd.extend([0, 0]);
        let got = values(
            &row(&attributes, &[Some(&zstd)]),
            &attributes,
            &HashMap::new(),
        );
        assert_eq!(got, Err("a value compressed in a way Straum can't read"));
    }

    #[test]
    fn rejects_rows_that_dont_fit() {
        let int = attribute(4, 4, Kind::Int);
        let text_column = attribute(-1, 4, Kind::Text);
        let none = HashMap::new();
        let four = 1i32.to_le_bytes();
        let fits = row(&[int], &[Some(&four)]);
        let check = |tuple: &Tuple, attributes: &[Attribute], problem: &str| {
            assert_eq!(values(tuple, attributes, &none), Err(problem));
        };
        check(&fits, &[], "a row with more columns than its table");
        let mut cut = fits.clone();
        cut.body.pop();
        check(&cut, &[int], "a row shorter than its columns");
        let mut no_header = fits.clone();
        no_header.hoff = 8;
        check(&no_header, &[int], "a row shorter than its columns");
        check(
            &fits,
            &[attribute(-3, 1, Kind::Int)],
            "a column of unknown length",
        );
        check(
            &fits,
            &[attribute(4, 4, Kind::Bool)],
            "a value of the wrong length for its type",
        );
        check(
            &fits,
            &[attribute(4, 4, Kind::Text)],
            "a value of the wrong length for its type",
        );
        check(
            &fits,
            &[attribute(4, 4, Kind::Timestamp)],
            "a value of the wrong length for its type",
        );
        check(
            &fits,
            &[attribute(2, 2, Kind::Enum)],
            "a value of the wrong length for its type",
        );
        check(
            &fits,
            &[attribute(4, 4, Kind::Enum)],
            "an enum value Straum has no label for",
        );
        let three = row(&[int], &[Some(&[1, 2, 3])]);
        check(
            &three,
            &[attribute(3, 1, Kind::Int)],
            "a value of the wrong length for its type",
        );
        let bad_utf8 = short_varlena("ab");
        let bad_utf8 = [bad_utf8[0], 0xFF, 0xFE];
        check(
            &row(&[text_column], &[Some(&bad_utf8)]),
            &[text_column],
            "text that isn't UTF-8",
        );
        let numeric = attribute(-1, 4, Kind::Numeric);
        check(
            &row(&[numeric], &[Some(&short_varlena("\u{1}"))]),
            &[numeric],
            "a numeric Straum can't read",
        );
        let other_tag = [0x01, 1, 0, 0];
        check(
            &row(&[text_column], &[Some(&other_tag)]),
            &[text_column],
            "a row shorter than its columns",
        );
        let half = [10 << 2 | 2, 0, 0, 0, 0, 0];
        check(
            &row(&[text_column], &[Some(&half)]),
            &[text_column],
            "a row shorter than its columns",
        );
        // A four-byte header naming fewer bytes than itself.
        check(
            &row(&[text_column], &[Some(&[0x04, 0, 0, 0])]),
            &[text_column],
            "a row shorter than its columns",
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
            | scale << NUMERIC_SHORT_DSCALE_SHIFT
            | weight_bits;
        let mut payload = header.to_le_bytes().to_vec();
        for digit in digits {
            payload.extend(digit.to_le_bytes());
        }
        payload
    }

    #[test]
    fn prints_numerics_as_numeric_out() {
        let cases: [(bool, u16, i16, &[i16], &str); 9] = [
            (false, 0, 0, &[], "0"),
            (false, 2, 0, &[], "0.00"),
            (false, 0, 0, &[42], "42"),
            (true, 4, 0, &[12, 3400], "-12.3400"),
            (false, 4, -1, &[1], "0.0001"),
            (false, 6, -2, &[5000], "0.000050"),
            (false, 0, 1, &[1, 7], "10007"),
            (false, 0, 2, &[9], "900000000"),
            (true, 3, 1, &[12, 3456, 7891], "-123456.789"),
        ];
        for (negative, scale, weight, digits, expected) in cases {
            for payload in [
                long_numeric(negative, scale, weight, digits),
                short_numeric(negative, scale, weight, digits),
            ] {
                assert_eq!(numeric(&payload).as_deref(), Some(expected), "{payload:?}");
            }
        }
        for (header, expected) in [
            (NUMERIC_NAN, "NaN"),
            (NUMERIC_PINF, "Infinity"),
            (NUMERIC_NINF, "-Infinity"),
        ] {
            assert_eq!(numeric(&header.to_le_bytes()).as_deref(), Some(expected));
        }
        assert_eq!(numeric(&0xE000u16.to_le_bytes()), None);
        assert_eq!(numeric(&[0]), None);
        assert_eq!(numeric(&[0, 0, 0]), None);
        assert_eq!(numeric(&[0, 0, 0, 0, 1]), None);
        assert_eq!(numeric(&[0, 0x80, 1]), None);
    }

    #[test]
    fn prints_timestamps_as_timestamp_out() {
        let day = SECONDS_PER_DAY * MICROS_PER_SECOND;
        let cases = [
            (0, "2000-01-01 00:00:00"),
            (500_000, "2000-01-01 00:00:00.5"),
            (1, "2000-01-01 00:00:00.000001"),
            (-1, "1999-12-31 23:59:59.999999"),
            (758_629_800_500_000, "2024-01-15 10:30:00.5"),
            (59 * day, "2000-02-29 00:00:00"),
            (-730_119 * day, "0001-01-01 00:00:00"),
            (-730_120 * day, "0001-12-31 00:00:00 BC"),
            (2_915_001 * day, "9981-01-01 00:00:00"),
            (2_918_653 * day, "9991-01-01 00:00:00"),
            (2_922_306 * day, "10001-01-01 00:00:00"),
        ];
        for (micros, expected) in cases {
            assert_eq!(timestamp(micros, false), expected);
        }
        assert_eq!(timestamp(1, true), "2000-01-01 00:00:00.000001+00");
        assert_eq!(timestamp(-730_120 * day, true), "0001-12-31 00:00:00+00 BC");
        assert_eq!(timestamp(i64::MAX, true), "infinity");
        assert_eq!(timestamp(i64::MIN, false), "-infinity");
    }

    /// Every day from 4713 BC to 9999 back to the day it came from.
    #[test]
    fn civil_days_round_trip() {
        let mut previous = civil_from_days(-2_440_589);
        for days in -2_440_588..2_932_897 {
            let (year, month, day) = civil_from_days(days);
            let next_day = (year, month, day) > previous;
            assert!(next_day && (1..=12).contains(&month) && (1..=31).contains(&day));
            assert_eq!(crate::value::days_from_civil(year, month, day), Some(days));
            previous = (year, month, day);
        }
    }
}
