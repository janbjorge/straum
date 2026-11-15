//! `Spec::new` on hand-built declarations, including shapes Straum SQL can't
//! produce (a ref to a missing dim, a dim attribute listed twice).

use straum_domain::{
    Aggregate, Ask, AskError, AskKey, ByKey, Condition, Dim, Fact, Fraction, Identifier, Key,
    Literal, Measure, Origin, Reads, Ref, RefKind, Spec, SpecError, Time, Via, Width,
};

mod common;

use common::{attribute, col, id, percentile, table};

fn dim() -> Dim {
    Dim {
        as_text: Vec::new(),
        name: id("z"),
        table: table("z"),
        key: vec![id("id")],
        attributes: vec![id("name")],
    }
}

fn fact() -> Fact {
    Fact {
        as_text: Vec::new(),
        name: id("t"),
        table: table("t"),
        time: Time {
            column: id("at"),
            width: Width { seconds: 3_600 },
            wider: vec![],
            origin: Origin::Timestamptz,
        },
        refs: vec![Ref {
            alias: id("r"),
            dim: id("z"),
            via: vec![Via::Fact(id("z_id"))],
            kind: RefKind::Left,
            reads: vec![attribute("name", "name")],
        }],
        by_keys: vec![col("a"), col("z_id")],
        filter: vec![],
        measures: vec![Measure {
            name: id("n"),
            aggregate: Aggregate::Count,
            filter: vec![],
        }],
    }
}

#[test]
fn what_each_measure_reads() {
    let cases = [
        (Aggregate::Sum(id("x")), Some(Reads::Numbers)),
        (Aggregate::Min(id("x")), Some(Reads::Numbers)),
        (Aggregate::Max(id("x")), Some(Reads::Numbers)),
        (percentile(5, 1, "x"), Some(Reads::Numbers)),
        (Aggregate::Avg(id("x")), Some(Reads::Numbers)),
        (Aggregate::CountDistinct(id("x")), Some(Reads::Values)),
        (Aggregate::CountNotNull(id("x")), Some(Reads::Nullness)),
        (Aggregate::BoolAnd(id("x")), Some(Reads::Booleans)),
        (Aggregate::BoolOr(id("x")), Some(Reads::Booleans)),
        (Aggregate::Count, None),
    ];
    for (aggregate, reads) in cases {
        assert_eq!(aggregate.reads(), reads, "{aggregate:?}");
    }
}

/// A fraction is from 0 to 1, however many places it is written in.
#[test]
fn a_fraction_is_at_most_one() {
    assert!(Fraction::new(0, 0).is_some());
    assert!(Fraction::new(1, 0).is_some());
    assert!(Fraction::new(1000, 3).is_some());
    assert!(Fraction::new(99, 2).is_some());
    assert!(Fraction::new(1001, 3).is_none());
    assert!(Fraction::new(2, 0).is_none());
    assert!(Fraction::new(1, 20).is_none());
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
    let view = spec.fact("t").unwrap();
    assert_eq!(view.resolve(&id("a")), Some(Key::ByKey(&view.by_keys[0])));
    assert_eq!(
        view.resolve(&id("z_id")),
        Some(Key::ByKey(&view.by_keys[1]))
    );
    assert_eq!(
        view.resolve(&id("name")),
        Some(Key::Attribute(&view.refs[0], &view.refs[0].reads[0]))
    );
    for missing in ["r", "id", "nope", "b", "n"] {
        assert_eq!(view.resolve(&id(missing)), None, "{missing}");
    }
    let keys: Vec<&Identifier> = view.keys().collect();
    assert_eq!(keys, [&id("a"), &id("z_id"), &id("name")]);
}

/// A row is read from the time column, the by columns, the via columns and
/// the measure inputs, each once, and then holds the ref attributes.
#[test]
fn columns_and_rollup_key() {
    let mut view = fact();
    view.by_keys = vec![col("a")];
    view.measures.push(Measure {
        name: id("s"),
        aggregate: Aggregate::Sum(id("z_id")),
        filter: vec![],
    });
    assert_eq!(view.columns(), [id("at"), id("a"), id("z_id")]);
    let attributes: Vec<String> = (view.attributes())
        .map(|(join, attribute)| {
            format!("{}.{} AS {}", join.alias, attribute.column, attribute.name)
        })
        .collect();
    assert_eq!(attributes, ["r.name AS name"]);
    assert_eq!(view.rollup_key().count(), 3);
}

