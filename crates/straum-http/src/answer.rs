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

use std::fmt::Write as _;
use std::time::SystemTime;

use jiff::Timestamp;
use straum_app::Answered;
use straum_domain::{AnswerColumn, AnswerRow, Ask, Datum, Value, decimal};

use crate::json::quoted;
use crate::time::{rfc3339, rfc3339_of};

/// How far an ask's range stays clear of the earliest time jiff can write,
/// so a time key that starts up to a month before it still prints.
const LOOK_BACK: i64 = 32 * 86_400;

/// A time key starts in its ask's range or at most `LOOK_BACK` before it,
/// so an ask that passes here never fails `write`, whoever read it.
pub(crate) fn range_errors(ask: &Ask) -> Vec<String> {
    let earliest = Timestamp::MIN.as_second() + LOOK_BACK;
    let latest = Timestamp::MAX.as_second();
    let mut errors = Vec::new();
    for (end, at) in [("start", ask.from), ("end", ask.to)] {
        if !(earliest..=latest).contains(&at) {
            errors.push(format!(
                "range {end} {at} is not between {earliest} and {latest}"
            ));
        }
    }
    errors
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
    for (row_index, AnswerRow { keys, values }) in answer.rows.iter().enumerate() {
        if row_index > 0 {
            out.push(',');
        }
        let (key_columns, measure_columns) = answer.columns.split_at(keys.len());
        let keys = (key_columns.iter().zip(keys)).map(|(column, key)| key_json(column, key));
        let values = (measure_columns.iter().zip(values))
            .map(|(column, value)| value_json(value, column.scale));
        out.push('{');
        for (cell_index, (name, cell)) in names.iter().zip(keys.chain(values)).enumerate() {
            if cell_index > 0 {
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

fn key_json(column: &AnswerColumn, key: &Datum) -> String {
    match key {
        &Datum::Int(seconds) if column.is_time => quoted(&time_key(seconds)),
        key => datum_json(key, column.scale),
    }
}

fn as_of_text(as_of: SystemTime) -> String {
    // Past jiff's year 9999, which only a broken clock gives; 1970 is
    // still a true lower bound.
    rfc3339_of(as_of).unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned())
}

fn time_key(seconds: i64) -> String {
    rfc3339(seconds).expect("range_errors keeps time keys in range")
}

/// Numbers line up on the right, as their digits do.
#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
}

struct Cell {
    text: String,
    align: Align,
}

/// The answer as a table: the names, a rule, a line per row, then the
/// row count and `as_of`. Values print as in the JSON, text without its
/// quotes. Past `limit` rows the count says how many there are.
pub(crate) fn table(answered: &Answered, limit: Option<usize>) -> String {
    let Answered { answer, as_of, .. } = answered;
    let shown = limit.map_or(answer.rows.len(), |limit| limit.min(answer.rows.len()));
    let mut lines = vec![name_cells(&answer.columns)];
    for row in &answer.rows[..shown] {
        lines.push(row_cells(&answer.columns, row));
    }
    let widths = column_widths(&lines);

    let mut out = String::new();
    for (line_index, line) in lines.iter().enumerate() {
        out.push_str(&table_line(line, &widths));
        if line_index == 0 {
            out.push_str(&rule(&widths));
        }
    }
    let rows = row_count(answer.rows.len(), shown);
    let _ = writeln!(out, "{rows}, as of {}", as_of_text(*as_of));
    out
}

fn name_cells(columns: &[AnswerColumn]) -> Vec<Cell> {
    columns.iter().map(name_cell).collect()
}

fn name_cell(column: &AnswerColumn) -> Cell {
    Cell {
        text: column.name.clone(),
        align: Align::Left,
    }
}

fn row_cells(columns: &[AnswerColumn], row: &AnswerRow) -> Vec<Cell> {
    let (key_columns, measure_columns) = columns.split_at(row.keys.len());
    let keys = (key_columns.iter().zip(&row.keys)).map(|(column, key)| key_cell(column, key));
    let values =
        (measure_columns.iter().zip(&row.values)).map(|(column, value)| value_cell(column, value));
    keys.chain(values).collect()
}

fn key_cell(column: &AnswerColumn, key: &Datum) -> Cell {
    let text = match key {
        &Datum::Int(seconds) if column.is_time => time_key(seconds),
        Datum::Text(text) => text.to_string(),
        key => datum_json(key, column.scale),
    };
    let align = match key {
        Datum::Int(_) if !column.is_time => Align::Right,
        _ => Align::Left,
    };
    Cell { text, align }
}

fn value_cell(column: &AnswerColumn, value: &Value) -> Cell {
    let align = match value {
        Value::Int(_) | Value::Avg { .. } => Align::Right,
        Value::Null | Value::Bool(_) => Align::Left,
    };
    Cell {
        text: value_json(value, column.scale),
        align,
    }
}

/// The widest cell of each column. The first line holds the names, so
/// it has every column.
fn column_widths(lines: &[Vec<Cell>]) -> Vec<usize> {
    let mut widths = vec![0; lines[0].len()];
    for line in lines {
        for (width, cell) in widths.iter_mut().zip(line) {
            *width = (*width).max(cell.text.chars().count());
        }
    }
    widths
}

fn table_line(line: &[Cell], widths: &[usize]) -> String {
    let cells: Vec<String> = (line.iter().zip(widths))
        .map(|(cell, &width)| padded(cell, width))
        .collect();
    format!("{}\n", cells.join(" | ").trim_end())
}

fn padded(cell: &Cell, width: usize) -> String {
    let text = &cell.text;
    match cell.align {
        Align::Left => format!("{text:<width$}"),
        Align::Right => format!("{text:>width$}"),
    }
}

fn rule(widths: &[usize]) -> String {
    let dashes: Vec<String> = widths.iter().map(|&width| "-".repeat(width)).collect();
    format!("{}\n", dashes.join("-+-"))
}

fn row_count(rows: usize, shown: usize) -> String {
    match rows {
        1 => "1 row".to_owned(),
        rows if rows > shown => format!("{shown} of {rows} rows"),
        rows => format!("{rows} rows"),
    }
}

fn datum_json(datum: &Datum, scale: u32) -> String {
    match datum {
        Datum::Null => "null".to_owned(),
        Datum::Bool(boolean) => boolean.to_string(),
        Datum::Int(integer) => decimal(i128::from(*integer), scale),
        Datum::Text(text) => quoted(text),
    }
}

fn value_json(value: &Value, scale: u32) -> String {
    match value {
        Value::Null => "null".to_owned(),
        Value::Int(integer) => decimal(*integer, scale),
        Value::Bool(boolean) => boolean.to_string(),
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
    let sum = sum.unsigned_abs();
    let count = u128::from(count);
    let shift = 10u128.pow(AVG_PLACES);
    let tail = sum % count * shift;
    let mut units = sum / count * shift + tail / count;
    // Doubled, what is left over compares with half of count without
    // dividing, which would round the half away.
    let twice_rest = tail % count * 2;
    if twice_rest > count || (twice_rest == count && units % 2 == 1) {
        units += 1;
    }
    let units = i128::try_from(units).expect("an average of i64s");
    if negative { -units } else { units }
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use straum_app::Lsn;
    use straum_domain::Answer;

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

    /// A row in the second day of 1970.
    fn row(shared: Datum, fare: Value) -> AnswerRow {
        AnswerRow {
            keys: vec![Datum::Int(86_400), shared],
            values: vec![fare],
        }
    }

    #[test]
    fn a_table_lines_numbers_up_on_the_right() {
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
        let rows = vec![
            row(Datum::Null, Value::Bool(true)),
            row(Datum::Null, Value::Int(123_456)),
        ];
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
    fn an_as_of_past_jiff_is_1970() {
        let answered = Answered {
            as_of: UNIX_EPOCH + std::time::Duration::from_secs(400_000_000_000),
            ..answered(Vec::new())
        };
        let json = String::from_utf8(write(&answered)).unwrap();
        assert_eq!(json, r#"{"as_of":"1970-01-01T00:00:00Z","rows":[]}"#);
        let text = table(&answered, None);
        assert!(
            text.ends_with("0 rows, as of 1970-01-01T00:00:00Z\n"),
            "{text}"
        );
    }

    #[test]
    #[should_panic(expected = "range_errors keeps time keys in range")]
    fn a_time_key_past_jiff_is_a_bug() {
        time_key(i64::MAX);
    }

    fn written(sum: i128, count: u64, scale: u32) -> String {
        value_json(&Value::Avg { sum, count }, scale)
    }

    #[test]
    fn rounds_avg_half_to_even() {
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
