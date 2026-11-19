//! HAVING on hand-made groups: each comparison keeps the groups it holds
//! for and drops the rest, a null measure meets no comparison, an average
//! compares exactly, and every wrong condition says what is wrong. A
//! window of the rows HAVING keeps builds those rows alone.

use crate::common::id;
use crate::rows::base_trip_with;
use crate::views::{spec, trip_types, typed};
use crate::{AT, END, FARE, PAY, START, TIP, TOLLED};
use straum_domain::{
    Ask, AskKey, ColumnType, Comparison, Condition, Cut, Datum, Identifier, Literal,
    MeasureCondition, Start, Store, Window,
};

/// The measures each ask here selects.
const MEASURES: [&str; 4] = ["trips", "fare", "tip_avg", "all_tolled"];

/// Fares at scale 2 and tips at scale 1, in three groups by pay:
///
/// | pay | fares         | tips     | tolled      |
/// | --- | ------------- | -------- | ----------- |
/// | 1   | 10.00, 20.50  | 0.1, 0.2 | true, false |
/// | 2   | 20.50         | 0.3      | true        |
/// | 3   | null          | null     | null        |
fn store() -> Store {
    let mut store = typed(&spec());
    let mut types = trip_types();
    types[FARE] = ColumnType::Numeric { scale: 2 };
    types[TIP] = ColumnType::Numeric { scale: 1 };
    store.set_types("trips", &types).unwrap();
    let rows = [
        (1, Datum::Int(1000), Datum::Int(1), Datum::Bool(true)),
        (1, Datum::Int(2050), Datum::Int(2), Datum::Bool(false)),
        (2, Datum::Int(2050), Datum::Int(3), Datum::Bool(true)),
        (3, Datum::Null, Datum::Null, Datum::Null),
    ];
    for (pay, fare, tip, tolled) in rows {
        let mut row = base_trip_with(AT, Datum::Int(START));
        row[PAY] = Datum::Int(pay);
        row[FARE] = fare;
        row[TIP] = tip;
        row[TOLLED] = tolled;
        store.add_row("trips", &row).unwrap();
    }
    store
}

/// `measure comparison value`, as HAVING writes it.
fn having(measure: &str, comparison: Comparison, value: Literal) -> MeasureCondition {
    MeasureCondition {
        measure: id(measure),
        comparison,
        value,
    }
}

fn decimal(units: i128, scale: u32) -> Literal {
    Literal::Decimal { units, scale }
}

/// The ask of [`MEASURES`] by `keys`, kept by `conditions`.
fn ask(keys: Vec<AskKey>, filter: Vec<Condition>, conditions: Vec<MeasureCondition>) -> Ask {
    let mut measures: Vec<Identifier> = Vec::new();
    for name in MEASURES {
        measures.push(id(name));
    }
    Ask {
        view: id("trips"),
        measures,
        keys,
        filter,
        having: conditions,
        from: Some(START),
        to: Some(END),
        bounded: None,
    }
}

/// The pays of the groups kept by `conditions`.
fn pays_kept(store: &Store, conditions: Vec<MeasureCondition>) -> Vec<i64> {
    let by_pay = ask(vec![AskKey::Key(id("pay"))], Vec::new(), conditions);
    let mut pays = Vec::new();
    for row in store.ask(&by_pay).unwrap().rows {
        let Datum::Int(pay) = row.keys[0] else {
            panic!("pay is an integer: {:?}", row.keys)
        };
        pays.push(pay);
    }
    pays
}

#[test]
fn each_comparison_keeps_the_groups_it_holds_for() {
    use Comparison::{Equal, Greater, GreaterOrEqual, Less, LessOrEqual, NotEqual};
    let store = store();
    let cases = [
        (vec![], vec![1, 2, 3]),
        (vec![having("trips", Greater, Literal::Int(1))], vec![1]),
        (
            vec![having("trips", GreaterOrEqual, Literal::Int(1))],
            vec![1, 2, 3],
        ),
        (vec![having("trips", LessOrEqual, Literal::Int(0))], vec![]),
        // 20.5, 20.50 and 2050 hundredths are one number.
        (vec![having("fare", Equal, decimal(205, 1))], vec![2]),
        (vec![having("fare", Equal, decimal(2050, 2))], vec![2]),
        (vec![having("fare", Equal, decimal(205_000, 4))], vec![2]),
        // A null fare meets no comparison, <> included.
        (vec![having("fare", NotEqual, decimal(205, 1))], vec![1]),
        (vec![having("fare", Greater, Literal::Int(20))], vec![1, 2]),
        (vec![having("fare", Less, decimal(305, 1))], vec![2]),
        (
            vec![having("fare", LessOrEqual, decimal(305, 1))],
            vec![1, 2],
        ),
        (vec![having("fare", Greater, Literal::Int(-1))], vec![1, 2]),
        // The average of 0.1 and 0.2 is 0.15 exactly; 0.15 has no exact
        // binary float, and a rounded one would miss either neighbour.
        (vec![having("tip_avg", Equal, decimal(15, 2))], vec![1]),
        (
            vec![having(
                "tip_avg",
                Greater,
                decimal(149_999_999_999_999_999, 18),
            )],
            vec![1, 2],
        ),
        (
            vec![having(
                "tip_avg",
                Less,
                decimal(150_000_000_000_000_001, 18),
            )],
            vec![1],
        ),
        (
            vec![having("all_tolled", Equal, Literal::Bool(true))],
            vec![2],
        ),
        (
            vec![having("all_tolled", NotEqual, Literal::Bool(true))],
            vec![1],
        ),
        // Past what an i128 holds at the fare's scale.
        (
            vec![having("fare", Less, Literal::Int(i64::MAX))],
            vec![1, 2],
        ),
        (
            vec![having("fare", Greater, Literal::Int(i64::MIN))],
            vec![1, 2],
        ),
        // Every condition must hold.
        (
            vec![
                having("trips", GreaterOrEqual, Literal::Int(1)),
                having("fare", Greater, Literal::Int(25)),
            ],
            vec![1],
        ),
    ];
    for (conditions, kept) in cases {
        let shown = format!("{conditions:?}");
        assert_eq!(pays_kept(&store, conditions), kept, "{shown}");
    }
}

