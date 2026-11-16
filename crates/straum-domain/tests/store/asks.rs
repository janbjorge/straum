//! What an ask must be and what its answer looks like: filter values of
//! their column's type, answer columns named and scaled as asked, one row
//! when it groups by nothing, and an error for a name the spec lacks.

use crate::common::{condition, id, lit};
use crate::views::{spec, trip_types, typed};
use crate::{END, FARE, PAY, ROW_COLUMNS, START, TIP};
use straum_domain::{
    AnswerRow, Ask, AskError, AskKey, ColumnType, Condition, Identifier, Instead, Literal, Store,
    Value,
};

/// The trips over every bucket the tests fill, filtered.
fn trips_filtered(filter: Vec<Condition>) -> Ask {
    Ask {
        view: id("trips"),
        measures: vec![id("trips")],
        keys: Vec::new(),
        filter,
        having: Vec::new(),
        from: START,
        to: END,
    }
}

#[test]
fn filter_values_are_of_their_column_type() {
    let mut store = typed(&spec());
    let fits = vec![
        condition("pay", vec![Literal::Int(1)]),
        condition("shared", vec![Literal::Bool(true)]),
        condition("pu", vec![Literal::Int(2)]),
        condition("pickup_borough", vec![lit("m")]),
    ];
    assert!(store.ask(&trips_filtered(fits)).is_ok());

    let wrong = vec![
        condition("pay", vec![lit("1"), Literal::Int(1)]),
        condition("shared", vec![Literal::Int(1)]),
        condition("pickup_borough", vec![Literal::Bool(true)]),
    ];
    let errors: Vec<String> = (store.ask(&trips_filtered(wrong)).unwrap_err().iter())
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        errors,
        [
            r#"pay is an integer, so it can't be the text "1""#,
            "shared is a boolean, so it can't be the integer 1",
            "pickup_borough is text, so it can't be the boolean true",
        ]
    );

    // An integer reads as one only at scale 0.
    let pay = trips_filtered(vec![condition("pay", vec![Literal::Int(1)])]);
    let mut types = trip_types();
    types[PAY] = ColumnType::Numeric { scale: 0 };
    store.set_types("trips", &types).unwrap();
    assert!(store.ask(&pay).is_ok());
    types[PAY] = ColumnType::Numeric { scale: 2 };
    store.set_types("trips", &types).unwrap();
    assert_eq!(
        store.ask(&pay).unwrap_err()[0].to_string(),
        "pay is a numeric of scale 2, so it can't be the integer 1"
    );
}

/// The scale of each column of the answer.
fn answer_scales(store: &Store, ask: &Ask) -> Vec<u32> {
    let answer = store.ask(ask).unwrap();
    let mut scales = Vec::with_capacity(answer.columns.len());
    for column in &answer.columns {
        scales.push(column.scale);
    }
    scales
}

#[test]
fn types_stay_through_clear() {
    let mut store = Store::new(spec());
    let ask = Ask {
        view: id("trips"),
        measures: vec![id("trips"), id("fare"), id("tip_max")],
        keys: vec![AskKey::Key(id("pay")), AskKey::Key(id("pickup_zone"))],
        filter: Vec::new(),
        having: Vec::new(),
        from: START,
        to: END,
    };
    assert_eq!(answer_scales(&store, &ask), [0, 0, 0, 0, 0]);
    let mut types = vec![ColumnType::Int; ROW_COLUMNS];
    types[PAY] = ColumnType::Numeric { scale: 3 };
    types[FARE] = ColumnType::Numeric { scale: 2 };
    types[TIP] = ColumnType::Numeric { scale: 1 };
    store.set_types("trips", &types).unwrap();
    // An attribute has the type its dim column has.
    let zone = ColumnType::Numeric { scale: 4 };
    store
        .set_types("zones", &[ColumnType::Int, ColumnType::Text, zone])
        .unwrap();
    store.clear("trips");
    store.clear("zones");
    assert_eq!(answer_scales(&store, &ask), [3, 4, 0, 2, 1]);
}

#[test]
fn columns_name_keys_and_measures_as_asked() {
    let store = Store::new(spec());
    let ask = Ask {
        view: id("trips"),
        measures: vec![id("fare"), id("trips")],
        keys: vec![AskKey::Key(id("pickup_zone")), AskKey::Bucket],
        filter: Vec::new(),
        having: Vec::new(),
        from: START,
        to: END,
    };
    let columns = store.ask(&ask).unwrap().columns;
    let mut names = Vec::new();
    let mut times = Vec::new();
    for column in columns {
        names.push(column.name);
        times.push(column.is_time);
    }
    assert_eq!(names, ["pickup_zone", "bucket", "fare", "trips"]);
    assert_eq!(times, [false, true, false, false]);
}

#[test]
fn ask_without_by_answers_one_row() {
    let store = Store::new(spec());
    let fact = spec();
    let mut measures: Vec<Identifier> = Vec::new();
    for measure in &fact.fact("trips").unwrap().measures {
        measures.push(measure.name.clone());
    }
    let ask = Ask {
        view: id("trips"),
        measures,
        keys: Vec::new(),
        filter: Vec::new(),
        having: Vec::new(),
        from: START,
        to: END,
    };
    let zero = Value::Int(0);
    let null = Value::Null;
    assert_eq!(
        store.ask(&ask).unwrap().rows,
        [AnswerRow {
            keys: Vec::new(),
            values: vec![
                zero.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                // The five percentiles: of no values, none.
                null.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                zero.clone(),
                zero.clone(),
                zero.clone(),
                null.clone(),
                null.clone(),
                zero.clone(),
                null.clone(),
                null.clone(),
                zero.clone(),
                zero,
                null.clone(),
                null.clone(),
                null,
            ]
        }]
    );
}

#[test]
fn unknown_names() {
    let store = Store::new(spec());
    assert!(store.cells("zones").is_none());
    let ask = Ask {
        view: id("zones"),
        measures: vec![id("trips")],
        keys: Vec::new(),
        filter: Vec::new(),
        having: Vec::new(),
        from: START,
        to: END,
    };
    assert_eq!(
        store.ask(&ask),
        Err(vec![AskError::UnknownView {
            name: id("zones"),
            instead: Instead::Every(vec![id("trips")])
        }])
    );
}
