//! Column types straum reads, and their text form as PostgreSQL prints it
//! under `DateStyle = ISO` and `TimeZone = UTC`, which straum sets on connect.

use std::fmt;

use straum_domain::Datum;

const BOOL: u32 = 16;
const INT8: u32 = 20;
const INT2: u32 = 21;
const INT4: u32 = 23;
const TEXT: u32 = 25;
const BPCHAR: u32 = 1042;
const VARCHAR: u32 = 1043;
const TIMESTAMPTZ: u32 = 1184;
const NUMERIC: u32 = 1700;

/// `VARHDRSZ`, which a numeric typmod adds to precision and scale.
const TYPMOD_HEADER: i32 = 4;
/// The most digits an `i64` holds whole.
const MAX_PRECISION: i32 = 18;
const SECONDS_PER_DAY: i64 = 86_400;
/// Days from 0000-03-01 to 1970-01-01 in the proleptic Gregorian calendar.
const UNIX_EPOCH_DAYS: i64 = 719_468;
const DAYS_PER_ERA: i64 = 146_097;

/// How straum reads a column, from its type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// `int2`, `int4`, `int8`.
    Int,
    /// `numeric(p, s)` with `p` at most 18, as an integer in units of
    /// `10^-s`.
    Numeric {
        scale: u32,
    },
    Bool,
    /// `text`, `varchar`, `bpchar`.
    Text,
    /// `timestamptz` as Unix seconds, rounded down.
    Time,
}

impl Kind {
    /// `None` for a type straum doesn't read.
    pub(crate) fn of(type_oid: u32, typmod: i32) -> Option<Kind> {
        match type_oid {
            INT2 | INT4 | INT8 => Some(Kind::Int),
            NUMERIC => {
                // An unconstrained numeric (typmod -1) has no fixed scale.
                let packed = typmod.checked_sub(TYPMOD_HEADER).filter(|p| *p >= 0)?;
                let precision = packed >> 16;
                // The low 11 bits hold the scale, signed since PostgreSQL 15.
                let scale = ((packed & 0x7ff) ^ 1024) - 1024;
                let scale = u32::try_from(scale).ok()?;
                (precision <= MAX_PRECISION).then_some(Kind::Numeric { scale })
            }
            BOOL => Some(Kind::Bool),
            TEXT | VARCHAR | BPCHAR => Some(Kind::Text),
            TIMESTAMPTZ => Some(Kind::Time),
            _ => None,
        }
    }

    /// The scale of sums over a column of this kind.
    pub(crate) fn scale(self) -> u32 {
        match self {
            Kind::Numeric { scale } => scale,
            _ => 0,
        }
    }

    pub(crate) fn read(self, text: &str) -> Result<Datum, ValueError> {
        let datum = match self {
            Kind::Int => text.parse().ok().map(Datum::Int),
            Kind::Numeric { scale } => decimal(text, scale)
                .and_then(|v| i64::try_from(v).ok())
                .map(Datum::Int),
            Kind::Bool => match text {
                "t" => Some(Datum::Bool(true)),
                "f" => Some(Datum::Bool(false)),
                _ => None,
            },
            Kind::Text => Some(Datum::Text(text.into())),
            Kind::Time => timestamptz(text).map(Datum::Int),
        };
        datum.ok_or_else(|| ValueError::new(self, text))
    }
}

/// A sum as PostgreSQL prints it, scaled like the column it sums.
pub(crate) fn read_sum(text: &str, scale: u32) -> Result<i128, ValueError> {
    decimal(text, scale).ok_or_else(|| ValueError::new(Kind::Numeric { scale }, text))
}

/// `-123.45` at scale 2 is -12345. More fraction digits than the scale, or
/// anything else (`NaN`, `Infinity`, an exponent), is `None`.
fn decimal(text: &str, scale: u32) -> Option<i128> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let all_digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() || !all_digits(whole) || !all_digits(fraction) {
        return None;
    }
    let pad = scale.checked_sub(u32::try_from(fraction.len()).ok()?)?;
    let mut value: i128 = format!("{whole}{fraction}").parse().ok()?;
    value = value.checked_mul(10_i128.checked_pow(pad)?)?;
    Some(if negative { -value } else { value })
}