/// A via column adds a part to the rollup key unless a `by` key reads the
/// column itself; whether the column is null is another part.
#[test]
fn a_via_column_adds_a_part_unless_a_by_key_reads_it() {
    let mut view = fact();
    assert!(!view.vias_add_parts());
    view.by_keys = vec![col("a")];
    assert!(view.vias_add_parts());
    view.by_keys.push(ByKey::IsNull {
        column: id("z_id"),
        name: id("unzoned"),
        negated: false,
    });
    assert!(view.vias_add_parts());
    view.refs.clear();
    assert!(!view.vias_add_parts());
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
        .map(|table| format!("{}.{}", table.schema, table.name))
        .collect();
    assert_eq!(tables, ["public.z", "other.z", "public.t"]);
}

#[test]
fn buckets_widen_by_whole_multiples() {
    let mut view = fact();
    view.time.wider = vec![
        Width { seconds: 7_200 },
        Width { seconds: 86_400 },
        Width { seconds: 604_800 },
    ];
    assert!(Spec::new(vec![dim()], vec![view.clone()]).is_ok());

    // 1.5 hours over an hour, then a day that doesn't widen the day.
    view.time.wider = vec![
        Width { seconds: 5_400 },
        Width { seconds: 86_400 },
        Width { seconds: 86_400 },
    ];
    let errors = Spec::new(vec![dim()], vec![view]).unwrap_err();
    let shown: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert_eq!(
        shown,
        [
            "view t: bucket of 5400s is not a wider multiple of 3600s",
            "view t: bucket of 86400s is not a wider multiple of 86400s",
        ]
    );
}

/// A view named as a dim, with a width that doesn't divide a day, by keys
/// twice and on the time column, a ref to no dim and a measure twice.
fn bad_view() -> Fact {
    let mut view = fact();
    view.name = id("z");
    view.time.width = Width { seconds: 420 };
    view.by_keys = vec![
        col("id"),
        col("at"),
        col("name"),
        col("id"),
        ByKey::IsNull {
            column: id("at"),
            name: id("no_time"),
            negated: false,
        },
    ];
    view.refs[0].dim = id("nope");
    view.measures = vec![
        Measure {
            name: id("m"),
            aggregate: Aggregate::Count,
            filter: vec![Condition {
                key: id("c"),
                values: vec![Literal::Int(1)],
            }],
        },
        Measure {
            name: id("m"),
            aggregate: Aggregate::Count,
            filter: vec![],
        },
    ];
    view
}

/// A ref on two columns to a key of one, one joining on a column twice,
/// one reading nothing, and one reading an attribute the dim lacks and
/// another twice.
fn bad_joins() -> Fact {
    let mut joins = fact();
    joins.name = id("u");
    joins.by_keys = vec![col("a"), col("z_id"), col("b")];
    joins.refs = vec![
        Ref {
            alias: id("r"),
            dim: id("z"),
            via: vec![Via::Fact(id("z_id")), Via::Fact(id("b"))],
            kind: RefKind::Left,
            reads: vec![attribute("r_name", "name")],
        },
        Ref {
            alias: id("s"),
            dim: id("z"),
            via: vec![Via::Fact(id("a")), Via::Fact(id("a"))],
            kind: RefKind::Left,
            reads: vec![attribute("s_name", "name")],
        },
        Ref {
            alias: id("e"),
            dim: id("z"),
            via: vec![Via::Fact(id("a"))],
            kind: RefKind::Left,
            reads: vec![],
        },
        Ref {
            alias: id("q"),
            dim: id("z"),
            via: vec![Via::Fact(id("b"))],
            kind: RefKind::Left,
            reads: vec![
                attribute("nope", "nope"),
                attribute("q_name", "name"),
                attribute("a", "name"),
            ],
        },
    ];
    joins.measures = vec![];
    joins
}

