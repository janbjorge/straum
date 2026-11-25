//! Column types Straum reads, by their type OIDs: their text form as
//! PostgreSQL prints it under `DateStyle = ISO` and `TimeZone = UTC`, which
//! Straum sets on connect, for the logical stream; and their binary form,
//! which loads (`load.rs`) read. Each becomes a `Datum`, the domain's
//! value. Also PostgreSQL's timestamps as clock times, as the stream reads
//! and writes them.

use std::fmt::{self, Write as _};
use std::str;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use straum_domain::{
    ColumnType, Datum, Era, Identifier, parse_calendar_day, parse_date, parse_uuid,
};

/// An object id: how PostgreSQL names a row of its catalog, such as a
/// table (`pg_class`), a type (`pg_type`) or an enum label (`pg_enum`).
pub(crate) type Oid = u32;

/// The OIDs of the built-in types Straum reads, from `pg_type`.
pub(crate) const BOOL: Oid = 16;
pub(crate) const INT8: Oid = 20;
pub(crate) const INT2: Oid = 21;
pub(crate) const INT4: Oid = 23;
pub(crate) const TEXT: Oid = 25;
pub(crate) const FLOAT4: Oid = 700;
pub(crate) const FLOAT8: Oid = 701;
pub(crate) const BPCHAR: Oid = 1042;
pub(crate) const VARCHAR: Oid = 1043;
pub(crate) const DATE: Oid = 1082;
pub(crate) const TIMESTAMP: Oid = 1114;
pub(crate) const TIMESTAMPTZ: Oid = 1184;
pub(crate) const NUMERIC: Oid = 1700;
pub(crate) const UUID: Oid = 2950;

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
/// `NBASE`: a numeric digit holds four decimal ones.
const NUMERIC_BASE: u128 = 10_000;

/// How Straum reads a column of this type: `int2`, `int4` and `int8` as
/// integers, `numeric(p, s)` with `p` at most 18 as an integer in units of
/// `10^-s`, `text`, `varchar` and `bpchar` as text, `timestamptz` as Unix
/// seconds, rounded down, and `timestamp` as the Unix seconds its wall
/// clock reading would be in UTC, `date` as days since 2000-01-01 and
/// `uuid` as its bytes. `None` for a type Straum doesn't read: among them
/// `real` and `double precision`, whose sums can't equal PostgreSQL's.
pub(crate) fn type_of(type_oid: Oid, typmod: i32) -> Option<ColumnType> {
    match type_oid {
        INT2 | INT4 | INT8 => Some(ColumnType::Int),
        NUMERIC => {
            // An unconstrained numeric (typmod -1) has no fixed scale.
            let packed = typmod.checked_sub(TYPMOD_HEADER)?;
            if packed < 0 {
                return None;
            }
            let precision = packed >> TYPMOD_PRECISION_SHIFT;
            // The low 11 bits hold the scale, signed since PostgreSQL 15.
            let scale = ((packed & TYPMOD_SCALE_MASK) ^ TYPMOD_SCALE_SIGN) - TYPMOD_SCALE_SIGN;
            let scale = u32::try_from(scale).ok()?;
            if precision > MAX_PRECISION {
                return None;
            }
            Some(ColumnType::Numeric { scale })
        }
        BOOL => Some(ColumnType::Bool),
        TEXT | VARCHAR | BPCHAR => Some(ColumnType::Text),
        TIMESTAMPTZ => Some(ColumnType::Time { zoned: true }),
        TIMESTAMP => Some(ColumnType::Time { zoned: false }),
        DATE => Some(ColumnType::Date),
        UUID => Some(ColumnType::Uuid),
        _ => None,
    }
}

