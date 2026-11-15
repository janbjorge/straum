//! Column types Straum reads: their text form as PostgreSQL prints it
//! under `DateStyle = ISO` and `TimeZone = UTC`, which Straum sets on
//! connect, and their binary form, which loads read. Also PostgreSQL's
//! timestamps as clock times, as the stream reads and writes them.

use std::fmt::{self, Write as _};
use std::str;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use straum_domain::{ColumnType, Datum, Identifier};

pub(crate) const BOOL: u32 = 16;
pub(crate) const INT8: u32 = 20;
pub(crate) const INT2: u32 = 21;
pub(crate) const INT4: u32 = 23;
pub(crate) const TEXT: u32 = 25;
pub(crate) const BPCHAR: u32 = 1042;
pub(crate) const VARCHAR: u32 = 1043;
pub(crate) const TIMESTAMP: u32 = 1114;
pub(crate) const TIMESTAMPTZ: u32 = 1184;
pub(crate) const NUMERIC: u32 = 1700;

/// `VARHDRSZ`, which a numeric typmod adds to precision and scale.
const TYPMOD_HEADER: i32 = 4;
/// Less `VARHDRSZ`, a numeric typmod holds the precision from bit 16 and
/// the scale in the low 11 bits (`make_numeric_typmod`).
const TYPMOD_PRECISION_SHIFT: i32 = 16;
const TYPMOD_SCALE_MASK: i32 = 0x7ff;
const TYPMOD_SCALE_SIGN: i32 = 0x400;
/// The most digits an `i64` holds whole.
const MAX_PRECISION: i32 = 18;
const SECONDS_PER_MINUTE: i64 = 60;
const SECONDS_PER_HOUR: i64 = 3_600;
const SECONDS_PER_DAY: i64 = 86_400;
const MAX_HOUR: i64 = 23;
const MAX_MINUTE: i64 = 59;
/// A leap second reads as `:60`.
const MAX_SECOND: i64 = 60;
/// Days from 0000-03-01 to 1970-01-01 in the proleptic Gregorian calendar.
const UNIX_EPOCH_DAYS: i64 = 719_468;
const DAYS_PER_ERA: i64 = 146_097;
/// Seconds from 1970-01-01 to 2000-01-01, PostgreSQL's epoch.
const PG_EPOCH: i64 = 946_684_800;
const MICROS_PER_SECOND: i64 = 1_000_000;
/// [`PG_EPOCH`] as a clock time, less [`UNIX_EPOCH`].
const PG_EPOCH_AFTER_UNIX: Duration = Duration::from_secs(PG_EPOCH.unsigned_abs());
/// The sign words of a binary numeric; NaN and the infinities have others.
const NUMERIC_POS: u16 = 0x0000;
const NUMERIC_NEG: u16 = 0x4000;
/// `numeric_send`'s header, two bytes each: `ndigits`, `weight`, `sign`
/// and `dscale`, then the digits.
const NUMERIC_COUNT_AT: usize = 0;
const NUMERIC_WEIGHT_AT: usize = 2;
const NUMERIC_SIGN_AT: usize = 4;
const NUMERIC_HEADER: usize = 8;
/// `DEC_DIGITS`: the decimal digits in one base-10000 digit.
const DECIMAL_DIGITS: i32 = 4;

/// How Straum reads a column of this type: `int2`, `int4` and `int8` as
/// integers, `numeric(p, s)` with `p` at most 18 as an integer in units of
/// `10^-s`, `text`, `varchar` and `bpchar` as text, `timestamptz` as Unix
/// seconds, rounded down, and `timestamp` as the Unix seconds its wall
/// clock reading would be in UTC. `None` for a type Straum doesn't read.
pub(crate) fn type_of(type_oid: u32, typmod: i32) -> Option<ColumnType> {
    match type_oid {
        INT2 | INT4 | INT8 => Some(ColumnType::Int),
        NUMERIC => {
            // An unconstrained numeric (typmod -1) has no fixed scale.
            let packed = typmod
                .checked_sub(TYPMOD_HEADER)
                .filter(|packed| *packed >= 0)?;
            let precision = packed >> TYPMOD_PRECISION_SHIFT;
            // The low 11 bits hold the scale, signed since PostgreSQL 15.
            let scale = ((packed & TYPMOD_SCALE_MASK) ^ TYPMOD_SCALE_SIGN) - TYPMOD_SCALE_SIGN;
            let scale = u32::try_from(scale).ok()?;
            (precision <= MAX_PRECISION).then_some(ColumnType::Numeric { scale })
        }
        BOOL => Some(ColumnType::Bool),
        TEXT | VARCHAR | BPCHAR => Some(ColumnType::Text),
        TIMESTAMPTZ => Some(ColumnType::Time { zoned: true }),
        TIMESTAMP => Some(ColumnType::Time { zoned: false }),
        _ => None,
    }
}

