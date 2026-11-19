//! What an ask must be and what its answer looks like: filter values of
//! their column's type, answer columns named and scaled as asked, one row
//! when it groups by nothing, and an error for a name the spec lacks.

use crate::common::{Rng, condition, id, lit};
use crate::random::trip;
use crate::views::{spec, trip_types, typed};
use crate::{AT, END, FARE, PAY, ROW_COLUMNS, START, TIP};
use straum_domain::{
    AnswerRow, Ask, AskError, AskKey, ColumnType, Condition, Datum, Identifier, Instead, Literal,
    Store, Value,
};

/// The trips over every bucket the tests fill, filtered.
fn trips_filtered(filter: Vec<Condition>) -> Ask {
    Ask {
        view: id("trips"),
        measures: vec![id("trips")],
        keys: Vec::new(),
        filter,
        having: Vec::new(),
        from: Some(START),
        to: Some(END),
        bounded: None,
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

/// The trips whose `pay` is the value `text` spells.
fn trips_paid(store: &Store, text: &str) -> Result<Value, Vec<AskError>> {
    let ask = trips_filtered(vec![condition("pay", vec![lit(text)])]);
    let answer = store.ask(&ask)?;
    Ok(answer.rows[0].values[0].clone())
}

/// A date or a uuid key is filtered by its text as PostgreSQL prints
/// it, and by no other spelling.
#[test]
fn dates_and_uuids_are_filtered_by_their_text() {
    let mut store = typed(&spec());
    let mut types = trip_types();
    types[PAY] = ColumnType::Date;
    store.set_types("trips", &types).unwrap();
    let mut rng = Rng(5);
    let mut rows = Vec::new();
    for days in [8_780, 8_780, 8_781, i32::MAX] {
        let mut row = trip(&mut rng);
        row[AT] = Datum::Int(START);
        row[PAY] = Datum::Date(days);
        rows.extend(row);
    }
    store.add_rows("trips", &rows).unwrap();
    assert_eq!(trips_paid(&store, "2024-01-15"), Ok(Value::Int(2)));
    assert_eq!(trips_paid(&store, "2024-01-16"), Ok(Value::Int(1)));
    assert_eq!(trips_paid(&store, "infinity"), Ok(Value::Int(1)));
    assert_eq!(trips_paid(&store, "2024-01-17"), Ok(Value::Int(0)));
    assert_eq!(
        trips_paid(&store, "2024-1-15").unwrap_err()[0].to_string(),
        r#"pay is a date, so it can't be the text "2024-1-15"; write a date as YYYY-MM-DD, such as 2024-01-15"#
    );

    let uuid = "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11";
    types[PAY] = ColumnType::Uuid;
    store.set_types("trips", &types).unwrap();
    store.clear("trips");
    let mut row = trip(&mut rng);
    row[AT] = Datum::Int(START);
    row[PAY] = Datum::Uuid(straum_domain::parse_uuid(uuid).unwrap());
    store.add_row("trips", &row).unwrap();
    assert_eq!(trips_paid(&store, uuid), Ok(Value::Int(1)));
    assert_eq!(
        trips_paid(&store, "00000000-0000-0000-0000-000000000000"),
        Ok(Value::Int(0))
    );
    assert_eq!(
        trips_paid(&store, "2024-01-15").unwrap_err()[0].to_string(),
        "pay is a uuid, so it can't be the text \"2024-01-15\"; write a uuid as lowercase hex \
         in groups of 8-4-4-4-12, such as a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11"
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
        from: Some(START),
        to: Some(END),
        bounded: None,
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
        keys: vec![AskKey::Key(id("pickup_zone")), AskKey::Key(id("bucket"))],
        filter: Vec::new(),
        having: Vec::new(),
        from: Some(START),
        to: Some(END),
        bounded: None,
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
        from: Some(START),
        to: Some(END),
        bounded: None,
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
                // The five percentiles and the two modes: of no values,
                // none.
                null.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                null.clone(),
                // A sum of distinct values, two counts of them, an
                // average of them and the four variances and deviations:
                // of no values, none, or none counted.
                null.clone(),
                zero.clone(),
                zero.clone(),
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
        from: Some(START),
        to: Some(END),
        bounded: None,
    };
    assert_eq!(
        store.ask(&ask),
        Err(vec![AskError::UnknownView {
            name: id("zones"),
            instead: Instead::Every(vec![id("trips")])
        }])
    );
}

/// `pay IN (values)`.
fn pay_in(values: &[i64]) -> Condition {
    let mut literals = Vec::new();
    for value in values {
        literals.push(Literal::Int(*value));
    }
    condition("pay", literals)
}

/// The trips `filter` keeps.
fn trips_kept(store: &Store, filter: Vec<Condition>) -> Value {
    store.ask(&trips_filtered(filter)).unwrap().rows[0].values[0].clone()
}

/// Conditions on one key all hold: the values they share, as SQL's AND
/// of them says, however often one is repeated.
#[test]
fn conditions_on_one_key_hold_for_the_values_they_share() {
    let mut store = typed(&spec());
    let mut rng = Rng(7);
    let mut paid = [0_i128; 3];
    for _ in 0..300 {
        let mut row = trip(&mut rng);
        row[AT] = Datum::Int(START);
        let pay = rng.below(3);
        row[PAY] = Datum::Int(i64::try_from(pay).unwrap());
        paid[pay] += 1;
        store.add_row("trips", &row).unwrap();
    }
    let both = Value::Int(paid[1] + paid[2]);
    assert_eq!(trips_kept(&store, vec![pay_in(&[1, 2])]), both);
    let shared = vec![pay_in(&[1, 2]), pay_in(&[2, 0])];
    assert_eq!(trips_kept(&store, shared), Value::Int(paid[2]));
    let none = vec![pay_in(&[1]), pay_in(&[2])];
    assert_eq!(trips_kept(&store, none), Value::Int(0));
    let repeated = vec![pay_in(&[1]); 1_000];
    assert_eq!(trips_kept(&store, repeated), Value::Int(paid[1]));
}
