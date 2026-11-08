use weir_domain::{
    Agg, Ask, AskError, AskKey, Cond, Grain, Ident, KeyPath, Literal, Slot, Spec, SpecError,
    TimeKey,
};
use weir_notation::{Error, parse};

const TAXI: &str = include_str!("taxi.weir");

// 2024-01-01 and 2024-02-01, 00:00 UTC.
const JAN: i64 = 1_704_067_200;
const FEB: i64 = 1_706_745_600;

fn id(s: &str) -> Ident {
    Ident::new(s).unwrap()
}

fn path(s: &str) -> KeyPath {
    match s.split_once('.') {
        Some((head, attr)) => KeyPath {
            head: id(head),
            attr: Some(id(attr)),
        },
        None => KeyPath {
            head: id(s),
            attr: None,
        },
    }
}

fn taxi() -> Spec {
    parse(TAXI).unwrap()
}

fn spec_errors(src: &str) -> Vec<SpecError> {
    match parse(src) {
        Err(Error::Spec(errors)) => errors,
        other => panic!("expected spec errors, got {other:?}"),
    }
}

fn syntax_error(src: &str) -> (usize, String) {
    match parse(src) {
        Err(Error::Syntax(e)) => (e.line, e.msg),
        other => panic!("expected a syntax error, got {other:?}"),
    }
}

fn month_ask() -> Ask {
    Ask {
        fact: id("trips"),
        measures: vec![id("trips"), id("fare"), id("tip_max")],
        by: vec![
            AskKey::Key(path("pickup.borough")),
            AskKey::Time(TimeKey::Day),
        ],
        filter: vec![Cond {
            path: path("dropoff.borough"),
            values: vec![Literal::Text("Manhattan".into())],
        }],
        from: JAN,
        to: FEB,
    }
}

#[test]
fn taxi_parses() {
    let spec = taxi();
    let trips = spec.fact("trips").unwrap();
    assert_eq!(trips.time.grain, Grain(900));
    assert!(trips.append_only);
    assert_eq!(spec.dim("zones").unwrap().attrs.len(), 3);

    let key: Vec<String> = trips
        .rollup_key()
        .map(|slot| match slot {
            Slot::Column(c) => c.to_string(),
            Slot::Ref(r) => r.name.to_string(),
            Slot::Bucket => "bucket".into(),
        })
        .collect();
    assert_eq!(
        key,
        ["payment_type", "vendor_id", "pickup", "dropoff", "bucket"]
    );

    assert_eq!(
        trips.measure("distance").unwrap().agg,
        Agg::Avg(id("trip_distance"))
    );
    assert_eq!(
        trips.measure("card").unwrap().filter,
        [Cond {
            path: path("payment_type"),
            values: vec![Literal::Int(1)]
        }]
    );
    assert_eq!(
        trips.measure("manhattan_pickups").unwrap().filter[0].path,
        path("pickup.borough")
    );
}

#[test]
fn ask_by_dim_attribute() {
    let spec = taxi();
    assert_eq!(spec.check(&month_ask()).unwrap().name, id("trips"));
}

#[test]
fn ask_rejects_what_the_rollup_does_not_hold() {
    let spec = taxi();
    let ask = Ask {
        measures: vec![id("trips"), id("tips")],
        by: vec![
            AskKey::Key(path("fare_amount")),
            AskKey::Key(path("pickup.colour")),
            AskKey::Time(TimeKey::Day),
            AskKey::Time(TimeKey::Hour),
        ],
        from: JAN + 60,
        ..month_ask()
    };
    assert_eq!(
        spec.check(&ask).unwrap_err(),
        [
            AskError::UnknownMeasure(id("tips")),
            AskError::UnknownKey(path("fare_amount")),
            AskError::UnknownKey(path("pickup.colour")),
            AskError::TimeKeyTwice,
            AskError::RangeUnaligned {
                at: JAN + 60,
                grain: Grain(900)
            },
        ]
    );
}

#[test]
fn ask_hour_needs_grain_that_splits_an_hour() {
    let spec = parse(&TAXI.replace("/ 15m", "/ 2h")).unwrap();
    let ask = Ask {
        by: vec![AskKey::Time(TimeKey::Hour)],
        ..month_ask()
    };
    assert_eq!(
        spec.check(&ask).unwrap_err(),
        [AskError::TimeKeyUnaligned {
            key: TimeKey::Hour,
            grain: Grain(7_200)
        }]
    );
}

#[test]
fn spec_errors_are_all_reported() {
    let src = TAXI
        .replace("/ 15m", "/ 7m")
        .replace("dropoff -> zones", "dropoff -> zone")
        .replace("by payment_type", "by id, payment_type")
        .replace("payment_type = 1", "fare_amount = 1");
    assert_eq!(
        spec_errors(&src),
        [
            SpecError::BadGrain {
                fact: id("trips"),
                seconds: 420
            },
            SpecError::KeyInRollup {
                fact: id("trips"),
                column: id("id")
            },
            SpecError::UnknownDim {
                fact: id("trips"),
                ref_name: id("dropoff"),
                dim: id("zone")
            },
            SpecError::UnknownKey {
                fact: id("trips"),
                measure: id("card"),
                path: path("fare_amount"),
            },
        ]
    );
}

#[test]
fn ref_name_clashes_with_by_column() {
    let src = TAXI.replace("by payment_type", "by pickup, payment_type");
    assert_eq!(
        spec_errors(&src),
        [SpecError::Duplicate {
            scope: Some(id("trips")),
            name: id("pickup")
        }]
    );
}

#[test]
fn syntax_errors_point_at_the_line() {
    assert_eq!(
        syntax_error("fact t = public.t key id\n  time at / 15x\n  n count\n"),
        (2, "grain 15x needs a unit: s, m, h or d".into())
    );
    assert_eq!(
        syntax_error("fact t = public.t key id\n  n count\n"),
        (1, "fact t has no time line".into())
    );
    assert_eq!(
        syntax_error("  n count\n"),
        (1, "indented line outside a dim or fact block".into())
    );
    assert_eq!(
        syntax_error("dim Zones = public.zones key id\n"),
        (1, "name Zones is not a lowercase identifier".into())
    );
    assert_eq!(
        syntax_error("fact t = public.t key id\n  time at / 15m\n  n count where x = 'open\n"),
        (3, "unterminated text".into())
    );
}

#[test]
fn literals() {
    let spec = parse(
        "fact t = public.t key id\n\
         \x20 time at / 1h\n\
         \x20 by a, b, c\n\
         \x20 n count where a in (-1, 2) and b = 'it''s' and c = true # note\n",
    )
    .unwrap();
    assert_eq!(
        spec.fact("t").unwrap().measure("n").unwrap().filter,
        [
            Cond {
                path: path("a"),
                values: vec![Literal::Int(-1), Literal::Int(2)]
            },
            Cond {
                path: path("b"),
                values: vec![Literal::Text("it's".into())]
            },
            Cond {
                path: path("c"),
                values: vec![Literal::Bool(true)]
            },
        ]
    );
}
