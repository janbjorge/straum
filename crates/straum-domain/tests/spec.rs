//! `Spec::new` on hand-built declarations, including shapes straum SQL can't
//! produce (a ref to a missing dim, a dim attribute listed twice).

use straum_domain::{
    Agg, Ask, AskError, AskKey, Attr, By, Cond, Dim, Fact, Grain, Ident, Key, Literal, Measure,
    Origin, Ref, RefKind, Spec, SpecError, Table, Time, TimeKey, Via,
};

fn id(s: &str) -> Ident {
    Ident::new(s).unwrap()
}

fn table(name: &str) -> Table {
    Table {
        schema: id("public"),
        name: id(name),
    }
}

fn dim() -> Dim {
    Dim {
        as_text: Vec::new(),
        name: id("z"),
        table: table("z"),
        key: vec![id("id")],
        attrs: vec![id("name")],
    }
}

fn fact() -> Fact {
    Fact {
        as_text: Vec::new(),
        name: id("t"),
        table: table("t"),
        time: Time {
            column: id("at"),
            grain: Grain { seconds: 3_600 },
            buckets: vec![],
            origin: Origin::Timestamptz,
        },
        refs: vec![Ref {
            name: id("r"),
            dim: id("z"),
            via: vec![Via::Fact(id("z_id"))],
            kind: RefKind::Left,
            attrs: vec![attr("name", "name")],
        }],
        by: vec![col("a"), col("z_id")],
        filter: vec![],
        measures: vec![Measure {
            name: id("n"),
            agg: Agg::Count,
            filter: vec![],
        }],
    }
}

fn col(s: &str) -> By {
    By::Column {
        column: id(s),
        name: id(s),
    }
}

fn attr(name: &str, column: &str) -> Attr {
    Attr {
        name: id(name),
        column: id(column),
    }
}

#[test]
fn which_measures_read_numbers() {
    let x = || id("x");
    let cases = [
        (Agg::Sum(x()), true),
        (Agg::Min(x()), true),
        (Agg::Max(x()), true),
        (Agg::Avg(x()), true),
        (Agg::CountDistinct(x()), false),
        (Agg::Count, false),
    ];
    for (agg, numbers) in cases {
        assert_eq!(agg.reads_numbers(), numbers, "{agg:?}");
    }
}

#[test]
fn valid() {
    assert!(Spec::new(vec![dim()], vec![fact()]).is_ok());
}

/// A name is a by key or an attribute a ref reads, in one namespace. A
/// ref has no value of its own, and reads only what it declares.
#[test]
fn names_resolve_to_by_keys_and_attributes() {
    let spec = Spec::new(vec![dim()], vec![fact()]).unwrap();
    let t = spec.fact("t").unwrap();
    assert_eq!(t.resolve(&id("a")), Some(Key::By(&t.by[0])));
    assert_eq!(t.resolve(&id("z_id")), Some(Key::By(&t.by[1])));
    assert_eq!(
        t.resolve(&id("name")),
        Some(Key::Attr(&t.refs[0], &t.refs[0].attrs[0]))
    );
    for missing in ["r", "id", "nope", "b", "n"] {
        assert_eq!(t.resolve(&id(missing)), None, "{missing}");
    }
    let keys: Vec<&Ident> = t.keys().collect();
    assert_eq!(keys, [&id("a"), &id("z_id"), &id("name")]);
}

/// A row is read from the time column, the by columns, the via columns and
/// the measure inputs, each once, and then holds the ref attributes.
#[test]
fn columns_and_rollup_key() {
    let mut t = fact();
    t.by = vec![col("a")];
    t.measures.push(Measure {
        name: id("s"),
        agg: Agg::Sum(id("z_id")),
        filter: vec![],
    });
    assert_eq!(t.columns(), [id("at"), id("a"), id("z_id")]);
    let attrs: Vec<String> = (t.attrs())
        .map(|(r, a)| format!("{}.{} AS {}", r.name, a.column, a.name))
        .collect();
    assert_eq!(attrs, ["r.name AS name"]);
    assert_eq!(t.rollup_key().count(), 3);
}

#[test]
fn tables_once_each_dims_first() {
    let mut zones = dim();
    zones.name = id("zones");
    // A fact over the dim's own table, as a log of the dim's rows.
    let mut log = fact();
    log.name = id("log");
    log.table = table("z");
    let mut other = dim();
    other.name = id("other");
    other.table.schema = id("other");
    let spec = Spec::new(vec![dim(), other, zones], vec![fact(), log]).unwrap();
    let tables: Vec<String> = (spec.tables().iter())
        .map(|t| format!("{}.{}", t.schema, t.name))
        .collect();
    assert_eq!(tables, ["public.z", "other.z", "public.t"]);
}

