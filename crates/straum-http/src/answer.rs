//! Writes an answer from the app for a client: `write` as JSON for
//! programs, `table` as plain text for people. `routes.rs` calls
//! `range_errors` on the bounds an ask writes before asking the app, then
//! calls one of the two writers on the app's answer.
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
//!   exact decimals in their column's scale; avg is rounded half to even
//!   at four places past it, with trailing zeros dropped.
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

use std::fmt::{self, Write as _};
use std::time::SystemTime;

use jiff::Timestamp;
use straum_app::Answered;
use straum_domain::{AnswerColumn, AnswerRow, Ask, Datum, Value, decimal, push_decimal};

use crate::json::quoted;
use crate::time::{rfc3339, rfc3339_of};

/// How far an ask's range stays clear of the earliest time jiff can write,
/// so a time key that starts up to a month before it still prints.
const LOOK_BACK_SECONDS: i64 = 32 * 86_400;

/// Places past a column's scale that avg keeps.
const AVG_PLACES: u32 = 4;

/// Which end of an ask's range.
#[derive(Debug, Clone, Copy)]
enum RangeEnd {
    Start,
    End,
}

/// An end of an ask's range that jiff could not write as a time.
#[derive(Debug)]
pub(crate) struct RangeError {
    end: RangeEnd,
    at_seconds: i64,
}

impl fmt::Display for RangeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let end = match self.end {
            RangeEnd::Start => "start",
            RangeEnd::End => "end",
        };
        write!(
            formatter,
            "range {end} {} is not between {} and {}",
            self.at_seconds,
            earliest_seconds(),
            latest_seconds()
        )
    }
}

fn earliest_seconds() -> i64 {
    Timestamp::MIN.as_second() + LOOK_BACK_SECONDS
}

fn latest_seconds() -> i64 {
    Timestamp::MAX.as_second()
}

/// The bounds an ask writes that jiff could not write as a time; an open
/// end has none to check.
pub(crate) fn range_errors(ask: &Ask) -> Vec<RangeError> {
    let writable = earliest_seconds()..=latest_seconds();
    let mut errors = Vec::new();
    for (end, bound) in [(RangeEnd::Start, ask.from), (RangeEnd::End, ask.to)] {
        let Some(at_seconds) = bound else {
            continue;
        };
        if !writable.contains(&at_seconds) {
            errors.push(RangeError { end, at_seconds });
        }
    }
    errors
}

/// The most bytes the JSON writer reserves ahead of the rows: past it the
/// body grows by doubling.
const MAX_RESERVE: usize = 64 << 20;

/// The answer as a JSON body; the LSN is the caller's to send.
pub(crate) fn write(answered: &Answered) -> Vec<u8> {
    let Answered { answer, as_of, .. } = answered;
    let mut names = Vec::with_capacity(answer.columns.len());
    for column in &answer.columns {
        names.push(quoted(&column.name));
    }
    let mut out = format!("{{\"as_of\":{},\"rows\":[", quoted(&as_of_text(*as_of)));
    let mut last_time = LastTime::default();
    for (row_index, row) in answer.rows.iter().enumerate() {
        if row_index > 0 {
            out.push(',');
        }
        let start = out.len();
        push_row_json(&mut out, &names, &answer.columns, row, &mut last_time);
        if row_index == 0 {
            // The rows to come, at about the first one's length: growing
            // by doubling copies the answer written so far each time. At
            // most `MAX_RESERVE`: one long first row is no measure of the
            // rest, and a 200 MB key times a million rows aborted.
            let rows_to_come = (out.len() - start + 1).saturating_mul(answer.rows.len() - 1);
            out.reserve(rows_to_come.min(MAX_RESERVE));
        }
    }
    out.push_str("]}");
    out.into_bytes()
}

/// The time key written last, quoted: the rows of a bucket follow one
/// another, so most rows write the time the one before did.
#[derive(Default)]
struct LastTime {
    seconds: Option<i64>,
    quoted: String,
}