/// Reads a value's text form as `column_type`.
pub(crate) fn read(column_type: ColumnType, text: &str) -> Result<Datum, ValueError> {
    let datum = match column_type {
        ColumnType::Int => text.parse().ok().map(Datum::Int),
        ColumnType::Numeric { scale } => match decimal(text, scale).map(i64::try_from) {
            Some(Ok(value)) => Some(Datum::Int(value)),
            _ => None,
        },
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
        // Unlike a time's, a date's infinities are kept: a date is a key,
        // and PostgreSQL groups by them.
        ColumnType::Date => parse_date(text).map(Datum::Date),
        ColumnType::Uuid => parse_uuid(text).map(Datum::Uuid),
    };
    match datum {
        Some(datum) => Ok(datum),
        None => Err(ValueError::new(column_type, text)),
    }
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
        ColumnType::Text => match str::from_utf8(bytes) {
            Ok(text) => Some(Datum::Text(text.into())),
            Err(_) => None,
        },
        // Both types are microseconds since 2000-01-01, a `timestamp`'s
        // read as if UTC.
        ColumnType::Time { .. } => match <[u8; 8]>::try_from(bytes) {
            Ok(array) => Some(time(i64::from_be_bytes(array))),
            Err(_) => None,
        },
        // Days since 2000-01-01, as the text reader has them.
        ColumnType::Date => match <[u8; 4]>::try_from(bytes) {
            Ok(array) => Some(Datum::Date(i32::from_be_bytes(array))),
            Err(_) => None,
        },
        ColumnType::Uuid => match <[u8; 16]>::try_from(bytes) {
            Ok(array) => Some(Datum::Uuid(array)),
            Err(_) => None,
        },
    };
    match datum {
        Some(datum) => Ok(datum),
        None => Err(ValueError::binary(column_type, bytes)),
    }
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
    let Ok(since) = time.duration_since(UNIX_EPOCH + PG_EPOCH_AFTER_UNIX) else {
        return 0;
    };
    i64::try_from(since.as_micros()).unwrap_or(i64::MAX)
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
    let digits = digits.iter().copied().map(u16::from_be_bytes);
    scaled(digits, weight.into(), negative, scale)
}

/// The two bytes at `at`, if `bytes` reach that far.
fn word_at(bytes: &[u8], at: usize) -> Option<[u8; 2]> {
    bytes.get(at..)?.first_chunk().copied()
}

/// Base-10000 digits, the first worth `10000^weight`, in units of
/// `10^-scale`, as [`decimal`] scales text. `None` for a digit below the
/// scale, or past an `i64`.
// `impl Iterator`: the load and the WAL read digits from different byte
// layouts, and neither collects them first.
pub(crate) fn scaled(
    digits: impl Iterator<Item = u16>,
    weight: i32,
    negative: bool,
    scale: u32,
) -> Option<i64> {
    let scale = i32::try_from(scale).ok()?;
    // Horner's rule, then one power of ten for the last digit's place: a
    // power per digit was a twentieth of reading a WAL row
    // (`physical_wal_bench`). A value that fits an `i64` has few digits,
    // since PostgreSQL drops the zero digits at either end.
    let mut value: u128 = 0;
    let mut count: i32 = 0;
    for digit in digits {
        value = value.checked_mul(NUMERIC_BASE)?.checked_add(digit.into())?;
        count = count.checked_add(1)?;
    }
    let last_place = weight.checked_sub(count)?.checked_add(1)?;
    let exponent = DECIMAL_DIGITS.checked_mul(last_place)?.checked_add(scale)?;
    let magnitude = match u32::try_from(exponent) {
        Ok(exponent) => value.checked_mul(10_u128.checked_pow(exponent)?)?,
        // Below the scale the digits must be zero.
        Err(_) => match 10_u128.checked_pow(exponent.unsigned_abs()) {
            Some(unit) => divide_exactly(value, unit)?,
            None if value == 0 => 0,
            None => return None,
        },
    };
    let magnitude = i128::try_from(magnitude).ok()?;
    i64::try_from(if negative { -magnitude } else { magnitude }).ok()
}