#[test]
fn having_can_drop_the_one_row_of_an_ask_without_keys() {
    let store = store();
    let all = ask(Vec::new(), Vec::new(), Vec::new());
    assert_eq!(store.ask(&all).unwrap().rows.len(), 1);
    let none = ask(
        Vec::new(),
        Vec::new(),
        vec![having("trips", Comparison::Greater, Literal::Int(4))],
    );
    assert_eq!(store.ask(&none).unwrap().rows.len(), 0);
}

#[test]
fn each_wrong_condition_says_what_is_wrong() {
    use Comparison::Greater;
    let store = store();
    let wrong = ask(
        vec![AskKey::Key(id("pay"))],
        vec![Condition {
            key: id("fare"),
            values: vec![Literal::Int(1)],
        }],
        vec![
            having("pay", Comparison::Equal, Literal::Int(1)),
            having("tip_max", Greater, Literal::Int(1)),
            having("colour", Greater, Literal::Int(1)),
            having("fare", Greater, Literal::Text("1".into())),
            having("trips", Greater, Literal::Bool(true)),
            having("all_tolled", Comparison::Equal, Literal::Int(1)),
            having("all_tolled", Comparison::LessOrEqual, Literal::Bool(true)),
            having("fare", Greater, decimal(1, 39)),
        ],
    );
    let mut errors = Vec::new();
    for error in store.ask(&wrong).unwrap_err() {
        errors.push(error.to_string());
    }
    assert_eq!(
        errors,
        [
            "fare is a measure; compare it in HAVING",
            "pay is a key; filter it in WHERE",
            "HAVING compares tip_max, which the ask doesn't select",
            "unknown measure colour; the view's measures are trips, fare, tip_min, \
             tip_max, tip_median, tip_lowest, tip_highest, fare_p07, card_tip_p90, \
             tip_mode, distance_mode, distinct_fares, distinct_fare_count, \
             card_distinct_tips, card_distinct_tip, fare_var, tip_var_pop, distance_sd, card_tip_sd_pop, \
             tip_avg, distance, riders, pickups, card, shared_fare, mq_fare, m_riders, \
             far_tip, zone_four_distance, tipped, far_riders, all_tolled, any_tolled, \
             card_tolled",
            r#"fare is a number, so it can't be compared with the text "1""#,
            "trips is a number, so it can't be compared with the boolean true",
            "all_tolled is a boolean, so it can't be compared with the integer 1",
            "all_tolled is a boolean; compare it with = or <>, not <=",
            "fare can't be compared with the number 0.000000000000000000000000000000000000001: \
             at most 38 digits after the point",
        ]
    );
    let right = ask(
        vec![AskKey::Key(id("pay"))],
        Vec::new(),
        vec![
            having("fare", Greater, decimal(1, 38)),
            having("all_tolled", Comparison::NotEqual, Literal::Bool(false)),
        ],
    );
    assert!(store.ask(&right).is_ok());
}

#[test]
fn comparisons_print_as_an_ask_writes_them() {
    let comparisons = [
        Comparison::Equal,
        Comparison::NotEqual,
        Comparison::Less,
        Comparison::LessOrEqual,
        Comparison::Greater,
        Comparison::GreaterOrEqual,
    ];
    let mut written = Vec::new();
    for comparison in comparisons {
        written.push(comparison.to_string());
    }
    assert_eq!(written, ["=", "<>", "<", "<=", ">", ">="]);
}

/// The rows of `total` a window shows, as a Python slice gives them:
/// `rows[start:start + count]`, or `rows[-start:][:count]` from the end.
fn sliced(start: Start, count: usize, total: usize) -> std::ops::Range<usize> {
    let first = match start {
        Start::FromFirst(row) => row.min(total),
        Start::FromLast(rows) => total - rows.min(total),
    };
    first..(first + count).min(total)
}

/// A window builds the rows the whole answer has at its place, from
/// either end, and says where they lie among how many; past either end
/// it holds none. With HAVING, the rows counted are the ones it keeps.
#[test]
fn a_window_builds_the_rows_the_whole_answer_has_there() {
    let store = store();
    let kept_by = [
        vec![],
        vec![having("trips", Comparison::Greater, Literal::Int(0))],
        vec![having("trips", Comparison::Equal, Literal::Int(1))],
    ];
    for conditions in kept_by {
        let by_pay = ask(vec![AskKey::Key(id("pay"))], Vec::new(), conditions);
        let whole = store.ask(&by_pay).unwrap();
        let total = whole.rows.len();
        for row in 0..=total + 1 {
            for start in [Start::FromFirst(row), Start::FromLast(row)] {
                for count in 0..=total + 1 {
                    let window = Window { start, count };
                    let cut = store.ask_window(&by_pay, Some(window)).unwrap();
                    let range = sliced(start, count, total);
                    let expected = Cut {
                        first: range.start,
                        count: range.len(),
                        total,
                    };
                    assert_eq!(cut.cut, expected, "{window:?} of {total}");
                    assert_eq!(cut.answer.columns, whole.columns);
                    assert_eq!(cut.answer.rows, whole.rows[range], "{window:?}");
                }
            }
        }
    }
}