#[test]
fn every_error_is_reported_and_displayed() {
    let mut twice = dim();
    twice.attributes = vec![id("name"), id("name")];
    let mut keyless = dim();
    keyless.name = id("w");
    keyless.key = vec![];

    let errors = Spec::new(vec![twice, keyless], vec![bad_view(), bad_joins()]).unwrap_err();
    let shown: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert_eq!(
        shown,
        [
            "z declared twice",
            "z: name declared twice",
            "dim w: no key columns",
            "view z: width of 420s does not divide a day",
            "view z: at is the time column, so it can't be a by column too",
            "z: id declared twice",
            "view z: no_time asks whether the time column at is null, which no ask could see: \
             a row with no time is never answered",
            "view z: join r points at unknown dim nope",
            "z: name declared twice",
            "view z: measure m filters on c, which names no key of the view",
            "z: m declared twice",
            "view u: join r joins on 2 columns, but the key of dim z has 1",
            "view u: join s joins on a twice",
            "view u: join s joins on 2 columns, but the key of dim z has 1",
            "view u: join e reads no column of its dim",
            "view u: join q reads nope, which dim z has no column of",
            "q: name declared twice",
            "u: a declared twice",
            "view u: no measures",
        ]
    );
    assert!(matches!(errors[7], SpecError::UnknownDim { .. }));
}

#[test]
fn a_chained_via_used_twice_names_its_ref() {
    let mut view = fact();
    let chained = Via::Attribute {
        through: 0,
        column: id("name"),
    };
    view.refs.push(Ref {
        alias: id("c"),
        dim: id("z"),
        via: vec![chained.clone(), chained],
        kind: RefKind::Left,
        reads: vec![attribute("c_name", "name")],
    });
    let shown: Vec<String> = (Spec::new(vec![dim()], vec![view]).unwrap_err().iter())
        .map(ToString::to_string)
        .collect();
    assert!(
        shown.contains(&"view t: join c joins on r.name twice".to_owned()),
        "{shown:?}"
    );
}

/// Two refs under one alias, which Straum SQL refuses before the spec
/// does; the spec refuses it too, for a spec built by hand.
#[test]
fn a_ref_name_used_twice_is_refused() {
    let mut view = fact();
    let again = view.refs[0].clone();
    view.refs.push(again);
    let shown: Vec<String> = (Spec::new(vec![dim()], vec![view]).unwrap_err().iter())
        .map(ToString::to_string)
        .collect();
    assert_eq!(shown, ["t: r declared twice", "t: name declared twice"]);
}

/// A semi join that reads nothing, a join through a column the dim
/// before it carries, and a `WHERE` on a key.
fn chained() -> Fact {
    let mut view = fact();
    view.refs.push(Ref {
        alias: id("through"),
        dim: id("z"),
        via: vec![Via::Attribute {
            through: 0,
            column: id("name"),
        }],
        kind: RefKind::Semi,
        reads: vec![],
    });
    view.filter = vec![Condition {
        key: id("name"),
        values: vec![Literal::Text("x".into())],
    }];
    view
}

/// The errors of [`chained`] once `change` is made to it.
fn refused(change: &dyn Fn(&mut Fact)) -> Vec<String> {
    let mut view = chained();
    change(&mut view);
    let errors = Spec::new(vec![dim()], vec![view]).unwrap_err();
    errors.iter().map(ToString::to_string).collect()
}

/// A change that joins the second ref through `column` of join `through`.
fn through(through: usize, column: &str) -> impl Fn(&mut Fact) {
    move |view: &mut Fact| {
        view.refs[1].via = vec![Via::Attribute {
            through,
            column: id(column),
        }];
    }
}

