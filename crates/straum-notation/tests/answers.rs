//! Asks read end to end: a spec and an ask parsed from their text, then
//! answered by a store that holds a few rows, as PostgreSQL would answer
//! the same query over them: bounds with an offset, and a FILTER on every
//! measure, which keeps the groups a WHERE would drop.

mod common;

use common::{JAN, described};
use straum_domain::{AnswerRow, AskError, ColumnType, Datum, Literal, Store, Value};
use straum_notation::{parse, parse_ask};

/// Seconds in an hour.
const HOUR: i64 = 3_600;

/// One fact `t`, its time `at` in hour buckets, counted.
const COUNTED: &str = "CREATE VIEW t AS
SELECT buckets(f.at, '1 hour') AS bucket, count(*) AS n
FROM public.t AS f
GROUP BY 1";

/// The view's time, as the load reads its column.
#[derive(Clone, Copy)]
enum TimeColumn {
    Timestamp,
    Timestamptz,
}

/// A store over `COUNTED` whose time is `time`, holding one row at
/// `at_seconds`.
fn one_row(time: TimeColumn, at_seconds: i64) -> Store {
    let zoned = match time {
        TimeColumn::Timestamp => false,
        TimeColumn::Timestamptz => true,
    };
    let mut store = Store::new(parse(COUNTED).unwrap());
    store.set_types("t", &[ColumnType::Time { zoned }]).unwrap();
    store.add_row("t", &[Datum::Int(at_seconds)]).unwrap();
    store
}

/// The count over `[from, to)`, both RFC 3339 text.
fn count(store: &Store, from: &str, to: &str) -> Result<Value, Vec<AskError>> {
    let ask = parse_ask(
        "SELECT n FROM t WHERE bucket >= $1 AND bucket < $2",
        &[Literal::Text(from.into()), Literal::Text(to.into())],
    )
    .unwrap();
    let answer = store.ask(&ask)?;
    Ok(answer.rows[0].values[0].clone())
}

/// PostgreSQL drops the offset when it reads text as a `timestamp`, so
/// `00:00:00-05:00` there means the wall clock 00:00. Straum would read
/// it as the instant 05:00Z and answer for other buckets, so it refuses.
#[test]
fn an_offset_bound_on_a_timestamp_is_refused() {
    let store = one_row(TimeColumn::Timestamp, JAN);
    let counted = count(
        &store,
        "2024-01-01T00:00:00-05:00",
        "2024-01-01T01:00:00-05:00",
    );
    assert_eq!(
        described(&counted.unwrap_err()),
        [
            "the view's time is a timestamp, so PostgreSQL does not use \
             the offset of a bound; write the wall clock time of the bound with Z"
        ]
    );
}

/// An offset on the upper bound alone is refused too: after a lower
/// bound with Z, and with no lower bound.
#[test]
fn an_offset_on_the_upper_bound_only_is_refused() {
    let store = one_row(TimeColumn::Timestamp, JAN);
    let refused = [
        "the view's time is a timestamp, so PostgreSQL does not use \
         the offset of a bound; write the wall clock time of the bound with Z",
    ];
    let to = "2024-01-01T01:00:00-05:00";
    let counted = count(&store, "2024-01-01T00:00:00Z", to);
    assert_eq!(described(&counted.unwrap_err()), refused);
    let ask = parse_ask(
        "SELECT n FROM t WHERE bucket < $1",
        &[Literal::Text(to.into())],
    )
    .unwrap();
    assert_eq!(described(&store.ask(&ask).unwrap_err()), refused);
}

#[test]
fn a_z_bound_on_a_timestamp_answers() {
    let store = one_row(TimeColumn::Timestamp, JAN);
    let counted = count(&store, "2024-01-01T00:00:00Z", "2024-01-01T01:00:00Z");
    assert_eq!(counted, Ok(Value::Int(1)));
}

#[test]
fn an_offset_bound_on_a_timestamptz_answers() {
    let store = one_row(TimeColumn::Timestamptz, JAN + 5 * HOUR);
    let counted = count(
        &store,
        "2024-01-01T00:00:00-05:00",
        "2024-01-01T01:00:00-05:00",
    );
    assert_eq!(counted, Ok(Value::Int(1)));
}

/// Every measure counts only the rows with `a = 1`.
const EVERY_MEASURE_FILTERED: &str = "CREATE VIEW t AS
SELECT buckets(f.at, '1 hour') AS bucket, f.a AS a,
       count(*) FILTER (WHERE f.a = 1) AS n, sum(f.v) FILTER (WHERE f.a = 1) AS s
FROM public.t AS f
GROUP BY 1, 2";

/// The rows with `a = 1`, counted.
const WHERE_FILTERED: &str = "CREATE VIEW t AS
SELECT buckets(f.at, '1 hour') AS bucket, f.a AS a, count(*) AS n, sum(f.v) AS s
FROM public.t AS f
WHERE f.a = 1
GROUP BY 1, 2";

/// The answer by `a` of the view `spec` makes, over one row with `a = 2`
/// and `v = 5`, which no filter keeps.
fn by_a_over_unkept_row(spec: &str) -> Vec<AnswerRow> {
    let spec = parse(spec).unwrap();
    let mut types = Vec::new();
    for column in spec.fact("t").unwrap().columns() {
        types.push(match column.as_str() {
            "at" => ColumnType::Time { zoned: true },
            _ => ColumnType::Int,
        });
    }
    let mut row = Vec::new();
    for column in spec.fact("t").unwrap().columns() {
        row.push(match column.as_str() {
            "at" => Datum::Int(JAN),
            "a" => Datum::Int(2),
            _ => Datum::Int(5),
        });
    }
    let mut store = Store::new(spec);
    store.set_types("t", &types).unwrap();
    store.add_row("t", &row).unwrap();
    let ask = parse_ask("SELECT a, n, s FROM t GROUP BY a", &[]).unwrap();
    store.ask(&ask).unwrap().rows
}

/// PostgreSQL keeps a group whose rows no FILTER keeps, counting 0 with
/// a null sum, where WHERE drops the rows before they group.
#[test]
fn a_filter_on_every_measure_keeps_a_group_where_would_drop() {
    assert_eq!(
        by_a_over_unkept_row(EVERY_MEASURE_FILTERED),
        [AnswerRow {
            keys: vec![Datum::Int(2)],
            values: vec![Value::Int(0), Value::Null],
        }]
    );
    assert_eq!(by_a_over_unkept_row(WHERE_FILTERED), []);
}
