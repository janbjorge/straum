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
//!
//! For people, the same answer as a table in plain text:
//!
//! ```text
//! bucket               | pickup.borough | trips |    fare
//! ---------------------+----------------+-------+--------
//! 2024-01-01T00:00:00Z | Bronx          |   412 | 6234.50
//! 1 row, as of 2024-01-31T22:59:59.123456Z
//! ```

use std::time::SystemTime;

use jiff::Timestamp;
use straum_app::Answered;
use straum_domain::{AnswerRow, Ask, Datum, Value, decimal};

use crate::json::quoted;

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
    let names: Vec<String> = (answer.columns.iter())
        .map(|column| quoted(&column.name))
        .collect();
    let mut out = format!("{{\"as_of\":{},\"rows\":[", quoted(&as_of_text(*as_of)));
    for (i, AnswerRow { keys, values }) in answer.rows.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let (key_columns, measure_columns) = answer.columns.split_at(keys.len());
        let keys = key_columns.iter().zip(keys).map(|(column, key)| match key {
            &Datum::Int(seconds) if column.is_time => quoted(&time_key(seconds)),
            key => datum_json(key, column.scale),
        });
        let values = (measure_columns.iter().zip(values))
            .map(|(column, value)| value_json(value, column.scale));
        out.push('{');
        for (j, (name, cell)) in names.iter().zip(keys.chain(values)).enumerate() {
            if j > 0 {
                out.push(',');
            }
            out.push_str(name);
            out.push(':');
            out.push_str(&cell);
        }
        out.push('}');
    }
    out.push_str("]}");
    out.into_bytes()
}

fn as_of_text(as_of: SystemTime) -> String {
    // Past jiff's year 9999, which only a broken clock gives; 1970 is
    // still a true lower bound.
    (Timestamp::try_from(as_of).unwrap_or(Timestamp::UNIX_EPOCH)).to_string()
}

fn time_key(seconds: i64) -> String {
    Timestamp::from_second(seconds)
        .expect("range_errors keeps time keys in range")
        .to_string()
}

struct Cell {
    text: String,
    /// Numbers line up on the right, as their digits do.
    number: bool,
}

/// The answer as a table: the names, a rule, a line per row, then the
/// row count and `as_of`. Values print as in the JSON, text without its
/// quotes. Past `limit` rows the count says how many there are.
pub(crate) fn table(answered: &Answered, limit: Option<usize>) -> String {
    let Answered { answer, as_of, .. } = answered;
    let names = (answer.columns.iter())
        .map(|column| Cell {
            text: column.name.clone(),
            number: false,
        })
        .collect();
    let shown = limit.map_or(answer.rows.len(), |limit| limit.min(answer.rows.len()));
    let rows = answer.rows[..shown]
        .iter()
        .map(|AnswerRow { keys, values }| {
            let (key_columns, measure_columns) = answer.columns.split_at(keys.len());
            let keys = key_columns.iter().zip(keys).map(|(column, datum)| {
                let text = match datum {
                    &Datum::Int(seconds) if column.is_time => time_key(seconds),
                    Datum::Text(text) => text.to_string(),
                    datum => datum_json(datum, column.scale),
                };
                let number = matches!(datum, Datum::Int(_)) && !column.is_time;
                Cell { text, number }
            });
            let values = measure_columns
                .iter()
                .zip(values)
                .map(|(column, value)| Cell {
                    text: value_json(value, column.scale),
                    number: matches!(value, Value::Int(_) | Value::Avg { .. }),
                });
            keys.chain(values).collect()
        });
    let lines: Vec<Vec<Cell>> = std::iter::once(names).chain(rows).collect();
    let widths: Vec<usize> = (0..answer.columns.len())
        .map(|i| lines.iter().map(|line| line[i].text.chars().count()).max())
        .map(|widest| widest.expect("the names are a line"))
        .collect();

    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        let cells: Vec<String> = (line.iter().zip(&widths))
            .map(|(Cell { text, number }, &width)| match number {
                true => format!("{text:>width$}"),
                false => format!("{text:<width$}"),
            })
            .collect();
        out.push_str(cells.join(" | ").trim_end());
        out.push('\n');
        if i == 0 {
            let rule: Vec<String> = widths.iter().map(|&width| "-".repeat(width)).collect();
            out.push_str(&rule.join("-+-"));
            out.push('\n');
        }
    }
    let rows = match answer.rows.len() {
        1 => "1 row".to_owned(),
        rows if rows > shown => format!("{shown} of {rows} rows"),
        rows => format!("{rows} rows"),
    };
    out.push_str(&format!("{rows}, as of {}\n", as_of_text(*as_of)));
    out
}