pub(crate) fn read(column_type: ColumnType, text: &str) -> Result<Datum, ValueError> {
    let datum = match column_type {
        ColumnType::Int => text.parse().ok().map(Datum::Int),
        ColumnType::Numeric { scale } => decimal(text, scale)
            .and_then(|value| i64::try_from(value).ok())
            .map(Datum::Int),
        ColumnType::Bool => match text {
            "t" => Some(Datum::Bool(true)),
            "f" => Some(Datum::Bool(false)),
            _ => None,
        },
        ColumnType::Text => Some(Datum::Text(text.into())),
        // An infinite time is kept as a null one: the row counts in no
        // bucket an ask reads, and the load goes on.
        ColumnType::Time { .. } if text == "infinity" || text == "-infinity" => Some(Datum::Null),
        ColumnType::Time { zoned } => timestamp(text, zoned).map(Datum::Int),
    };
    datum.ok_or_else(|| ValueError::new(column_type, text))
}

/// Reads the binary form `COPY ... (FORMAT binary)` sends.
pub(crate) fn read_binary(column_type: ColumnType, bytes: &[u8]) -> Result<Datum, ValueError> {
    let datum = match column_type {
        ColumnType::Int => int(bytes).map(Datum::Int),
        ColumnType::Numeric { scale } => binary_decimal(bytes, scale).map(Datum::Int),
        ColumnType::Bool => match bytes {
            [0] => Some(Datum::Bool(false)),
            [1] => Some(Datum::Bool(true)),
            _ => None,
        },
        ColumnType::Text => str::from_utf8(bytes)
            .ok()
            .map(|text| Datum::Text(text.into())),
        // Both types are microseconds since 2000-01-01, a `timestamp`'s
        // read as if UTC.
        ColumnType::Time { .. } => <[u8; 8]>::try_from(bytes)
            .ok()
            .map(|array| time(i64::from_be_bytes(array))),
    };
    datum.ok_or_else(|| ValueError::binary(column_type, bytes))
}

/// A timestamp, microseconds since PostgreSQL's epoch, as Straum reads
/// it: Unix seconds, rounded down. `infinity` and `-infinity` are null,
/// as the text reader has them.
pub(crate) fn time(micros: i64) -> Datum {
    match micros {
        i64::MIN | i64::MAX => Datum::Null,
        _ => Datum::Int(micros.div_euclid(MICROS_PER_SECOND) + PG_EPOCH),
    }
}

/// A PostgreSQL timestamp, microseconds since its epoch, as a clock time.
pub(crate) fn system_time(micros: i64) -> SystemTime {
    let epoch = UNIX_EPOCH + PG_EPOCH_AFTER_UNIX;
    let offset = Duration::from_micros(micros.unsigned_abs());
    if micros < 0 {
        epoch - offset
    } else {
        epoch + offset
    }
}

/// A clock time as a PostgreSQL timestamp; 0 for one before its epoch.
pub(crate) fn timestamp_micros(time: SystemTime) -> i64 {
    time.duration_since(UNIX_EPOCH + PG_EPOCH_AFTER_UNIX)
        .map_or(0, |since| {
            i64::try_from(since.as_micros()).unwrap_or(i64::MAX)
        })
}

/// `int2`, `int4` or `int8`, big-endian, told apart by length.
fn int(bytes: &[u8]) -> Option<i64> {
    match bytes.len() {
        2 => Some(i16::from_be_bytes(bytes.try_into().ok()?).into()),
        4 => Some(i32::from_be_bytes(bytes.try_into().ok()?).into()),
        8 => Some(i64::from_be_bytes(bytes.try_into().ok()?)),
        _ => None,
    }
}

