//! What each ask error says: the words straum-notation gives the
//! domain's `AskError`, naming the Straum SQL to write.

mod common;

use common::id;
use straum_domain::{AskError, ColumnType, Comparison, Instead, Literal, Width};
use straum_notation::describe_ask_error;

const DAY: u32 = 86_400;
const WEEK: u32 = 7 * DAY;

#[test]
fn ask_errors_display() {
    let cases = [
        (
            AskError::UnknownView {
                name: id("nope"),
                instead: Instead::Every(vec![]),
            },
            "unknown view nope",
        ),
        (
            AskError::UnknownView {
                name: id("tirps"),
                instead: Instead::Closest(id("trips")),
            },
            "unknown view tirps; did you mean trips?",
        ),
        (
            AskError::UnknownMeasure {
                name: id("tips"),
                instead: Instead::Every(vec![id("n"), id("fare")]),
            },
            "unknown measure tips; the view's measures are n, fare",
        ),
        (
            AskError::UnknownKey {
                name: id("fare"),
                instead: Instead::Every(vec![id("bucket"), id("a")]),
            },
            "fare is not a key of the view; the view's keys are bucket, a",
        ),
        (
            AskError::KeyNotGrouped(id("name")),
            "name is a key; add it to GROUP BY",
        ),
        (AskError::NoMeasures, "no measures asked"),
        (AskError::TimeKeyTwice, "more than one time key"),
        (AskError::AskedTwice("n".into()), "n asked twice"),
        (
            AskError::RangeEmpty { from: 2, to: 1 },
            "empty range [2, 1)",
        ),
        (
            AskError::RangeUnaligned {
                at: 60,
                width: Width { seconds: 900 },
            },
            "bound 60 is not on a 15 minutes bucket edge; the nearest are 0 and 900",
        ),
    ];
    for (error, shown) in cases {
        assert_eq!(describe_ask_error(&error), shown);
    }
}

#[test]
fn width_and_range_errors_display() {
    let cases = [
        (
            AskError::OwnWidth {
                name: id("bucket"),
                width: Width { seconds: 3_600 },
            },
            "buckets(bucket, '1 hour') is the view's own width; write bucket",
        ),
        (
            AskError::WidthNotKept {
                width: Width { seconds: 2 * DAY },
                kept: vec![Width { seconds: 3_600 }, Width { seconds: DAY }],
            },
            "bucket width '2 days' is not kept by the view; it keeps 1 hour, 1 day",
        ),
        (
            AskError::RangeUnaligned {
                at: -60,
                width: Width { seconds: WEEK },
            },
            "bound -60 is not on a 7 days bucket edge; the nearest are -604800 and 0",
        ),
        (
            AskError::RangeUnaligned {
                at: i64::MAX,
                width: Width { seconds: 900 },
            },
            "bound 9223372036854775807 is not on a 15 minutes bucket edge; \
             the nearest before it is 9223372036854775800",
        ),
        (
            AskError::RangeUnaligned {
                at: i64::MIN,
                width: Width { seconds: 900 },
            },
            "bound -9223372036854775808 is not on a 15 minutes bucket edge; \
             the nearest after it is -9223372036854775800",
        ),
    ];
    for (error, shown) in cases {
        assert_eq!(describe_ask_error(&error), shown);
    }
}

/// The errors about the view's time and the clauses each name what to
/// write, in Straum SQL.
#[test]
fn time_and_clause_errors_display() {
    let cases = [
        (
            AskError::NotTime {
                name: id("a"),
                time: id("pickup"),
            },
            "a is not the view's time; buckets and the range read pickup",
        ),
        (
            AskError::TimeInWhere(id("pickup")),
            "pickup is the view's time; bound it with pickup >= and pickup <",
        ),
        (
            AskError::MeasureInWhere(id("fare")),
            "fare is a measure; compare it in HAVING",
        ),
        (
            AskError::KeyInHaving(id("pay")),
            "pay is a key; filter it in WHERE",
        ),
        (
            AskError::HavingNotSelected(id("tip_max")),
            "HAVING compares tip_max, which the ask doesn't select",
        ),
    ];
    for (error, shown) in cases {
        assert_eq!(describe_ask_error(&error), shown);
    }
}

/// A value of the wrong kind says what the key or measure is, and how to
/// write a value it takes where there is one spelling.
#[test]
fn value_errors_display() {
    let cases = [
        (
            AskError::NotANumber {
                measure: id("fare"),
                value: Literal::Text("1".into()),
            },
            r#"fare is a number, so it can't be compared with the text "1""#,
        ),
        (
            AskError::NotABoolean {
                measure: id("all_tolled"),
                value: Literal::Int(1),
            },
            "all_tolled is a boolean, so it can't be compared with the integer 1",
        ),
        (
            AskError::BooleanOrder {
                measure: id("all_tolled"),
                comparison: Comparison::LessOrEqual,
            },
            "all_tolled is a boolean; compare it with = or <>, not <=",
        ),
        (
            AskError::TooManyDigits {
                measure: id("fare"),
                value: Literal::Decimal {
                    units: 1,
                    scale: 39,
                },
            },
            "fare can't be compared with the number 0.000000000000000000000000000000000000001: \
             at most 38 digits after the point",
        ),
        (
            AskError::WrongType {
                key: id("pay"),
                column: ColumnType::Numeric { scale: 2 },
                value: Literal::Int(1),
            },
            "pay is a numeric of scale 2, so it can't be the integer 1",
        ),
        (
            AskError::WrongType {
                key: id("pay"),
                column: ColumnType::Date,
                value: Literal::Text("2024-1-15".into()),
            },
            r#"pay is a date, so it can't be the text "2024-1-15"; write a date as YYYY-MM-DD, such as 2024-01-15"#,
        ),
        (
            AskError::WrongType {
                key: id("pickup_borough"),
                column: ColumnType::Text,
                value: Literal::Bool(true),
            },
            "pickup_borough is text, so it can't be the boolean true",
        ),
    ];
    for (error, shown) in cases {
        assert_eq!(describe_ask_error(&error), shown);
    }
}

#[test]
fn comparisons_are_written_as_an_ask_writes_them() {
    let comparisons = [
        (Comparison::Equal, "="),
        (Comparison::NotEqual, "<>"),
        (Comparison::Less, "<"),
        (Comparison::LessOrEqual, "<="),
        (Comparison::Greater, ">"),
        (Comparison::GreaterOrEqual, ">="),
    ];
    for (comparison, written) in comparisons {
        let error = AskError::BooleanOrder {
            measure: id("paid"),
            comparison,
        };
        assert_eq!(
            describe_ask_error(&error),
            format!("paid is a boolean; compare it with = or <>, not {written}")
        );
    }
}
