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
fn comments_and_blank_lines_only() {
    let spec = parse("# nothing declared yet\n\n   \n").unwrap();
    assert!(spec.fact("trips").is_none());
    assert!(spec.dim("zones").is_none());
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

#[test]
fn every_agg_grain_unit_and_key_form() {
    let spec = parse(
        "fact t = public.t key id\n\
         \x20 time at / 30s\n\
         \x20 ref r -> z via r_id\n\
         \x20 by a\n\
         \x20 n count\n\
         \x20 s sum x\n\
         \x20 lo min x\n\
         \x20 hi max x\n\
         \x20 mean avg x\n\
         \x20 uniq count_distinct x where r = 1 and r.id = 2 and a = false\n\
         dim z = public.z key id\n",
    )
    .unwrap();
    let t = spec.fact("t").unwrap();
    assert_eq!(t.time.grain, Grain(30));
    let aggs: Vec<&Agg> = t.measures.iter().map(|m| &m.agg).collect();
    assert_eq!(
        aggs,
        [
            &Agg::Count,
            &Agg::Sum(id("x")),
            &Agg::Min(id("x")),
            &Agg::Max(id("x")),
            &Agg::Avg(id("x")),
            &Agg::CountDistinct(id("x")),
        ]
    );
    assert!(spec.dim("z").unwrap().attrs.is_empty());

    let day = parse("fact d = public.d key id\n  time at / 1d\n  n count\n").unwrap();
    assert_eq!(day.fact("d").unwrap().time.grain, Grain(86_400));
}

#[test]
fn syntax_error_messages() {
    const T: &str = "fact t = public.t key id\n  time at / 15m\n";
    let cases = [
        (
            "table t = public.t key id",
            1,
            "expected dim or fact, found table",
        ),
        ("5", 1, "expected dim or fact, found 5"),
        ("dim z public.z key id", 1, "expected =, found public"),
        ("dim z = public.z", 1, "expected key, found end of line"),
        ("dim z = public.z key id extra", 1, "unexpected extra"),
        ("dim z = public.z key id )", 1, "unexpected )"),
        (
            "dim z = public.z key id\n  by a",
            2,
            "expected attrs, found by",
        ),
        (
            "dim z = public.z key id\n  attrs 'it''s'",
            2,
            "expected column, found 'it''s'",
        ),
        (
            "fact t = public.t key id\n  time at / 15",
            2,
            "expected grain such as 15m, found 15",
        ),
        (
            "fact t = public.t key id\n  time at / xm",
            2,
            "bad grain xm",
        ),
        (
            "fact t = public.t key id\n  time at / 5000000d",
            2,
            "grain 5000000d too large",
        ),
        (
            &format!("{T}  5 count"),
            3,
            "expected time, ref, by, append or a measure, found 5",
        ),
        (&format!("{T}  time at / 1h"), 3, "time declared twice"),
        (
            &format!("{T}  ref r zones via c"),
            3,
            "expected ->, found zones",
        ),
        (
            &format!("{T}  Fare sum x"),
            3,
            "measure name Fare is not a lowercase identifier",
        ),
        (
            &format!("{T}  n median x"),
            3,
            "expected count, sum, min, max, avg or count_distinct, found median",
        ),
        (
            &format!("{T}  n count where a b"),
            3,
            "expected = or in, found b",
        ),
        (
            &format!("{T}  n count where a in 1"),
            3,
            "expected (, found 1",
        ),
        (
            &format!("{T}  n count where a = x"),
            3,
            "expected a number, 'text', true or false, found x",
        ),
        (&format!("{T}  n count where a = -x"), 3, "bad number -x"),
        (
            &format!("{T}  n count where a = 99999999999999999999"),
            3,
            "number 99999999999999999999 too large",
        ),
        (&format!("{T}  n count;"), 3, "unexpected ';'"),
    ];
    for (src, line, msg) in cases {
        assert_eq!(syntax_error(src), (line, msg.to_string()), "{src}");
    }
}

#[test]
fn error_display() {
    assert_eq!(
        parse("  n count").unwrap_err().to_string(),
        "line 1: indented line outside a dim or fact block"
    );

    let src = "dim z = public.z key id\n\
               \x20 attrs name, name\n\
               dim z = public.z2 key id\n\
               fact t = public.t key id\n\
               \x20 time at / 7m\n\
               \x20 ref r -> nope via r_id\n\
               \x20 by id, at, b, r\n\
               \x20 m count where c = 1\n\
               \x20 m count\n\
               fact u = public.u key id\n\
               \x20 time at / 1h\n";
    assert_eq!(
        parse(src).unwrap_err().to_string(),
        "z declared twice\n\
         z: name declared twice\n\
         fact t: grain of 420s does not divide a day\n\
         fact t: key column id can't be part of the rollup key\n\
         fact t: column at used twice among time, by and ref\n\
         t: r declared twice\n\
         fact t: ref r points at unknown dim nope\n\
         fact t: measure m filters on c, which is not a by column, ref or ref attribute\n\
         t: m declared twice\n\
         fact u: no measures"
    );

    let ask_errors = [
        (AskError::UnknownFact(id("nope")), "unknown fact nope"),
        (AskError::UnknownMeasure(id("tips")), "unknown measure tips"),
        (
            AskError::UnknownKey(path("pickup.colour")),
            "pickup.colour is not a by column, ref or ref attribute",
        ),
        (AskError::NoMeasures, "no measures asked"),
        (AskError::TimeKeyTwice, "more than one time key"),
        (
            AskError::TimeKeyUnaligned {
                key: TimeKey::Hour,
                grain: Grain(7_200),
            },
            "hour does not split into 7200s buckets",
        ),
        (
            AskError::RangeEmpty { from: 2, to: 1 },
            "empty range [2, 1)",
        ),
        (
            AskError::RangeUnaligned {
                at: 60,
                grain: Grain(900),
            },
            "range end 60 is not on a 900s bucket edge",
        ),
    ];
    for (e, msg) in ask_errors {
        assert_eq!(e.to_string(), msg);
    }

    let keys = [
        TimeKey::Bucket,
        TimeKey::Hour,
        TimeKey::Day,
        TimeKey::Week,
        TimeKey::Month,
    ];
    assert_eq!(
        keys.map(|k| k.to_string()),
        ["bucket", "hour", "day", "week", "month"]
    );
}

#[test]
fn ask_every_time_key_and_key_form() {
    let spec = taxi();
    for key in [
        TimeKey::Bucket,
        TimeKey::Hour,
        TimeKey::Day,
        TimeKey::Week,
        TimeKey::Month,
    ] {
        let ask = Ask {
            by: vec![
                AskKey::Key(path("pickup")),
                AskKey::Key(path("dropoff.location_id")),
                AskKey::Key(path("payment_type")),
                AskKey::Time(key),
            ],
            ..month_ask()
        };
        assert!(spec.check(&ask).is_ok(), "{key}");
    }
}

#[test]
fn ask_rejects_bad_shape() {
    let spec = taxi();
    let unknown = Ask {
        fact: id("nope"),
        ..month_ask()
    };
    assert_eq!(
        spec.check(&unknown).unwrap_err(),
        [AskError::UnknownFact(id("nope"))]
    );

    let ask = Ask {
        measures: vec![],
        filter: vec![Cond {
            path: path("fare_amount"),
            values: vec![Literal::Int(1)],
        }],
        from: FEB,
        to: JAN + 60,
        ..month_ask()
    };
    assert_eq!(
        spec.check(&ask).unwrap_err(),
        [
            AskError::NoMeasures,
            AskError::UnknownKey(path("fare_amount")),
            AskError::RangeEmpty {
                from: FEB,
                to: JAN + 60
            },
            AskError::RangeUnaligned {
                at: JAN + 60,
                grain: Grain(900)
            },
        ]
    );
}