#[test]
fn buckets_widen_by_whole_multiples() {
    let mut t = fact();
    t.time.buckets = vec![
        Grain { seconds: 7_200 },
        Grain { seconds: 86_400 },
        Grain { seconds: 604_800 },
    ];
    assert!(Spec::new(vec![dim()], vec![t.clone()]).is_ok());

    // 1.5 hours over an hour, then a day that doesn't widen the day.
    t.time.buckets = vec![
        Grain { seconds: 5_400 },
        Grain { seconds: 86_400 },
        Grain { seconds: 86_400 },
    ];
    let errors = Spec::new(vec![dim()], vec![t]).unwrap_err();
    let shown: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert_eq!(
        shown,
        [
            "fact t: bucket of 5400s is not a wider multiple of 3600s",
            "fact t: bucket of 86400s is not a wider multiple of 86400s",
        ]
    );
}

#[test]
fn every_error_is_reported_and_displayed() {
    let mut z = dim();
    z.attrs = vec![id("name"), id("name")];
    let mut w = dim();
    w.name = id("w");
    w.key = vec![];
    let mut t = fact();
    t.name = id("z");
    t.time.grain = Grain { seconds: 420 };
    t.by = vec![
        col("id"),
        col("at"),
        col("name"),
        col("id"),
        By::IsNull {
            column: id("at"),
            name: id("no_time"),
            negated: false,
        },
    ];
    t.refs[0].dim = id("nope");
    t.measures = vec![
        Measure {
            name: id("m"),
            agg: Agg::Count,
            filter: vec![Cond {
                key: id("c"),
                values: vec![Literal::Int(1)],
            }],
        },
        Measure {
            name: id("m"),
            agg: Agg::Count,
            filter: vec![],
        },
    ];
    // A ref on two columns to a key of one, one joining on a column twice,
    // one reading nothing, and one reading an attribute the dim lacks and
    // another twice.
    let mut u = fact();
    u.name = id("u");
    u.by = vec![col("a"), col("z_id"), col("b")];
    u.refs = vec![
        Ref {
            name: id("r"),
            dim: id("z"),
            via: vec![Via::Fact(id("z_id")), Via::Fact(id("b"))],
            kind: RefKind::Left,
            attrs: vec![attr("r_name", "name")],
        },
        Ref {
            name: id("s"),
            dim: id("z"),
            via: vec![Via::Fact(id("a")), Via::Fact(id("a"))],
            kind: RefKind::Left,
            attrs: vec![attr("s_name", "name")],
        },
        Ref {
            name: id("e"),
            dim: id("z"),
            via: vec![Via::Fact(id("a"))],
            kind: RefKind::Left,
            attrs: vec![],
        },
        Ref {
            name: id("q"),
            dim: id("z"),
            via: vec![Via::Fact(id("b"))],
            kind: RefKind::Left,
            attrs: vec![
                attr("nope", "nope"),
                attr("q_name", "name"),
                attr("a", "name"),
            ],
        },
    ];
    u.measures = vec![];

    let errors = Spec::new(vec![z, w], vec![t, u]).unwrap_err();
    let shown: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert_eq!(
        shown,
        [
            "z declared twice",
            "z: name declared twice",
            "dim w: no key columns",
            "fact z: grain of 420s does not divide a day",
            "fact z: at is the time column, so it can't be a by column too",
            "z: id declared twice",
            "fact z: no_time asks whether the time column at is null, which no ask could see: \
             a row with no time is never answered",
            "fact z: ref r points at unknown dim nope",
            "z: name declared twice",
            "fact z: measure m filters on c, which names no key of the view",
            "z: m declared twice",
            "fact u: ref r joins on 2 columns, but the key of dim z has 1",
            "fact u: ref s joins on a twice",
            "fact u: ref s joins on 2 columns, but the key of dim z has 1",
            "fact u: ref e reads no column of its dim",
            "fact u: ref q reads nope, which dim z has no column of",
            "q: name declared twice",
            "u: a declared twice",
            "fact u: no measures",
        ]
    );
    assert!(matches!(errors[7], SpecError::UnknownDim { .. }));
}