/// One row as an object: the keys first, then the measures, each under
/// its column's quoted name.
fn push_row_json(
    out: &mut String,
    names: &[String],
    columns: &[AnswerColumn],
    row: &AnswerRow,
    last_time: &mut LastTime,
) {
    out.push('{');
    for (index, key) in row.keys.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        push_name(out, &names[index]);
        push_key_json(out, &columns[index], key, last_time);
    }
    let key_count = row.keys.len();
    for (offset, value) in row.values.iter().enumerate() {
        let index = key_count + offset;
        if index > 0 {
            out.push(',');
        }
        push_name(out, &names[index]);
        push_value_json(out, value, columns[index].scale);
    }
    out.push('}');
}

fn push_name(out: &mut String, name: &str) {
    out.push_str(name);
    out.push(':');
}

fn push_key_json(out: &mut String, column: &AnswerColumn, key: &Datum, last_time: &mut LastTime) {
    match key {
        &Datum::Int(seconds) if column.is_time => {
            if last_time.seconds != Some(seconds) {
                last_time.quoted = quoted(&time_key(seconds));
                last_time.seconds = Some(seconds);
            }
            out.push_str(&last_time.quoted);
        }
        Datum::Int(integer) => push_decimal(out, i128::from(*integer), column.scale),
        key => out.push_str(&datum_json(key, column.scale)),
    }
}

fn as_of_text(as_of: SystemTime) -> String {
    match rfc3339_of(as_of) {
        Some(text) => text,
        // Past jiff's year 9999, which only a broken clock gives; 1970 is
        // still a true lower bound.
        None => "1970-01-01T00:00:00Z".to_owned(),
    }
}

fn time_key(seconds: i64) -> String {
    match rfc3339(seconds) {
        Some(text) => text,
        // PostgreSQL keeps times past jiff's year 9999, and an open end
        // reads them.
        None => format!("Unix second {seconds}"),
    }
}

/// Numbers line up on the right, as their digits do.
#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
}

/// One cell of the text table, before it is padded to its column.
struct Cell {
    text: String,
    align: Align,
}

/// Which rows of an answer a table shows: at most `count` from `start`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Window {
    pub(crate) start: Start,
    pub(crate) count: usize,
}

/// Where a window starts, as a Python slice does: a row counted from the
/// first, which is 0, or back from the end, the last row being 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Start {
    FromFirst(usize),
    FromLast(usize),
}

/// The rows of `total` a window shows, from `first` up to `end`. A start
/// past either end is held at it, as a slice is, so no window fails.
struct Shown {
    first: usize,
    end: usize,
}

fn shown(window: Option<Window>, total: usize) -> Shown {
    let Some(Window { start, count }) = window else {
        return Shown {
            first: 0,
            end: total,
        };
    };
    let first = match start {
        Start::FromFirst(row) => row.min(total),
        Start::FromLast(rows) => total.saturating_sub(rows),
    };
    let end = first.saturating_add(count).min(total);
    Shown { first, end }
}

/// The answer as a table: the names, a rule, a line per row, then the
/// row count and `as_of`. Values print as in the JSON, text without its
/// quotes. With a window only its rows show, and the count says how many
/// there are and from which row.
pub(crate) fn table(answered: &Answered, window: Option<Window>) -> String {
    let Answered { answer, as_of, .. } = answered;
    let total = answer.rows.len();
    let Shown { first, end } = shown(window, total);
    let mut lines = vec![name_cells(&answer.columns)];
    for row in &answer.rows[first..end] {
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
    let rows = row_count(total, Shown { first, end });
    let _ = writeln!(out, "{rows}, as of {}", as_of_text(*as_of));
    out
}

fn name_cells(columns: &[AnswerColumn]) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(columns.len());
    for column in columns {
        cells.push(Cell {
            text: column.name.clone(),
            align: Align::Left,
        });
    }
    cells
}