/// A binary numeric: digit count, weight, sign and display scale, then
/// big-endian base-10000 digits.
fn binary_decimal(bytes: &[u8], scale: u32) -> Option<i64> {
    let count = u16::from_be_bytes(word_at(bytes, NUMERIC_COUNT_AT)?);
    let weight = i16::from_be_bytes(word_at(bytes, NUMERIC_WEIGHT_AT)?);
    let negative = match u16::from_be_bytes(word_at(bytes, NUMERIC_SIGN_AT)?) {
        NUMERIC_POS => false,
        NUMERIC_NEG => true,
        _ => return None,
    };
    let (digits, odd) = bytes.get(NUMERIC_HEADER..)?.as_chunks::<2>();
    if !odd.is_empty() || digits.len() != usize::from(count) {
        return None;
    }
    let digits = digits.iter().map(|pair| u16::from_be_bytes(*pair));
    scaled(digits, weight.into(), negative, scale)
}

/// The two bytes at `at`, if `bytes` reach that far.
fn word_at(bytes: &[u8], at: usize) -> Option<[u8; 2]> {
    bytes.get(at..)?.first_chunk().copied()
}

/// Base-10000 digits, the first worth `10000^weight`, in units of
/// `10^-scale`, as [`decimal`] scales text. `None` for a digit below the
/// scale, or past an `i64`.
pub(crate) fn scaled(
    digits: impl Iterator<Item = u16>,
    weight: i32,
    negative: bool,
    scale: u32,
) -> Option<i64> {
    let scale = i32::try_from(scale).ok()?;
    let mut value: i128 = 0;
    for (position, digit) in digits.enumerate() {
        let exponent = DECIMAL_DIGITS * (weight - i32::try_from(position).ok()?) + scale;
        let scaled = match u32::try_from(exponent) {
            Ok(exponent) => i128::from(digit).checked_mul(10_i128.checked_pow(exponent)?)?,
            // Below the scale a digit must be zero; a digit is below 10^4.
            Err(_) => match 10_u16.checked_pow(exponent.unsigned_abs()) {
                Some(unit) if digit % unit == 0 => (digit / unit).into(),
                _ if digit == 0 => 0,
                _ => return None,
            },
        };
        value = value.checked_add(scaled)?;
    }
    i64::try_from(if negative { -value } else { value }).ok()
}

/// `-123.45` at scale 2 is -12345. More fraction digits than the scale, or
/// anything else (`NaN`, `Infinity`, an exponent), is `None`.
fn decimal(text: &str, scale: u32) -> Option<i128> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if whole.is_empty() || !all_digits(whole) || !all_digits(fraction) {
        return None;
    }
    let pad = scale.checked_sub(u32::try_from(fraction.len()).ok()?)?;
    let mut value: i128 = format!("{whole}{fraction}").parse().ok()?;
    value = value.checked_mul(10_i128.checked_pow(pad)?)?;
    Some(if negative { -value } else { value })
}

/// `2024-01-15 10:30:00.5+00` is 1705314600. Offsets other than `+00` are
/// read, though Straum always asks for UTC. A `timestamp` has no offset,
/// and its text must have none. `BC` dates are `None`.
fn timestamp(text: &str, zoned: bool) -> Option<i64> {
    let (date, rest) = text.split_once(' ')?;
    let (year_month, day) = date.rsplit_once('-')?;
    let (year, month) = year_month.rsplit_once('-')?;
    let (clock, offset) = match rest.find(['+', '-']) {
        Some(at) if zoned => rest.split_at(at),
        None if !zoned => (rest, ""),
        _ => return None,
    };
    let (clock, _fraction) = clock.split_once('.').unwrap_or((clock, ""));
    let mut clock = clock.split(':');
    let hour = clock_field(clock.next(), MAX_HOUR)?;
    let minute = clock_field(clock.next(), MAX_MINUTE)?;
    let second = clock_field(clock.next(), MAX_SECOND)?;
    if clock.next().is_some() {
        return None;
    }
    let days = days_from_civil(number(year)?, number(month)?, number(day)?)?;
    let seconds =
        days * SECONDS_PER_DAY + hour * SECONDS_PER_HOUR + minute * SECONDS_PER_MINUTE + second;
    let offset = if zoned { offset_seconds(offset)? } else { 0 };
    Some(seconds - offset)
}

/// An hour, minute or second of a clock, at most `max`.
fn clock_field(text: Option<&str>, max: i64) -> Option<i64> {
    let value = number(text?)?;
    (value <= max).then_some(value)
}