/// `value / unit` when `unit` divides it, in 64 bits when both fit: a
/// 128-bit division took most of the time of reading a numeric
/// (`physical_wal_bench`).
fn divide_exactly(value: u128, unit: u128) -> Option<u128> {
    if let (Ok(value), Ok(unit)) = (u64::try_from(value), u64::try_from(unit)) {
        return value.is_multiple_of(unit).then_some((value / unit).into());
    }
    value.is_multiple_of(unit).then_some(value / unit)
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
    let mut value = append_digits(0, whole)?;
    value = append_digits(value, fraction)?;
    value = value.checked_mul(10_i128.checked_pow(pad)?)?;
    Some(if negative { -value } else { value })
}

/// `value` with the decimal `digits` written after it: 12 and `"34"` is
/// 1234. `None` past an `i128`. Every byte of `digits` is an ASCII digit.
fn append_digits(mut value: i128, digits: &str) -> Option<i128> {
    for byte in digits.bytes() {
        let digit = i128::from(byte.checked_sub(b'0')?);
        value = value.checked_mul(10)?.checked_add(digit)?;
    }
    Some(value)
}

/// `2024-01-15 10:30:00.5+00` is 1705314600. Offsets other than `+00` are
/// read, though Straum always asks for UTC. A `timestamp` has no offset,
/// and its text must have none. A ` BC` at the end counts years back from
/// 1 BC, the year before 1 AD, as the binary form does. The date part
/// reads by the same rules as a `date`.
fn timestamp(text: &str, zoned: bool) -> Option<i64> {
    let (text, era) = match text.strip_suffix(" BC") {
        Some(text) => (text, Era::BeforeChrist),
        None => (text, Era::AnnoDomini),
    };
    let (date, rest) = text.split_once(' ')?;
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
    // A date part's year has at most seven digits, so these seconds stay
    // far inside an `i64`; only the offset needs a checked step.
    let days = parse_calendar_day(date, era)?;
    let seconds =
        days * SECONDS_PER_DAY + hour * SECONDS_PER_HOUR + minute * SECONDS_PER_MINUTE + second;
    let offset = if zoned { offset_seconds(offset)? } else { 0 };
    seconds.checked_sub(offset)
}

/// An hour, minute or second of a clock, at most `max`.
fn clock_field(text: Option<&str>, max: i64) -> Option<i64> {
    let value = number(text?)?;
    if value > max {
        return None;
    }
    Some(value)
}

/// `+05:30` is 19800 seconds. `None` for an offset past an `i64` of
/// seconds, which PostgreSQL never prints.
fn offset_seconds(text: &str) -> Option<i64> {
    let sign = if text.starts_with('-') { -1 } else { 1 };
    let mut parts = text[1..].split(':');
    let hours: i64 = number(parts.next()?)?;
    let minutes: i64 = parts.next().map_or(Some(0), number)?;
    let seconds: i64 = parts.next().map_or(Some(0), number)?;
    if parts.next().is_some() {
        return None;
    }
    let total = hours
        .checked_mul(SECONDS_PER_HOUR)?
        .checked_add(minutes.checked_mul(SECONDS_PER_MINUTE)?)?
        .checked_add(seconds)?;
    Some(sign * total)
}

/// Digits only; no sign.
fn number(text: &str) -> Option<i64> {
    if text.is_empty() || !all_digits(text) {
        return None;
    }
    text.parse().ok()
}