fn row_cells(columns: &[AnswerColumn], row: &AnswerRow) -> Vec<Cell> {
    let mut cells = Vec::with_capacity(columns.len());
    for (index, key) in row.keys.iter().enumerate() {
        cells.push(key_cell(&columns[index], key));
    }
    let key_count = row.keys.len();
    for (offset, value) in row.values.iter().enumerate() {
        cells.push(value_cell(&columns[key_count + offset], value));
    }
    cells
}

fn key_cell(column: &AnswerColumn, key: &Datum) -> Cell {
    let text = match key {
        &Datum::Int(seconds) if column.is_time => time_key(seconds),
        Datum::Text(text) => escaped(text),
        key => datum_json(key, column.scale),
    };
    let align = match key {
        Datum::Int(_) if !column.is_time => Align::Right,
        _ => Align::Left,
    };
    Cell { text, align }
}

/// What a text `null` shows as, so it differs from a null: a backslash in
/// text shows as two, so no other text shows as this.
const NULL_TEXT: &str = "\\null";

/// Text as a cell shows it: a backslash and each control character as
/// JSON writes them, `\\`, `\n` or `\u001b`, so data can't break a line
/// of the table or reach the terminal as an escape sequence; the text
/// `null` as [`NULL_TEXT`].
fn escaped(text: &str) -> String {
    if text == "null" {
        return NULL_TEXT.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if control.is_control() => {
                let _ = write!(out, "\\u{:04x}", u32::from(control));
            }
            other => out.push(other),
        }
    }
    out
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

/// The widest cell of each column, in characters. The first line holds
/// the names, so it has every column.
fn column_widths(lines: &[Vec<Cell>]) -> Vec<usize> {
    let mut widths = vec![0; lines[0].len()];
    for line in lines {
        for (index, cell) in line.iter().enumerate() {
            widths[index] = widths[index].max(cell.text.chars().count());
        }
    }
    widths
}

fn table_line(line: &[Cell], widths: &[usize]) -> String {
    let mut cells = Vec::with_capacity(line.len());
    for (index, cell) in line.iter().enumerate() {
        cells.push(padded(cell, widths[index]));
    }
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
    let mut dashes = Vec::with_capacity(widths.len());
    for width in widths {
        dashes.push("-".repeat(*width));
    }
    format!("{}\n", dashes.join("-+-"))
}

fn row_count(total: usize, Shown { first, end }: Shown) -> String {
    let noun = if total == 1 { "row" } else { "rows" };
    let count = end - first;
    if count == total {
        return format!("{total} {noun}");
    }
    if first == 0 {
        return format!("{count} of {total} {noun}");
    }
    format!("{count} of {total} {noun} from row {first}")
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
    let mut text = String::new();
    push_value_json(&mut text, value, scale);
    text
}

