//! HAVING on hand-made groups: each comparison keeps the groups it holds
//! for and drops the rest, a null measure meets no comparison, an average
//! and a variance or deviation compare exactly, and every wrong condition
//! says what is wrong. A
//! window of the rows HAVING keeps builds those rows alone.

use crate::common::{id, measure};
use crate::rows::base_trip_with;
use crate::views::{spec, trip_types, trips_by_pay, typed};
use crate::{AT, FARE, PAY, START, TIP, TOLLED, trips_ask};
use straum_domain::{
    Aggregate, Ask, AskError, AskKey, ColumnType, Comparison, Condition, Cut, Datum, Literal,
    MeasureCondition, Spec, Start, Store, Window,
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
    Ask {
        filter,
        having: conditions,
        ..trips_ask(&MEASURES, keys)
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
        (
            vec![having("all_tolled", Equal, Literal::Bool(true))],
            vec![2],
        ),
        (
            vec![having("all_tolled", NotEqual, Literal::Bool(true))],
            vec![1],
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

/// A comparison is exact where a float would round, and holds for a
/// value past what an `i128` holds at the measure's scale.
#[test]
fn a_comparison_is_exact_past_a_float_and_an_i128() {
    use Comparison::{Equal, Greater, Less};
    let store = store();
    let cases = [
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
        // Past what an i128 holds at the fare's scale.
        (
            vec![having("fare", Less, Literal::Int(i64::MAX))],
            vec![1, 2],
        ),
        (
            vec![having("fare", Greater, Literal::Int(i64::MIN))],
            vec![1, 2],
        ),
    ];
    for (conditions, kept) in cases {
        let shown = format!("{conditions:?}");
        assert_eq!(pays_kept(&store, conditions), kept, "{shown}");
    }
}

/// The spread measures of [`spread_store`], each of the fare.
const SPREADS: [&str; 4] = ["var_samp", "var_pop", "sd_samp", "sd_pop"];

/// Integer fares in five groups by pay, and the first 6 places of each
/// spread; the answer gives each at 4 places:
///
/// | pay | fares      | var_samp | var_pop  | sd_samp  | sd_pop   |
/// | --- | ---------- | -------- | -------- | -------- | -------- |
/// | 1   | 1, 2       | 0.5      | 0.25     | 0.707106 | 0.5      |
/// | 2   | 0, 0, 1    | 0.333333 | 0.222222 | 0.577350 | 0.471404 |
/// | 3   | 0, 0, 2    | 1.333333 | 0.888888 | 1.154700 | 0.942809 |
/// | 4   | 0, 0, 1, 2 | 0.916666 | 0.6875   | 0.957427 | 0.829156 |
/// | 5   | 5          | null     | 0        | null     | 0        |
fn spread_store() -> Store {
    let trips = trips_by_pay(vec![
        measure("var_samp", Aggregate::VarSamp(id("fare"))),
        measure("var_pop", Aggregate::VarPop(id("fare"))),
        measure("sd_samp", Aggregate::StddevSamp(id("fare"))),
        measure("sd_pop", Aggregate::StddevPop(id("fare"))),
    ]);
    let mut store = Store::new(Spec::new(vec![], vec![trips]).unwrap());
    let groups: [(i64, &[i64]); 5] = [
        (1, &[1, 2]),
        (2, &[0, 0, 1]),
        (3, &[0, 0, 2]),
        (4, &[0, 0, 1, 2]),
        (5, &[5]),
    ];
    for (pay, fares) in groups {
        for &fare in fares {
            let row = [Datum::Int(START), Datum::Int(pay), Datum::Int(fare)];
            store.add_row("trips", &row).unwrap();
        }
    }
    store
}

/// A variance or deviation compares exactly, as PostgreSQL compares it,
/// not as its answer rounds it to 4 places. A rounded value drops a group
/// whose spread is a little above it, and keeps a group whose spread is
/// a little below it.
#[test]
fn a_spread_compares_exactly_not_as_rounded() {
    use Comparison::{Equal, Greater, GreaterOrEqual, LessOrEqual};
    let store = spread_store();
    let cases = [
        // Rounded down to the value: `>` keeps the group.
        ("var_samp", Greater, decimal(3_333, 4), vec![1, 2, 3, 4]),
        ("var_pop", Greater, decimal(2_222, 4), vec![1, 2, 3, 4]),
        ("sd_samp", Greater, decimal(7_071, 4), vec![1, 3, 4]),
        ("sd_pop", Greater, decimal(4_714, 4), vec![1, 2, 3, 4]),
        ("sd_samp", LessOrEqual, decimal(7_071, 4), vec![2]),
        // Rounded up to the value: `>=` and `=` drop the group.
        ("var_samp", GreaterOrEqual, decimal(9_167, 4), vec![3]),
        ("var_pop", Equal, decimal(8_889, 4), vec![]),
        ("sd_samp", GreaterOrEqual, decimal(5_774, 4), vec![1, 3, 4]),
        ("sd_pop", Equal, decimal(8_292, 4), vec![]),
        // More places than the answer: sd_samp of pay 1 is 0.707106781...
        ("sd_samp", Greater, decimal(70_710_678, 8), vec![1, 3, 4]),
        ("sd_samp", Greater, decimal(70_710_679, 8), vec![3, 4]),
    ];
    for (name, comparison, value, kept) in cases {
        let shown = format!("{name} {comparison:?} {value:?}");
        let conditions = vec![having(name, comparison, value)];
        assert_eq!(spread_pays_kept(&store, conditions), kept, "{shown}");
    }
}

/// A value clearly apart from each spread keeps the same groups as a
/// rounded comparison: below zero, at zero, past an `i128` and between
/// the groups. A null spread meets no comparison.
#[test]
fn a_spread_apart_from_the_value_keeps_what_rounded_keeps() {
    use Comparison::{Equal, Greater, GreaterOrEqual, Less};
    let store = spread_store();
    let cases = [
        ("var_samp", Greater, decimal(3, 1), vec![1, 2, 3, 4]),
        ("sd_pop", Less, decimal(5, 1), vec![2, 5]),
        ("sd_pop", Equal, decimal(5, 1), vec![1]),
        ("var_pop", Equal, Literal::Int(0), vec![5]),
        ("sd_samp", Greater, Literal::Int(-1), vec![1, 2, 3, 4]),
        (
            "var_pop",
            GreaterOrEqual,
            Literal::Int(0),
            vec![1, 2, 3, 4, 5],
        ),
        ("var_samp", Less, Literal::Int(i64::MAX), vec![1, 2, 3, 4]),
        ("sd_samp", Less, Literal::Int(i64::MAX), vec![1, 2, 3, 4]),
        (
            "sd_pop",
            Greater,
            Literal::Int(i64::MIN),
            vec![1, 2, 3, 4, 5],
        ),
        (
            "var_pop",
            Greater,
            Literal::Int(i64::MIN),
            vec![1, 2, 3, 4, 5],
        ),
    ];
    for (name, comparison, value, kept) in cases {
        let shown = format!("{name} {comparison:?} {value:?}");
        let conditions = vec![having(name, comparison, value)];
        assert_eq!(spread_pays_kept(&store, conditions), kept, "{shown}");
    }
}

/// The pays of the groups of [`spread_store`] kept by `conditions`.
fn spread_pays_kept(store: &Store, conditions: Vec<MeasureCondition>) -> Vec<i64> {
    let by_pay = Ask {
        having: conditions,
        ..trips_ask(&SPREADS, vec![AskKey::Key(id("pay"))])
    };
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
    // What each error says is straum-notation's; here, which errors.
    let mut variants = Vec::new();
    for error in store.ask(&wrong).unwrap_err() {
        variants.push(variant(&error));
    }
    assert_eq!(
        variants,
        [
            "MeasureInWhere",
            "KeyInHaving",
            "HavingNotSelected",
            "UnknownMeasure",
            "NotANumber",
            "NotANumber",
            "NotABoolean",
            "BooleanOrder",
            "TooManyDigits",
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

/// The name of `error`'s variant.
fn variant(error: &AskError) -> String {
    let debug = format!("{error:?}");
    let name = debug.split([' ', '(', '{']).next().unwrap_or_default();
    name.to_owned()
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