/// Whether `text` is ASCII digits only, with no sign; an empty text is.
pub(crate) fn all_digits(text: &str) -> bool {
    for byte in text.bytes() {
        if !byte.is_ascii_digit() {
            return false;
        }
    }
    true
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
    use crate::wal::test_support::Rng;

    /// `scaled` as written first, a power of ten per digit: the reference
    /// the one-pass rule must match.
    fn scaled_digit_by_digit(
        digits: &[u16],
        weight: i32,
        negative: bool,
        scale: u32,
    ) -> Option<i64> {
        let scale = i32::try_from(scale).ok()?;
        let mut value: i128 = 0;
        for (position, &digit) in digits.iter().enumerate() {
            let exponent = DECIMAL_DIGITS * (weight - i32::try_from(position).ok()?) + scale;
            let scaled = match u32::try_from(exponent) {
                Ok(exponent) => i128::from(digit).checked_mul(10_i128.checked_pow(exponent)?)?,
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

    #[test]
    fn scales_numerics_as_the_reference_does() {
        let mut rng = Rng(7);
        let mut read = 0;
        for _ in 0..200_000 {
            let mut digits = Vec::new();
            for _ in 0..rng.below(8) {
                // Mostly whole digits, often ones with zeros at the end.
                let digit = match rng.below(3) {
                    0 => rng.below(10) * 1000,
                    1 => rng.below(100) * 100,
                    _ => rng.below(10_000),
                };
                digits.push(u16::try_from(digit).unwrap());
            }
            let weight = i32::try_from(rng.below(12)).unwrap() - 6;
            let negative = rng.below(2) == 0;
            let scale = u32::try_from(rng.below(10)).unwrap();
            let ours = scaled(digits.iter().copied(), weight, negative, scale);
            let reference = scaled_digit_by_digit(&digits, weight, negative, scale);
            assert_eq!(ours, reference, "{digits:?} weight {weight} scale {scale}");
            read += usize::from(ours.is_some());
        }
        // Both answers, so neither side is vacuous.
        assert!(read > 20_000, "{read}");
    }

    /// Text numerics read whole up to an `i128`, and none past it.
    #[test]
    fn decimals_up_to_the_i128_boundary() {
        let max = i128::MAX.to_string();
        let past = (u128::try_from(i128::MAX).unwrap() + 1).to_string();
        let (head, last) = max.split_at(max.len() - 1);
        let cases = [
            (max.clone(), 0, Some(i128::MAX)),
            (format!("-{max}"), 0, Some(-i128::MAX)),
            (format!("0000{max}"), 0, Some(i128::MAX)),
            (format!("{head}.{last}"), 1, Some(i128::MAX)),
            (head.to_owned(), 1, Some(i128::MAX - 7)),
            (past.clone(), 0, None),
            (format!("-{past}"), 0, None),
            (format!("{max}0"), 0, None),
            // Whole, but past an `i128` once scaled.
            (max, 1, None),
        ];
        for (text, scale, expected) in cases {
            assert_eq!(decimal(&text, scale), expected, "{text} at scale {scale}");
        }
    }

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
            (DATE, -1, Some(ColumnType::Date)),
            (UUID, -1, Some(ColumnType::Uuid)),
            (FLOAT4, -1, None),
            (FLOAT8, -1, None),
            // time, interval, jsonb
            (1083, -1, None),
            (1186, -1, None),
            (3802, -1, None),
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
            // 1 BC, a leap year, is year 0.
            (naive, "0001-01-01 00:00:00 BC", Datum::Int(-62_167_219_200)),
            (
                zoned,
                "0044-03-15 12:00:00+00 BC",
                Datum::Int(-63_517_780_800),
            ),
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

    /// The ends of the range a timestamp holds, 4714-11-24 BC to
    /// 294276-12-31 AD. Expected values from Howard Hinnant's
    /// `days_from_civil` run in Python.
    #[test]
    fn reads_timestamps_at_the_ends_of_their_range() {
        let zoned = ColumnType::Time { zoned: true };
        let naive = ColumnType::Time { zoned: false };
        let cases = [
            (
                zoned,
                "4714-11-24 00:00:00+00 BC",
                Datum::Int(-210_866_803_200),
            ),
            (
                zoned,
                "294276-12-31 23:59:59.999999+00",
                Datum::Int(9_224_318_015_999),
            ),
            (
                naive,
                "4714-11-24 00:00:00 BC",
                Datum::Int(-210_866_803_200),
            ),
            (
                naive,
                "294276-12-31 23:59:59.999999",
                Datum::Int(9_224_318_015_999),
            ),
        ];
        for (column_type, text, datum) in cases {
            assert_eq!(read(column_type, text), Ok(datum), "{column_type} {text}");
        }
    }

    #[test]
    fn reads_dates_and_uuids() {
        let cases = [
            // Days since 2000-01-01; a date keeps its infinities.
            (ColumnType::Date, "2024-01-15", Datum::Date(8_780)),
            (ColumnType::Date, "0044-03-15 BC", Datum::Date(-746_117)),
            (ColumnType::Date, "infinity", Datum::Date(i32::MAX)),
            (ColumnType::Date, "-infinity", Datum::Date(i32::MIN)),
            (
                ColumnType::Uuid,
                "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
                Datum::Uuid([
                    0xa0, 0xee, 0xbc, 0x99, 0x9c, 0x0b, 0x4e, 0xf8, 0xbb, 0x6d, 0x6b, 0xb9, 0xbd,
                    0x38, 0x0a, 0x11,
                ]),
            ),
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
            (zoned, "0000-01-01 00:00:00+00 BC"),
            // A date part a `date` would refuse: year zero, or digits of
            // the wrong width.
            (zoned, "0000-01-01 00:00:00+00"),
            (naive, "0000-01-01 00:00:00"),
            (zoned, "2024-1-15 00:00:00+00"),
            (zoned, "2024-01-5 00:00:00+00"),
            (zoned, "24-01-15 00:00:00+00"),
            (zoned, "02024-01-15 00:00:00+00"),
            (naive, "12345678-01-01 00:00:00"),
            (zoned, "0044-03-15 12:00:00+00 AD"),
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
            // An offset past an `i64` of seconds, on its own or with the
            // time it shifts.
            (zoned, "2024-01-01 00:00:00+9999999999999999"),
            (zoned, "2024-01-01 00:00:00+00:999999999999999999"),
            (zoned, "2024-01-01 00:00:00-00:00:9223372036854775807"),
            (zoned, "0044-03-15 12:00:00+00:00:9223372036854775807 BC"),
            // A timestamp never carries an offset; one that does was read
            // under the wrong type.
            (naive, "2024-01-01 00:00:00+00"),
            (naive, "2024-01-01 00:00:00-05"),
            (naive, "0044-03-15 12:00:00+00 BC"),
            (naive, "2024-01-01 00:00"),
            (naive, "2024-01-01"),
            (naive, "2023-02-29 00:00:00"),
            (naive, "9999999999-01-01 00:00:00"),
            (ColumnType::Date, "2024-01-15 00:00:00"),
            (ColumnType::Date, "2023-02-29"),
            (ColumnType::Date, "01/15/2024"),
            (ColumnType::Uuid, "A0EEBC99-9C0B-4EF8-BB6D-6BB9BD380A11"),
            (ColumnType::Uuid, "a0eebc999c0b4ef8bb6d6bb9bd380a11"),
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
        for digit in digits {
            bytes.extend(digit.to_be_bytes());
        }
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
    fn reads_binary_dates_and_uuids() {
        assert_reads_binary(&[
            (
                ColumnType::Date,
                8_780_i32.to_be_bytes().to_vec(),
                Datum::Date(8_780),
            ),
            (
                ColumnType::Date,
                (-746_117_i32).to_be_bytes().to_vec(),
                Datum::Date(-746_117),
            ),
            (
                ColumnType::Date,
                i32::MAX.to_be_bytes().to_vec(),
                Datum::Date(i32::MAX),
            ),
            (
                ColumnType::Uuid,
                (0..16).collect(),
                Datum::Uuid([0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]),
            ),
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
            (ColumnType::Date, vec![0; 8]),
            (ColumnType::Date, vec![0; 3]),
            (ColumnType::Uuid, vec![0; 15]),
            (ColumnType::Uuid, vec![0; 17]),
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
            (ColumnType::Date, "x", r#"can't read "x" as a date"#),
            (ColumnType::Uuid, "x", r#"can't read "x" as a uuid"#),
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