fn push_value_json(out: &mut String, value: &Value, scale: u32) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Int(integer) => push_decimal(out, *integer, scale),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Avg { sum, count } => {
            push_decimal(out, avg(*sum, *count), scale + AVG_PLACES);
            // The places past the column's scale are only as long as they
            // need to be. AVG_PLACES puts a point in every average, so
            // the zeros end at it.
            while out.ends_with('0') {
                out.pop();
            }
            if out.ends_with('.') {
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
    use std::time::{Duration, UNIX_EPOCH};

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

    /// A long first row reserves no more than [`MAX_RESERVE`] for the
    /// rest: one key of a megabyte before 100,000 short rows would have
    /// reserved 100 GB.
    #[test]
    fn a_long_first_row_reserves_a_bounded_amount() {
        let long = "x".repeat(1 << 20);
        let mut rows = vec![row(Datum::Text(long.as_str().into()), Value::Int(1))];
        for _ in 0..100_000 {
            rows.push(row(Datum::Bool(true), Value::Int(1)));
        }
        let body = write(&answered(rows));
        assert!(body.len() > long.len() && body.capacity() < 2 * MAX_RESERVE);
        let short = write(&answered(vec![row(Datum::Bool(true), Value::Int(1))]));
        assert!(short.ends_with(b"]}"));
    }

    /// A row in the second day of 1970.
    fn row(shared: Datum, fare: Value) -> AnswerRow {
        AnswerRow {
            keys: vec![Datum::Int(86_400), shared],
            values: vec![fare],
        }
    }

    #[test]
    fn writes_rows_as_json() {
        let rows = vec![
            row(Datum::Bool(true), Value::Int(123_456)),
            row(Datum::Null, Value::Null),
        ];
        let json = String::from_utf8(write(&answered(rows))).unwrap();
        assert_eq!(
            json,
            concat!(
                r#"{"as_of":"1970-01-01T00:00:00Z","rows":["#,
                r#"{"bucket":"1970-01-02T00:00:00Z","shared":true,"fare":1234.56},"#,
                r#"{"bucket":"1970-01-02T00:00:00Z","shared":null,"fare":null}]}"#
            )
        );
    }

    #[test]
    fn a_table_lines_numbers_up_on_the_right() {
        let rows = vec![
            row(Datum::Bool(true), Value::Int(123_456)),
            row(Datum::Null, Value::Null),
        ];
        assert_eq!(
            table(&answered(rows.clone()), Some(first(2))),
            "\
bucket               | shared | fare
---------------------+--------+--------
1970-01-02T00:00:00Z | true   | 1234.56
1970-01-02T00:00:00Z | null   | null
2 rows, as of 1970-01-01T00:00:00Z
"
        );
        assert_eq!(
            table(&answered(rows), Some(first(1))),
            "\
bucket               | shared | fare
---------------------+--------+--------
1970-01-02T00:00:00Z | true   | 1234.56
1 of 2 rows, as of 1970-01-01T00:00:00Z
"
        );
    }

    /// Text from the data can't forge a row of the table or reach the
    /// terminal as an escape: a backslash and control characters show as
    /// JSON writes them, and other text as it is.
    #[test]
    fn a_table_escapes_control_characters_in_text() {
        let forged = "Bronx\n1970-01-02T00:00:00Z | true | 1.00\r\t\u{1b}[31m\\ ø";
        let rows = vec![row(Datum::Text(forged.into()), Value::Int(100))];
        let text = table(&answered(rows), Some(first(1)));
        assert_eq!(text.lines().count(), 4, "{text}");
        assert!(
            text.contains(r"Bronx\n1970-01-02T00:00:00Z | true | 1.00\r\t\u001b[31m\\ ø"),
            "{text}"
        );
        assert_eq!(escaped("plain | text"), "plain | text");
    }

    /// A null prints as `null`, and the text `null` otherwise, so the two
    /// differ; text that only holds `null` prints as written.
    #[test]
    fn a_table_tells_a_null_from_the_text_null() {
        let rows = vec![
            row(Datum::Null, Value::Int(1)),
            row(Datum::Text("null".into()), Value::Int(2)),
            row(Datum::Text("nullable".into()), Value::Int(3)),
            row(Datum::Text("\\null".into()), Value::Int(4)),
        ];
        let text = table(&answered(rows), Some(first(4)));
        let mut keys = Vec::new();
        for line in text.lines().skip(2).take(4) {
            keys.push(line.split(" | ").nth(1).unwrap().trim_end().to_owned());
        }
        assert_eq!(keys, ["null", r"\null", "nullable", r"\\null"], "{text}");
    }

    fn first(count: usize) -> Window {
        Window {
            start: Start::FromFirst(0),
            count,
        }
    }

    /// The bucket each row of a table shows, over five rows whose fares
    /// are 0 to 4, and its count.
    fn fares_shown(start: Start, count: usize) -> (Vec<String>, String) {
        let mut rows = Vec::new();
        for fare in 0..5 {
            rows.push(row(Datum::Null, Value::Int(fare * 100)));
        }
        let text = table(&answered(rows), Some(Window { start, count }));
        let mut lines: Vec<&str> = text.lines().skip(2).collect();
        let count = lines.pop().unwrap();
        let (count, _) = count.split_once(", as of").unwrap();
        let mut fares = Vec::new();
        for line in lines {
            fares.push(line.rsplit(' ').next().unwrap().to_owned());
        }
        (fares, count.to_owned())
    }

    fn fares(fares: &[&str], count: &str) -> (Vec<String>, String) {
        let mut owned = Vec::new();
        for fare in fares {
            owned.push((*fare).to_owned());
        }
        (owned, count.to_owned())
    }

    #[test]
    fn a_window_shows_its_rows_and_says_from_which() {
        assert_eq!(
            fares_shown(Start::FromFirst(1), 2),
            fares(&["1.00", "2.00"], "2 of 5 rows from row 1")
        );
        assert_eq!(
            fares_shown(Start::FromLast(2), 1000),
            fares(&["3.00", "4.00"], "2 of 5 rows from row 3")
        );
        assert_eq!(
            fares_shown(Start::FromLast(3), 1),
            fares(&["2.00"], "1 of 5 rows from row 2")
        );
        assert_eq!(
            fares_shown(Start::FromFirst(0), 5),
            fares(&["0.00", "1.00", "2.00", "3.00", "4.00"], "5 rows")
        );
    }

    #[test]
    fn a_window_past_either_end_is_held_at_it() {
        assert_eq!(
            fares_shown(Start::FromFirst(7), 2),
            fares(&[], "0 of 5 rows from row 5")
        );
        assert_eq!(
            fares_shown(Start::FromLast(9), 2),
            fares(&["0.00", "1.00"], "2 of 5 rows")
        );
        assert_eq!(
            fares_shown(Start::FromFirst(usize::MAX), usize::MAX),
            fares(&[], "0 of 5 rows from row 5")
        );
        assert_eq!(
            fares_shown(Start::FromFirst(2), 0),
            fares(&[], "0 of 5 rows from row 2")
        );
    }

    #[test]
    fn a_count_of_one_row_says_row() {
        let rows = vec![row(Datum::Null, Value::Null)];
        let text = table(&answered(rows), Some(first(0)));
        assert!(
            text.ends_with("0 of 1 row, as of 1970-01-01T00:00:00Z\n"),
            "{text}"
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
            as_of: UNIX_EPOCH + Duration::from_secs(400_000_000_000),
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

    fn ask_over(from: Option<i64>, to: Option<i64>) -> Ask {
        Ask {
            view: straum_domain::Identifier::new("trips").unwrap(),
            measures: Vec::new(),
            keys: Vec::new(),
            filter: Vec::new(),
            having: Vec::new(),
            from,
            to,
        }
    }

    #[test]
    fn a_range_jiff_can_write_has_no_errors() {
        assert!(range_errors(&ask_over(Some(0), Some(3_600))).is_empty());
        let errors = range_errors(&ask_over(Some(i64::MIN), Some(i64::MAX)));
        let mut lines = Vec::new();
        for error in &errors {
            lines.push(error.to_string());
        }
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("range start -9223372036854775808 is not between "));
        assert!(lines[1].starts_with("range end 9223372036854775807 is not between "));
    }

    #[test]
    fn an_open_end_has_no_range_error() {
        assert!(range_errors(&ask_over(None, None)).is_empty());
        let errors = range_errors(&ask_over(None, Some(i64::MAX)));
        assert_eq!(errors.len(), 1);
        assert!(errors[0].to_string().starts_with("range end "));
    }

    #[test]
    fn a_time_key_past_jiff_is_written_in_unix_seconds() {
        assert_eq!(time_key(0), "1970-01-01T00:00:00Z");
        assert_eq!(time_key(i64::MAX), "Unix second 9223372036854775807");
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