/// `+05:30` is 19800 seconds.
fn offset_seconds(text: &str) -> Option<i64> {
    let sign = if text.starts_with('-') { -1 } else { 1 };
    let mut parts = text[1..].split(':');
    let hours: i64 = number(parts.next()?)?;
    let minutes: i64 = parts.next().map_or(Some(0), number)?;
    let seconds: i64 = parts.next().map_or(Some(0), number)?;
    if parts.next().is_some() {
        return None;
    }
    Some(sign * (hours * SECONDS_PER_HOUR + minutes * SECONDS_PER_MINUTE + seconds))
}

/// Digits only; no sign.
fn number(text: &str) -> Option<i64> {
    if text.is_empty() || !all_digits(text) {
        return None;
    }
    text.parse().ok()
}

fn all_digits(text: &str) -> bool {
    text.bytes().all(|byte| byte.is_ascii_digit())
}

/// Days since 1970-01-01, after Howard Hinnant's `days_from_civil`.
fn days_from_civil(year: i64, month: i64, day: i64) -> Option<i64> {
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * DAYS_PER_ERA + day_of_era - UNIX_EPOCH_DAYS)
}

/// A value PostgreSQL sent that doesn't read as its column's type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ValueError {
    column_type: ColumnType,
    text: String,
    /// Set by the reader that knows it.
    place: Option<Place>,
}

/// Where a value was: its table, as `schema.table`, and its column.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Place {
    table: String,
    column: Identifier,
}

impl ValueError {
    fn new(column_type: ColumnType, text: &str) -> ValueError {
        ValueError {
            column_type,
            text: text.to_owned(),
            place: None,
        }
    }

    /// Shows the bytes in hex, as `bytea` prints.
    fn binary(column_type: ColumnType, bytes: &[u8]) -> ValueError {
        let mut hex = String::with_capacity(2 * bytes.len());
        for byte in bytes {
            write!(hex, "{byte:02x}").expect("a String takes any text");
        }
        ValueError {
            column_type,
            text: format!("\\x{hex}"),
            place: None,
        }
    }

    /// Names where the value came from, so a poison row can be found.
    pub(crate) fn at(mut self, table: &str, column: &Identifier) -> ValueError {
        self.place = Some(Place {
            table: table.to_owned(),
            column: column.clone(),
        });
        self
    }
}