fn datum_json(datum: &Datum, scale: u32) -> String {
    match datum {
        Datum::Null => "null".to_owned(),
        Datum::Bool(b) => b.to_string(),
        Datum::Int(n) => decimal(i128::from(*n), scale),
        Datum::Text(text) => quoted(text),
    }
}

fn value_json(value: &Value, scale: u32) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Int(n) => decimal(*n, scale),
        Value::Bool(b) => b.to_string(),
        Value::Avg { sum, count } => {
            let digits = decimal(avg(*sum, *count), scale + AVG_PLACES);
            // The places past the column's scale are only as long as they
            // need to be. AVG_PLACES puts a point in every average.
            let digits = digits.trim_end_matches('0');
            digits.strip_suffix('.').unwrap_or(digits).to_owned()
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
    use std::time::UNIX_EPOCH;

    use straum_app::Lsn;
    use straum_domain::{Answer, AnswerColumn};

    use super::*;

    #[test]
    fn writes_values() {
        let mut out = String::new();
        for (datum, scale) in [
            (Datum::Null, 0),
            (Datum::Bool(true), 0),
            (Datum::Bool(false), 0),
            (Datum::Int(-15), 1),
            (Datum::Text("a\"b".into()), 0),
        ] {
            out.push_str(&datum_json(&datum, scale));
            out.push(' ');
        }
        for (value, scale) in [
            (Value::Null, 2),
            (Value::Int(7), 2),
            (Value::Avg { sum: 15, count: 4 }, 1),
            (Value::Bool(true), 0),
            (Value::Bool(false), 0),
        ] {
            out.push_str(&value_json(&value, scale));
            out.push(' ');
        }
        assert_eq!(
            out,
            r#"null true false -1.5 "a\"b" null 0.07 0.375 true false "#
        );
    }

    fn column(name: &str, scale: u32, is_time: bool) -> AnswerColumn {
        AnswerColumn {
            name: name.to_owned(),
            scale,
            is_time,
        }
    }

    fn answered(rows: Vec<AnswerRow>) -> Answered {
        Answered {
            answer: Answer {
                columns: vec![
                    column("bucket", 0, true),
                    column("shared", 0, false),
                    column("fare", 2, false),
                ],
                rows,
            },
            applied: Lsn(0),
            as_of: UNIX_EPOCH,
        }
    }

    #[test]
    fn a_table_lines_numbers_up_on_the_right() {
        let row = |shared, fare| AnswerRow {
            keys: vec![Datum::Int(86_400), shared],
            values: vec![fare],
        };
        let rows = vec![
            row(Datum::Bool(true), Value::Int(123_456)),
            row(Datum::Null, Value::Null),
        ];
        assert_eq!(
            table(&answered(rows.clone()), Some(2)),
            "\
bucket               | shared | fare
---------------------+--------+--------
1970-01-02T00:00:00Z | true   | 1234.56
1970-01-02T00:00:00Z | null   | null
2 rows, as of 1970-01-01T00:00:00Z
"
        );
        assert_eq!(
            table(&answered(rows), Some(1)),
            "\
bucket               | shared | fare
---------------------+--------+--------
1970-01-02T00:00:00Z | true   | 1234.56
1 of 2 rows, as of 1970-01-01T00:00:00Z
"
        );
    }

    #[test]
    fn a_table_lines_a_boolean_measure_up_on_the_left() {
        let row = |fare| AnswerRow {
            keys: vec![Datum::Int(86_400), Datum::Null],
            values: vec![fare],
        };
        let rows = vec![row(Value::Bool(true)), row(Value::Int(123_456))];
        assert_eq!(
            table(&answered(rows), None),
            "\
bucket               | shared | fare
---------------------+--------+--------
1970-01-02T00:00:00Z | null   | true
1970-01-02T00:00:00Z | null   | 1234.56
2 rows, as of 1970-01-01T00:00:00Z
"
        );
    }

    #[test]
    fn a_table_without_rows_keeps_its_names() {
        assert_eq!(
            table(&answered(Vec::new()), None),
            "\
bucket | shared | fare
-------+--------+-----
0 rows, as of 1970-01-01T00:00:00Z
"
        );
    }

    #[test]
    fn rounds_avg_half_to_even() {
        let written = |sum, count, scale| value_json(&Value::Avg { sum, count }, scale);
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