/// What Straum SQL refuses in its own words before the spec sees it: a
/// semi or anti join that reads a column, a join through a column no
/// join before it carries, and a `WHERE` on something that is no key.
#[test]
fn joins_and_filters_straum_sql_cannot_write_are_refused() {
    assert!(Spec::new(vec![dim()], vec![chained()]).is_ok());

    let reads = vec![attribute("through_name", "name")];
    assert_eq!(
        refused(&|view| view.refs[1].reads.clone_from(&reads)),
        ["view t: join through is a semi join and reads columns, which it has none of"]
    );
    assert_eq!(
        refused(&|view| {
            view.refs[1].kind = RefKind::Anti;
            view.refs[1].reads.clone_from(&reads);
        }),
        ["view t: join through is an anti join and reads columns, which it has none of"]
    );
    // A column the dim before it doesn't carry, then the join itself
    // and one that isn't there.
    assert_eq!(
        refused(&through(0, "id")),
        ["view t: join through joins through column id of join 0, which no join before it carries"]
    );
    for not_before in [1, 7] {
        assert_eq!(
            refused(&through(not_before, "name")),
            [format!(
                "view t: join through joins through column name of join {not_before}, \
                 which no join before it carries"
            )]
        );
    }
    assert_eq!(
        refused(&|view| view.filter[0].key = id("n")),
        ["view t: WHERE filters on n, which names no key of the view"]
    );
}

/// One day of `t` by every key form: by keys, an attribute a ref reads
/// and the bucket.
fn ask() -> Ask {
    Ask {
        view: id("t"),
        measures: vec![id("n")],
        keys: vec![
            AskKey::Key(id("a")),
            AskKey::Key(id("z_id")),
            AskKey::Key(id("name")),
            AskKey::Bucket,
        ],
        filter: vec![Condition {
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
    assert_eq!(spec.check_ask(&ask()).unwrap().name, id("t"));
}

#[test]
fn an_ask_of_what_the_rollup_does_not_hold_is_rejected() {
    let spec = Spec::new(vec![dim()], vec![fact()]).unwrap();
    let unheld = Ask {
        measures: vec![id("n"), id("tips"), id("n")],
        keys: vec![
            AskKey::Key(id("fare")),
            AskKey::Key(id("colour")),
            AskKey::Key(id("name")),
            AskKey::Key(id("name")),
            AskKey::Bucket,
            AskKey::Bucket,
        ],
        from: 60,
        ..ask()
    };
    assert_eq!(
        spec.check_ask(&unheld).unwrap_err(),
        [
            AskError::UnknownMeasure(id("tips")),
            AskError::UnknownKey(id("fare")),
            AskError::UnknownKey(id("colour")),
            AskError::TimeKeyTwice,
            AskError::AskedTwice("name".into()),
            AskError::AskedTwice("n".into()),
            AskError::RangeUnaligned {
                at: 60,
                width: Width { seconds: 3_600 },
            },
        ]
    );
}

#[test]
fn an_ask_of_a_bad_shape_is_rejected() {
    let spec = Spec::new(vec![dim()], vec![fact()]).unwrap();
    let unknown = Ask {
        view: id("nope"),
        ..ask()
    };
    assert_eq!(
        spec.check_ask(&unknown).unwrap_err(),
        [AskError::UnknownView(id("nope"))]
    );

    let empty = Ask {
        measures: vec![],
        filter: vec![Condition {
            key: id("fare"),
            values: vec![Literal::Int(1)],
        }],
        from: 86_400,
        to: 60,
        ..ask()
    };
    assert_eq!(
        spec.check_ask(&empty).unwrap_err(),
        [
            AskError::NoMeasures,
            AskError::UnknownKey(id("fare")),
            AskError::RangeEmpty {
                from: 86_400,
                to: 60,
            },
            AskError::RangeUnaligned {
                at: 60,
                width: Width { seconds: 3_600 },
            },
        ]
    );

    // One bucket is the narrowest range; none is empty.
    let narrowest = Ask {
        from: 3_600,
        to: 7_200,
        ..ask()
    };
    assert!(spec.check_ask(&narrowest).is_ok());
    let none = Ask {
        to: 3_600,
        ..narrowest
    };
    assert_eq!(
        spec.check_ask(&none).unwrap_err(),
        [AskError::RangeEmpty {
            from: 3_600,
            to: 3_600,
        }]
    );
}

#[test]
fn ask_errors_display() {
    let cases = [
        (AskError::UnknownView(id("nope")), "unknown view nope"),
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
                width: Width { seconds: 900 },
            },
            "bound 60 is not on a 900s bucket edge",
        ),
    ];
    for (error, shown) in cases {
        assert_eq!(error.to_string(), shown);
    }
    assert_eq!(AskKey::Bucket.to_string(), "bucket");
}