impl fmt::Display for ValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "can't read {:?} as {}",
            self.text, self.column_type
        )?;
        if let Some(Place { table, column }) = &self.place {
            write!(formatter, " in column {column} of {table}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ValueError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric(precision: i32, scale: i32) -> i32 {
        ((precision << TYPMOD_PRECISION_SHIFT) | (scale & TYPMOD_SCALE_MASK)) + TYPMOD_HEADER
    }

    #[test]
    fn timestamps_as_clock_times() {
        // 10 957 days, from 1970-01-01 to 2000-01-01.
        let epoch = UNIX_EPOCH + Duration::from_hours(262_968);
        assert_eq!(system_time(0), epoch);
        assert_eq!(system_time(-1), epoch - Duration::from_micros(1));
        for micros in [0, 1, 1_000_000, 758_629_800_500_000] {
            assert_eq!(timestamp_micros(system_time(micros)), micros);
        }
        // Before PostgreSQL's epoch, which only a broken clock gives.
        assert_eq!(timestamp_micros(system_time(-1)), 0);
        assert_eq!(timestamp_micros(UNIX_EPOCH), 0);
    }

    #[test]
    fn column_types_from_oids() {
        let cases = [
            (INT2, -1, Some(ColumnType::Int)),
            (INT4, -1, Some(ColumnType::Int)),
            (INT8, -1, Some(ColumnType::Int)),
            (
                NUMERIC,
                numeric(12, 2),
                Some(ColumnType::Numeric { scale: 2 }),
            ),
            (
                NUMERIC,
                numeric(18, 0),
                Some(ColumnType::Numeric { scale: 0 }),
            ),
            (NUMERIC, numeric(19, 2), None),
            (NUMERIC, numeric(5, -2), None),
            (NUMERIC, -1, None),
            (BOOL, -1, Some(ColumnType::Bool)),
            (TEXT, -1, Some(ColumnType::Text)),
            (VARCHAR, 14, Some(ColumnType::Text)),
            (BPCHAR, 7, Some(ColumnType::Text)),
            (TIMESTAMPTZ, -1, Some(ColumnType::Time { zoned: true })),
            (TIMESTAMP, -1, Some(ColumnType::Time { zoned: false })),
            // float8, uuid, date
            (701, -1, None),
            (2950, -1, None),
            (1082, -1, None),
        ];
        for (oid, typmod, column_type) in cases {
            assert_eq!(type_of(oid, typmod), column_type, "{oid} {typmod}");
        }
    }

    #[test]
    fn reads_values() {
        let zoned = ColumnType::Time { zoned: true };
        let naive = ColumnType::Time { zoned: false };
        let cases = [
            (ColumnType::Int, "-42", Datum::Int(-42)),
            (ColumnType::Int, "9223372036854775807", Datum::Int(i64::MAX)),
            (ColumnType::Numeric { scale: 2 }, "12.50", Datum::Int(1250)),
            (ColumnType::Numeric { scale: 2 }, "-0.05", Datum::Int(-5)),
            (ColumnType::Numeric { scale: 2 }, "7", Datum::Int(700)),
            (ColumnType::Numeric { scale: 0 }, "7", Datum::Int(7)),
            (ColumnType::Bool, "t", Datum::Bool(true)),
            (ColumnType::Bool, "f", Datum::Bool(false)),
            (
                ColumnType::Text,
                "Upper East Side",
                Datum::Text("Upper East Side".into()),
            ),
            (ColumnType::Text, "", Datum::Text("".into())),
            // Expected values from `date -u -d ... +%s`.
            (zoned, "2024-01-15 10:30:00+00", Datum::Int(1_705_314_600)),
            (
                zoned,
                "2024-02-29 13:47:05.999+00",
                Datum::Int(1_709_214_425),
            ),
            (
                zoned,
                "2024-01-15 16:00:00+05:30",
                Datum::Int(1_705_314_600),
            ),
            (zoned, "2024-01-15 05:30:00-05", Datum::Int(1_705_314_600)),
            (zoned, "1970-01-01 00:00:00+00", Datum::Int(0)),
            (zoned, "1969-12-31 23:59:59.5+00", Datum::Int(-1)),
            (zoned, "1900-03-01 00:00:00+00", Datum::Int(-2_203_891_200)),
            (
                zoned,
                "12345-06-07 00:00:00+00",
                Datum::Int(327_416_947_200),
            ),
            (
                zoned,
                "1901-12-13 20:45:52+00:00:08",
                Datum::Int(-2_147_483_656),
            ),
            // A timestamp reads as the wall clock in UTC, whatever zone the
            // session has.
            (naive, "2024-01-15 10:30:00", Datum::Int(1_705_314_600)),
            (naive, "2024-02-29 13:47:05.999", Datum::Int(1_709_214_425)),
            (naive, "1969-12-31 23:59:59.5", Datum::Int(-1)),
            (naive, "0001-01-01 00:00:00", Datum::Int(-62_135_596_800)),
            // The infinities are times no bucket holds.
            (zoned, "infinity", Datum::Null),
            (zoned, "-infinity", Datum::Null),
            (naive, "infinity", Datum::Null),
            (naive, "-infinity", Datum::Null),
        ];
        for (column_type, text, datum) in cases {
            assert_eq!(read(column_type, text), Ok(datum), "{column_type} {text}");
        }
    }

    #[test]
    fn rejects_values() {
        let zoned = ColumnType::Time { zoned: true };
        let naive = ColumnType::Time { zoned: false };
        let cases = [
            (ColumnType::Int, "1.5"),
            (ColumnType::Int, "9223372036854775808"),
            (ColumnType::Int, ""),
            (ColumnType::Numeric { scale: 2 }, "1.234"),
            (ColumnType::Numeric { scale: 2 }, "NaN"),
            (ColumnType::Numeric { scale: 2 }, "Infinity"),
            (ColumnType::Numeric { scale: 2 }, "-Infinity"),
            (ColumnType::Numeric { scale: 2 }, "1e5"),
            (ColumnType::Numeric { scale: 2 }, ".5"),
            (ColumnType::Numeric { scale: 2 }, "1.-5"),
            (ColumnType::Numeric { scale: 0 }, "92233720368547758070"),
            (ColumnType::Numeric { scale: 0 }, "9223372036854775808"),
            (ColumnType::Bool, "true"),
            (zoned, "Infinity"),
            (zoned, "+infinity"),
            (zoned, "0044-03-15 12:00:00+00 BC"),
            (zoned, "2024-13-01 00:00:00+00"),
            (zoned, "2024-00-01 00:00:00+00"),
            (zoned, "2024-01-32 00:00:00+00"),
            (zoned, "2024-01-00 00:00:00+00"),
            (zoned, "2024-01-01 24:00:00+00"),
            (zoned, "2024-01-01 00:60:00+00"),
            (zoned, "2024-01-01 00:00:61+00"),
            (zoned, "2024-01-01 00:00+00"),
            (zoned, "2024-01-01 00:00:00:00+00"),
            (zoned, "2024-01-01 00:00:00"),
            (zoned, "2024-01-01T00:00:00+00"),
            (zoned, "2024-01 00:00:00+00"),
            (zoned, "2024-01-01 0a:00:00+00"),
            (zoned, "2024-01-01 00:00:00+0a"),
            (zoned, "2024-01-01 00:00:00+00:0a"),
            (zoned, "2024-01-01 00:00:00+00:00:0a"),
            (zoned, "2024-01-01 00:00:00+00:00:00:00"),
            (zoned, "2024-01-01 00:00:00+"),
            // A timestamp never carries an offset; one that does was read
            // under the wrong type.
            (naive, "2024-01-01 00:00:00+00"),
            (naive, "2024-01-01 00:00:00-05"),
            (naive, "0044-03-15 12:00:00 BC"),
            (naive, "2024-01-01 00:00"),
            (naive, "2024-01-01"),
        ];
        for (column_type, text) in cases {
            assert_eq!(
                read(column_type, text),
                Err(ValueError::new(column_type, text)),
                "{column_type} {text}"
            );
        }
    }

    /// A binary numeric of base-10000 `digits`, the first worth
    /// `10000^weight`.
    fn numeric_bytes(weight: i16, sign: u16, digits: &[u16]) -> Vec<u8> {
        let count = u16::try_from(digits.len()).unwrap();
        let mut bytes = count.to_be_bytes().to_vec();
        bytes.extend(weight.to_be_bytes());
        bytes.extend(sign.to_be_bytes());
        bytes.extend(0_u16.to_be_bytes());
        bytes.extend(digits.iter().flat_map(|digit| digit.to_be_bytes()));
        bytes
    }

    fn micros(unix: i64, micros: i64) -> Vec<u8> {
        ((unix - PG_EPOCH) * MICROS_PER_SECOND + micros)
            .to_be_bytes()
            .to_vec()
    }

    fn assert_reads_binary(cases: &[(ColumnType, Vec<u8>, Datum)]) {
        for (column_type, bytes, datum) in cases {
            assert_eq!(
                read_binary(*column_type, bytes),
                Ok(datum.clone()),
                "{column_type} {bytes:?}"
            );
        }
    }

    #[test]
    fn reads_binary_numbers() {
        let cents = ColumnType::Numeric { scale: 2 };
        let whole = ColumnType::Numeric { scale: 0 };
        assert_reads_binary(&[
            (ColumnType::Int, 7_i16.to_be_bytes().to_vec(), Datum::Int(7)),
            (
                ColumnType::Int,
                (-2_i32).to_be_bytes().to_vec(),
                Datum::Int(-2),
            ),
            (
                ColumnType::Int,
                i64::MAX.to_be_bytes().to_vec(),
                Datum::Int(i64::MAX),
            ),
            (
                cents,
                numeric_bytes(0, NUMERIC_POS, &[12, 5000]),
                Datum::Int(1250),
            ),
            (
                cents,
                numeric_bytes(-1, NUMERIC_NEG, &[500]),
                Datum::Int(-5),
            ),
            (cents, numeric_bytes(0, NUMERIC_POS, &[7]), Datum::Int(700)),
            (cents, numeric_bytes(0, NUMERIC_POS, &[]), Datum::Int(0)),
            (
                cents,
                numeric_bytes(2, NUMERIC_POS, &[1, 2345, 6789, 100]),
                Datum::Int(12_345_678_901),
            ),
            (whole, numeric_bytes(0, NUMERIC_POS, &[7]), Datum::Int(7)),
            (
                whole,
                numeric_bytes(4, NUMERIC_POS, &[1]),
                Datum::Int(10_000_000_000_000_000),
            ),
        ]);
    }

    #[test]
    fn reads_binary_bools_text_and_times() {
        let zoned = ColumnType::Time { zoned: true };
        let naive = ColumnType::Time { zoned: false };
        assert_reads_binary(&[
            (ColumnType::Bool, vec![1], Datum::Bool(true)),
            (ColumnType::Bool, vec![0], Datum::Bool(false)),
            (
                ColumnType::Text,
                "Bø".as_bytes().to_vec(),
                Datum::Text("Bø".into()),
            ),
            (ColumnType::Text, vec![], Datum::Text("".into())),
            (zoned, micros(PG_EPOCH, 0), Datum::Int(PG_EPOCH)),
            (zoned, micros(PG_EPOCH, -1), Datum::Int(PG_EPOCH - 1)),
            (
                zoned,
                micros(1_705_314_600, 500_000),
                Datum::Int(1_705_314_600),
            ),
            (zoned, micros(-1, 500_000), Datum::Int(-1)),
            (
                naive,
                micros(1_705_314_600, 999_999),
                Datum::Int(1_705_314_600),
            ),
            (naive, micros(-1, 500_000), Datum::Int(-1)),
            (zoned, i64::MAX.to_be_bytes().to_vec(), Datum::Null),
            (zoned, i64::MIN.to_be_bytes().to_vec(), Datum::Null),
            (naive, i64::MIN.to_be_bytes().to_vec(), Datum::Null),
        ]);
    }

    #[test]
    fn binary_value_errors() {
        let cents = ColumnType::Numeric { scale: 2 };
        let whole = ColumnType::Numeric { scale: 0 };
        let mut short = numeric_bytes(0, NUMERIC_POS, &[1, 2]);
        short.pop();
        let cases = [
            (ColumnType::Int, vec![0; 3]),
            (ColumnType::Int, vec![]),
            (ColumnType::Int, vec![0; 9]),
            (cents, numeric_bytes(0, NUMERIC_POS, &[1, 2340])),
            (cents, numeric_bytes(-2, NUMERIC_POS, &[1])),
            (cents, numeric_bytes(0, 0xc000, &[])),
            (cents, numeric_bytes(0, 0xd000, &[])),
            (cents, numeric_bytes(0, 0xf000, &[])),
            (cents, short),
            (cents, numeric_bytes(0, NUMERIC_POS, &[1])[..5].to_vec()),
            (whole, numeric_bytes(10, NUMERIC_POS, &[9999])),
            (
                whole,
                numeric_bytes(4, NUMERIC_POS, &[922, 3372, 368, 5477, 5808]),
            ),
            (ColumnType::Bool, vec![2]),
            (ColumnType::Bool, vec![]),
            (ColumnType::Text, vec![0xff]),
            (ColumnType::Time { zoned: true }, vec![0; 4]),
            (ColumnType::Time { zoned: false }, vec![0; 9]),
        ];
        for (column_type, bytes) in cases {
            assert_eq!(
                read_binary(column_type, &bytes),
                Err(ValueError::binary(column_type, &bytes)),
                "{column_type} {bytes:?}"
            );
        }
        assert_eq!(
            ValueError::binary(ColumnType::Int, &[1, 0xab]).to_string(),
            r#"can't read "\\x01ab" as an integer"#
        );
    }

    #[test]
    fn error_display() {
        let cases = [
            (ColumnType::Int, "x", r#"can't read "x" as an integer"#),
            (
                ColumnType::Numeric { scale: 2 },
                "x",
                r#"can't read "x" as a numeric of scale 2"#,
            ),
            (ColumnType::Bool, "x", r#"can't read "x" as a boolean"#),
            (ColumnType::Text, "x", r#"can't read "x" as text"#),
            (
                ColumnType::Time { zoned: true },
                "x",
                r#"can't read "x" as a timestamptz"#,
            ),
            (
                ColumnType::Time { zoned: false },
                "x",
                r#"can't read "x" as a timestamp"#,
            ),
        ];
        for (column_type, text, message) in cases {
            assert_eq!(ValueError::new(column_type, text).to_string(), message);
        }
        let placed = ValueError::new(ColumnType::Int, "x")
            .at("public.trips", &Identifier::new("fare").unwrap());
        assert_eq!(
            placed.to_string(),
            r#"can't read "x" as an integer in column fare of public.trips"#
        );
    }
}
