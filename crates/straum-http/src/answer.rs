//! An answer as JSON:
//!
//! ```json
//! {
//!   "lsn": "16/B374D848",
//!   "commit_time": "2024-01-31T22:59:59.123456Z",
//!   "columns": ["bucket", "pickup.borough", "trips", "fare", "fare_avg"],
//!   "rows": [["2024-01-01T00:00:00Z", "Bronx", 412, 6234.50,
//!             {"sum": 6234.50, "count": 412}]]
//! }
//! ```
//!
//! - `lsn`: the answer reflects every commit up to here and none after.
//! - `commit_time`: when the commit at `lsn` committed, by the server's
//!   clock; null when `lsn` is where the stream started.
//! - Time keys are RFC 3339 in UTC. Numeric values are exact
//!   decimals in their column's scale. avg is its sum and count, so the
//!   caller picks the rounding.

use std::mem;

use jiff::Timestamp;
use straum_app::Answered;
use straum_domain::{Ask, AskKey, Datum, Value};

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

pub(crate) fn write(ask: &Ask, answered: &Answered) -> Vec<u8> {
    let Answered { answer, lsn, time } = answered;
    let mut out = Vec::new();
    out.extend_from_slice(b"{\"lsn\":");
    string(&mut out, &lsn.to_string());
    out.extend_from_slice(b",\"commit_time\":");
    match time.map(Timestamp::try_from) {
        Some(Ok(at)) => string(&mut out, &at.to_string()),
        // Past jiff's year 9999, which only a broken clock gives.
        Some(Err(_)) | None => out.extend_from_slice(b"null"),
    }
    out.extend_from_slice(b",\"columns\":[");
    let columns = (ask.by.iter().map(|key| match key {
        AskKey::Time(t) => t.to_string(),
        AskKey::Key(path) => path.to_string(),
    }))
    .chain(ask.measures.iter().map(ToString::to_string));
    for (i, column) in columns.enumerate() {
        if i > 0 {
            out.push(b',');
        }
        string(&mut out, &column);
    }
    out.extend_from_slice(b"],\"rows\":[");
    let (key_scales, measure_scales) = answer.scales.split_at(ask.by.len());
    for (i, (keys, values)) in answer.rows.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.push(b'[');
        let mut first = true;
        let mut comma = |out: &mut Vec<u8>| {
            if !mem::take(&mut first) {
                out.push(b',');
            }
        };
        for ((datum, key), &scale) in keys.iter().zip(&ask.by).zip(key_scales) {
            comma(&mut out);
            match (key, datum) {
                (AskKey::Time(_), &Datum::Int(seconds)) => {
                    let at = Timestamp::from_second(seconds)
                        .expect("range_errors keeps time keys in range");
                    string(&mut out, &at.to_string());
                }
                (_, datum) => write_datum(&mut out, datum, scale),
            }
        }
        for (value, &scale) in values.iter().zip(measure_scales) {
            comma(&mut out);
            write_value(&mut out, value, scale);
        }
        out.push(b']');
    }
    out.extend_from_slice(b"]}");
    out
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
            out.extend_from_slice(b"{\"sum\":");
            decimal(out, *sum, scale);
            out.extend_from_slice(b",\"count\":");
            out.extend_from_slice(count.to_string().as_bytes());
            out.push(b'}');
        }
    }
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
            r#"null true false -1.5 "a\"b" null 0.07 {"sum":1.5,"count":4} "#
        );
    }
}