#[test]
fn a_chained_via_used_twice_names_its_ref() {
    let mut t = fact();
    let chained = Via::Attr {
        through: 0,
        column: id("name"),
    };
    t.refs.push(Ref {
        name: id("c"),
        dim: id("z"),
        via: vec![chained.clone(), chained],
        kind: RefKind::Left,
        attrs: vec![attr("c_name", "name")],
    });
    let shown: Vec<String> = (Spec::new(vec![dim()], vec![t]).unwrap_err().iter())
        .map(ToString::to_string)
        .collect();
    assert!(
        shown.contains(&"fact t: ref c joins on r.name twice".to_owned()),
        "{shown:?}"
    );
}

/// Two refs under one alias, which straum SQL refuses before the spec
/// does; the spec refuses it too, for a spec built by hand.
#[test]
fn a_ref_name_used_twice_is_refused() {
    let mut t = fact();
    let again = t.refs[0].clone();
    t.refs.push(again);
    let shown: Vec<String> = (Spec::new(vec![dim()], vec![t]).unwrap_err().iter())
        .map(ToString::to_string)
        .collect();
    assert_eq!(shown, ["t: r declared twice", "t: name declared twice"]);
}

/// One day of `t` by every key form: by keys, an attribute a ref reads
/// and the bucket.
fn ask() -> Ask {
    Ask {
        fact: id("t"),
        measures: vec![id("n")],
        by: vec![
            AskKey::Key(id("a")),
            AskKey::Key(id("z_id")),
            AskKey::Key(id("name")),
            AskKey::Time(TimeKey::Bucket),
        ],
        filter: vec![Cond {
            key: id("name"),
            values: vec![Literal::Text("x".into())],
        }],
        from: 0,
        to: 86_400,
    }
}

#[test]
fn an_ask_by_every_key_form_checks() {
    let spec = Spec::new(vec![dim()], vec![fact()]).unwrap();
    assert_eq!(spec.check(&ask()).unwrap().name, id("t"));
}

#[test]
fn an_ask_of_what_the_rollup_does_not_hold_is_rejected() {
    let spec = Spec::new(vec![dim()], vec![fact()]).unwrap();
    let unheld = Ask {
        measures: vec![id("n"), id("tips"), id("n")],
        by: vec![
            AskKey::Key(id("fare")),
            AskKey::Key(id("colour")),
            AskKey::Key(id("name")),
            AskKey::Key(id("name")),
            AskKey::Time(TimeKey::Bucket),
            AskKey::Time(TimeKey::Bucket),
        ],
        from: 60,
        ..ask()
    };
    assert_eq!(
        spec.check(&unheld).unwrap_err(),
        [
            AskError::UnknownMeasure(id("tips")),
            AskError::UnknownKey(id("fare")),
            AskError::UnknownKey(id("colour")),
            AskError::TimeKeyTwice,
            AskError::AskedTwice("name".into()),
            AskError::AskedTwice("n".into()),
            AskError::RangeUnaligned {
                at: 60,
                grain: Grain { seconds: 3_600 },
            },
        ]
    );
}

#[test]
fn an_ask_of_a_bad_shape_is_rejected() {
    let spec = Spec::new(vec![dim()], vec![fact()]).unwrap();
    let unknown = Ask {
        fact: id("nope"),
        ..ask()
    };
    assert_eq!(
        spec.check(&unknown).unwrap_err(),
        [AskError::UnknownFact(id("nope"))]
    );

    let empty = Ask {
        measures: vec![],
        filter: vec![Cond {
            key: id("fare"),
            values: vec![Literal::Int(1)],
        }],
        from: 86_400,
        to: 60,
        ..ask()
    };
    assert_eq!(
        spec.check(&empty).unwrap_err(),
        [
            AskError::NoMeasures,
            AskError::UnknownKey(id("fare")),
            AskError::RangeEmpty {
                from: 86_400,
                to: 60,
            },
            AskError::RangeUnaligned {
                at: 60,
                grain: Grain { seconds: 3_600 },
            },
        ]
    );
}

#[test]
fn ask_errors_display() {
    let cases = [
        (AskError::UnknownFact(id("nope")), "unknown fact nope"),
        (AskError::UnknownMeasure(id("tips")), "unknown measure tips"),
        (
            AskError::UnknownKey(id("fare")),
            "fare is not a key of the view",
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
                grain: Grain { seconds: 900 },
            },
            "range end 60 is not on a 900s bucket edge",
        ),
    ];
    for (error, shown) in cases {
        assert_eq!(error.to_string(), shown);
    }
    assert_eq!(TimeKey::Bucket.to_string(), "bucket");
}