/// `2024-01-15 10:30:00.5+00` is 1705314600. Offsets other than `+00` are
/// read, though straum always asks for UTC. `BC` dates and `infinity` are
/// `None`.
fn timestamptz(text: &str) -> Option<i64> {
    let (date, rest) = text.split_once(' ')?;
    let (year, date) = date.rsplit_once('-').and_then(|(ym, day)| {
        let (year, month) = ym.rsplit_once('-')?;
        Some((year, (month, day)))
    })?;
    let (month, day) = date;
    let offset_at = rest.find(['+', '-'])?;
    let (clock, offset) = rest.split_at(offset_at);
    let (clock, _fraction) = clock.split_once('.').unwrap_or((clock, ""));
    let mut clock = clock.split(':');
    let mut field = |max: i64| {
        let n: i64 = number(clock.next()?)?;
        (n <= max).then_some(n)
    };
    let (hour, minute, second) = (field(23)?, field(59)?, field(60)?);
    if clock.next().is_some() {
        return None;
    }
    let days = days_from_civil(number(year)?, number(month)?, number(day)?)?;
    let seconds = days * SECONDS_PER_DAY + hour * 3_600 + minute * 60 + second;
    Some(seconds - offset_seconds(offset)?)
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
    Some(sign * (hours * 3_600 + minutes * 60 + seconds))
}

/// Digits only; no sign.
fn number(text: &str) -> Option<i64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
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

/// A value PostgreSQL sent that doesn't read as its column's kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueError {
    kind: Kind,
    text: String,
}

impl ValueError {
    fn new(kind: Kind, text: &str) -> ValueError {
        ValueError {
            kind,
            text: text.to_owned(),
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Kind::Int => f.write_str("an integer"),
            Kind::Numeric { scale } => write!(f, "a numeric of scale {scale}"),
            Kind::Bool => f.write_str("a boolean"),
            Kind::Text => f.write_str("text"),
            Kind::Time => f.write_str("a timestamptz"),
        }
    }
}

impl fmt::Display for ValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "can't read {:?} as {}", self.text, self.kind)
    }
}

