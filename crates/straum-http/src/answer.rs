//! An answer as JSON:
//!
//! ```json
//! {
//!   "as_of": "2024-01-31T22:59:59.123456Z",
//!   "rows": [
//!     {"bucket": "2024-01-01T00:00:00Z", "pickup.borough": "Bronx",
//!      "trips": 412, "fare": 6234.50, "fare_avg": 15.1323}
//!   ]
//! }
//! ```
//!
//! - `as_of`: every commit made before it, by Straum's clock, is in the
//!   answer.
//! - `rows`: one object per group, keyed by the names the ask selected.
//! - Time keys are RFC 3339 in UTC; over a `timestamp` column, which has no
//!   zone, they are the wall clock reading with a `Z`. Numeric values are
//!   exact decimals in
//!   their column's scale; avg is rounded half to even at four places
//!   past it, with trailing zeros dropped.
//!
//! The LSN the answer reflects goes in the `Straum-LSN` header: a caller
//! can't wait for one yet, so it is there for debugging.

use jiff::Timestamp;
use straum_app::Answered;
use straum_domain::{AnswerRow, Ask, Datum, Value};

use crate::json::{decimal, string};

/// How far an ask's range stays clear of the earliest time jiff can write,
/// so a time key that starts up to a month before it still prints.
const LOOK_BACK: i64 = 32 * 86_400;

/// A time key starts in its ask's range or at most `LOOK_BACK` before it,
/// so an ask that passes here never fails `write`, whoever read it.
pub(crate) fn range_errors(ask: &Ask) -> Vec<String> {
    let earliest = Timestamp::MIN.as_second() + LOOK_BACK;
    let latest = Timestamp::MAX.as_second();
    [("start", ask.from), ("end", ask.to)]
        .into_iter()
        .filter(|(_, at)| !(earliest..=latest).contains(at))
        .map(|(end, at)| format!("range {end} {at} is not between {earliest} and {latest}"))
        .collect()
}

/// Places past a column's scale that avg keeps.
const AVG_PLACES: u32 = 4;

/// The body; the LSN is the caller's to send.
pub(crate) fn write(answered: &Answered) -> Vec<u8> {
    let Answered { answer, as_of, .. } = answered;
    let mut out = Vec::new();
    out.extend_from_slice(b"{\"as_of\":");
    // Past jiff's year 9999, which only a broken clock gives; 1970 is
    // still a true lower bound.
    let as_of = Timestamp::try_from(*as_of).unwrap_or(Timestamp::UNIX_EPOCH);
    string(&mut out, &as_of.to_string());
    out.extend_from_slice(b",\"rows\":[");
    for (i, AnswerRow { keys, values }) in answer.rows.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        let (key_columns, measure_columns) = answer.columns.split_at(keys.len());
        out.push(b'{');
        let mut first = true;
        for (column, datum) in key_columns.iter().zip(keys) {
            field(&mut out, &column.name, &mut first);
            match datum {
                &Datum::Int(seconds) if column.time => {
                    let at = Timestamp::from_second(seconds)
                        .expect("range_errors keeps time keys in range");
                    string(&mut out, &at.to_string());
                }
                datum => write_datum(&mut out, datum, column.scale),
            }
        }
        for (column, value) in measure_columns.iter().zip(values) {
            field(&mut out, &column.name, &mut first);
            write_value(&mut out, value, column.scale);
        }
        out.push(b'}');
    }
    out.extend_from_slice(b"]}");
    out
}

/// Starts a field of a row: a comma unless it is the first, then the name.
fn field(out: &mut Vec<u8>, name: &str, first: &mut bool) {
    if *first {
        *first = false;
    } else {
        out.push(b',');
    }
    string(out, name);
    out.push(b':');
}

fn write_datum(out: &mut Vec<u8>, datum: &Datum, scale: u32) {
    match datum {
        Datum::Null => out.extend_from_slice(b"null"),
        Datum::Bool(true) => out.extend_from_slice(b"true"),
        Datum::Bool(false) => out.extend_from_slice(b"false"),
        Datum::Int(n) => decimal(out, i128::from(*n), scale),
        Datum::Text(s) => string(out, s),
    }
}

fn write_value(out: &mut Vec<u8>, value: &Value, scale: u32) {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Int(n) => decimal(out, *n, scale),
        Value::Avg { sum, count } => {
            decimal(out, avg(*sum, *count), scale + AVG_PLACES);
            // The places past the column's scale are only as long as they
            // need to be.
            while out.last() == Some(&b'0') {
                out.pop();
            }
            if out.last() == Some(&b'.') {
                out.pop();
            }
        }
    }
}

/// `sum / count` in units of `10^-AVG_PLACES` of the sum's, rounded half to
/// even. The whole part is an average of `i64`s, so it fits with room for
/// the places.
fn avg(sum: i128, count: u64) -> i128 {
    let negative = sum < 0;
    let (sum, count) = (sum.unsigned_abs(), u128::from(count));
    let shift = 10u128.pow(AVG_PLACES);
    let tail = sum % count * shift;
    let mut units = sum / count * shift + tail / count;
    let (rest, half) = (tail % count * 2, count);
    if rest > half || (rest == half && units % 2 == 1) {
        units += 1;
    }
    let units = i128::try_from(units).expect("an average of i64s");
    if negative { -units } else { units }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_values() {
        let mut out = Vec::new();
        for (datum, scale) in [
            (Datum::Null, 0),
            (Datum::Bool(true), 0),
            (Datum::Bool(false), 0),
            (Datum::Int(-15), 1),
            (Datum::Text("a\"b".into()), 0),
        ] {
            write_datum(&mut out, &datum, scale);
            out.push(b' ');
        }
        for (value, scale) in [
            (Value::Null, 2),
            (Value::Int(7), 2),
            (Value::Avg { sum: 15, count: 4 }, 1),
        ] {
            write_value(&mut out, &value, scale);
            out.push(b' ');
        }
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"null true false -1.5 "a\"b" null 0.07 0.375 "#
        );
    }

    #[test]
    fn rounds_avg_half_to_even() {
        let written = |sum, count, scale| {
            let mut out = Vec::new();
            write_value(&mut out, &Value::Avg { sum, count }, scale);
            String::from_utf8(out).unwrap()
        };
        assert_eq!(written(2, 3, 0), "0.6667");
        assert_eq!(written(-1, 3, 2), "-0.003333");
        // Ties go to the even last place, either way.
        assert_eq!(written(1, 20_000, 0), "0");
        assert_eq!(written(3, 20_000, 0), "0.0002");
        assert_eq!(written(-3, 20_000, 0), "-0.0002");
        // Trailing zeros go, but not those of the whole part.
        assert_eq!(written(10, 1, 0), "10");
        assert_eq!(written(1_595, 5, 2), "3.19");
    }

    #[test]
    fn averages_the_widest_sums() {
        let max = i128::from(i64::MAX);
        assert_eq!(avg(max * 3, 3), max * 10_000);
        assert_eq!(avg(-max * 3, 3), -max * 10_000);
    }
}