impl std::error::Error for ValueError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn numeric(precision: i32, scale: i32) -> i32 {
        ((precision << 16) | (scale & 0x7ff)) + TYPMOD_HEADER
    }

    #[test]
    fn kinds_from_types() {
        let cases = [
            (INT2, -1, Some(Kind::Int)),
            (INT4, -1, Some(Kind::Int)),
            (INT8, -1, Some(Kind::Int)),
            (NUMERIC, numeric(12, 2), Some(Kind::Numeric { scale: 2 })),
            (NUMERIC, numeric(18, 0), Some(Kind::Numeric { scale: 0 })),
            (NUMERIC, numeric(19, 2), None),
            (NUMERIC, numeric(5, -2), None),
            (NUMERIC, -1, None),
            (BOOL, -1, Some(Kind::Bool)),
            (TEXT, -1, Some(Kind::Text)),
            (VARCHAR, 14, Some(Kind::Text)),
            (BPCHAR, 7, Some(Kind::Text)),
            (TIMESTAMPTZ, -1, Some(Kind::Time)),
            // float8, uuid, timestamp, date
            (701, -1, None),
            (2950, -1, None),
            (1114, -1, None),
            (1082, -1, None),
        ];
        for (oid, typmod, kind) in cases {
            assert_eq!(Kind::of(oid, typmod), kind, "{oid} {typmod}");
        }
        assert_eq!(Kind::Numeric { scale: 3 }.scale(), 3);
        assert_eq!(Kind::Int.scale(), 0);
    }

    #[test]
    fn reads_values() {
        let text = |s: &str| Datum::Text(s.into());
        let cases = [
            (Kind::Int, "-42", Datum::Int(-42)),
            (Kind::Int, "9223372036854775807", Datum::Int(i64::MAX)),
            (Kind::Numeric { scale: 2 }, "12.50", Datum::Int(1250)),
            (Kind::Numeric { scale: 2 }, "-0.05", Datum::Int(-5)),
            (Kind::Numeric { scale: 2 }, "7", Datum::Int(700)),
            (Kind::Numeric { scale: 0 }, "7", Datum::Int(7)),
            (Kind::Bool, "t", Datum::Bool(true)),
            (Kind::Bool, "f", Datum::Bool(false)),
            (Kind::Text, "Upper East Side", text("Upper East Side")),
            (Kind::Text, "", text("")),
            // Expected values from `date -u -d ... +%s`.
            (
                Kind::Time,
                "2024-01-15 10:30:00+00",
                Datum::Int(1_705_314_600),
            ),
            (
                Kind::Time,
                "2024-02-29 13:47:05.999+00",
                Datum::Int(1_709_214_425),
            ),
            (
                Kind::Time,
                "2024-01-15 16:00:00+05:30",
                Datum::Int(1_705_314_600),
            ),
            (
                Kind::Time,
                "2024-01-15 05:30:00-05",
                Datum::Int(1_705_314_600),
            ),
            (Kind::Time, "1970-01-01 00:00:00+00", Datum::Int(0)),
            (Kind::Time, "1969-12-31 23:59:59.5+00", Datum::Int(-1)),
            (
                Kind::Time,
                "1900-03-01 00:00:00+00",
                Datum::Int(-2_203_891_200),
            ),
            (
                Kind::Time,
                "12345-06-07 00:00:00+00",
                Datum::Int(327_416_947_200),
            ),
            (
                Kind::Time,
                "1901-12-13 20:45:52+00:00:08",
                Datum::Int(-2_147_483_656),
            ),
        ];
        for (kind, text, datum) in cases {
            assert_eq!(kind.read(text), Ok(datum), "{kind} {text}");
        }
    }

    #[test]
    fn rejects_values() {
        let cases = [
            (Kind::Int, "1.5"),
            (Kind::Int, "9223372036854775808"),
            (Kind::Int, ""),
            (Kind::Numeric { scale: 2 }, "1.234"),
            (Kind::Numeric { scale: 2 }, "NaN"),
            (Kind::Numeric { scale: 2 }, "Infinity"),
            (Kind::Numeric { scale: 2 }, "-Infinity"),
            (Kind::Numeric { scale: 2 }, "1e5"),
            (Kind::Numeric { scale: 2 }, ".5"),
            (Kind::Numeric { scale: 2 }, "1.-5"),
            (Kind::Numeric { scale: 0 }, "92233720368547758070"),
            (Kind::Numeric { scale: 0 }, "9223372036854775808"),
            (Kind::Bool, "true"),
            (Kind::Time, "infinity"),
            (Kind::Time, "-infinity"),
            (Kind::Time, "0044-03-15 12:00:00+00 BC"),
            (Kind::Time, "2024-13-01 00:00:00+00"),
            (Kind::Time, "2024-00-01 00:00:00+00"),
            (Kind::Time, "2024-01-32 00:00:00+00"),
            (Kind::Time, "2024-01-00 00:00:00+00"),
            (Kind::Time, "2024-01-01 24:00:00+00"),
            (Kind::Time, "2024-01-01 00:60:00+00"),
            (Kind::Time, "2024-01-01 00:00:61+00"),
            (Kind::Time, "2024-01-01 00:00+00"),
            (Kind::Time, "2024-01-01 00:00:00:00+00"),
            (Kind::Time, "2024-01-01 00:00:00"),
            (Kind::Time, "2024-01-01T00:00:00+00"),
            (Kind::Time, "2024-01 00:00:00+00"),
            (Kind::Time, "2024-01-01 0a:00:00+00"),
            (Kind::Time, "2024-01-01 00:00:00+0a"),
            (Kind::Time, "2024-01-01 00:00:00+00:0a"),
            (Kind::Time, "2024-01-01 00:00:00+00:00:0a"),
            (Kind::Time, "2024-01-01 00:00:00+00:00:00:00"),
            (Kind::Time, "2024-01-01 00:00:00+"),
        ];
        for (kind, text) in cases {
            assert_eq!(
                kind.read(text),
                Err(ValueError::new(kind, text)),
                "{kind} {text}"
            );
        }
    }

    #[test]
    fn reads_sums() {
        assert_eq!(
            read_sum("170141183460469231731687303715884105727", 0),
            Ok(i128::MAX)
        );
        assert_eq!(read_sum("-12.5", 2), Ok(-1250));
        assert_eq!(
            read_sum("12.345", 2),
            Err(ValueError::new(Kind::Numeric { scale: 2 }, "12.345"))
        );
        assert_eq!(
            read_sum("170141183460469231731687303715884105727", 1),
            Err(ValueError::new(
                Kind::Numeric { scale: 1 },
                "170141183460469231731687303715884105727"
            ))
        );
    }

    #[test]
    fn error_display() {
        let cases = [
            (Kind::Int, "x", r#"can't read "x" as an integer"#),
            (
                Kind::Numeric { scale: 2 },
                "x",
                r#"can't read "x" as a numeric of scale 2"#,
            ),
            (Kind::Bool, "x", r#"can't read "x" as a boolean"#),
            (Kind::Text, "x", r#"can't read "x" as text"#),
            (Kind::Time, "x", r#"can't read "x" as a timestamptz"#),
        ];
        for (kind, text, msg) in cases {
            assert_eq!(ValueError::new(kind, text).to_string(), msg);
        }
    }
}
