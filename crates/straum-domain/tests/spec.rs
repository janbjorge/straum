//! `Spec::new` on hand-built declarations, including shapes Straum SQL can't
//! produce (a ref to a missing dim, a dim attribute listed twice).

use straum_domain::{
    Aggregate, Ask, AskError, AskKey, ByKey, Condition, Dim, Fact, Identifier, Key, Literal,
    Measure, Origin, Reads, Ref, RefKind, Spec, SpecError, Time, TimeKey, Via, Width,
};

mod common;

use common::{attr, col, id, table};

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
            reads: vec![attr("name", "name")],
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
    let x = || id("x");
    let cases = [
        (Aggregate::Sum(x()), Some(Reads::Numbers)),
        (Aggregate::Min(x()), Some(Reads::Numbers)),
        (Aggregate::Max(x()), Some(Reads::Numbers)),
        (Aggregate::Avg(x()), Some(Reads::Numbers)),
        (Aggregate::CountDistinct(x()), Some(Reads::Values)),
        (Aggregate::CountNotNull(x()), Some(Reads::Nullness)),
        (Aggregate::BoolAnd(x()), Some(Reads::Booleans)),
        (Aggregate::BoolOr(x()), Some(Reads::Booleans)),
        (Aggregate::Count, None),
    ];
    for (aggregate, reads) in cases {
        assert_eq!(aggregate.reads(), reads, "{aggregate:?}");
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
    assert_eq!(t.resolve(&id("a")), Some(Key::ByKey(&t.by_keys[0])));
    assert_eq!(t.resolve(&id("z_id")), Some(Key::ByKey(&t.by_keys[1])));
    assert_eq!(
        t.resolve(&id("name")),
        Some(Key::Attribute(&t.refs[0], &t.refs[0].reads[0]))
    );
    for missing in ["r", "id", "nope", "b", "n"] {
        assert_eq!(t.resolve(&id(missing)), None, "{missing}");
    }
    let keys: Vec<&Identifier> = t.keys().collect();
    assert_eq!(keys, [&id("a"), &id("z_id"), &id("name")]);
}

/// A row is read from the time column, the by columns, the via columns and
/// the measure inputs, each once, and then holds the ref attributes.
#[test]
fn columns_and_rollup_key() {
    let mut t = fact();
    t.by_keys = vec![col("a")];
    t.measures.push(Measure {
        name: id("s"),
        aggregate: Aggregate::Sum(id("z_id")),
        filter: vec![],
    });
    assert_eq!(t.columns(), [id("at"), id("a"), id("z_id")]);
    let attrs: Vec<String> = (t.attributes())
        .map(|(r, a)| format!("{}.{} AS {}", r.alias, a.column, a.name))
        .collect();
    assert_eq!(attrs, ["r.name AS name"]);
    assert_eq!(t.rollup_key().count(), 3);
}

/// A via column adds a part to the rollup key unless a `by` key reads the
/// column itself; whether the column is null is another part.
#[test]
fn a_via_column_adds_a_part_unless_a_by_key_reads_it() {
    let mut t = fact();
    assert!(!t.vias_add_parts());
    t.by_keys = vec![col("a")];
    assert!(t.vias_add_parts());
    t.by_keys.push(ByKey::IsNull {
        column: id("z_id"),
        name: id("unzoned"),
        negated: false,
    });
    assert!(t.vias_add_parts());
    t.refs.clear();
    assert!(!t.vias_add_parts());
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
    t.time.wider = vec![
        Width { seconds: 7_200 },
        Width { seconds: 86_400 },
        Width { seconds: 604_800 },
    ];
    assert!(Spec::new(vec![dim()], vec![t.clone()]).is_ok());

    // 1.5 hours over an hour, then a day that doesn't widen the day.
    t.time.wider = vec![
        Width { seconds: 5_400 },
        Width { seconds: 86_400 },
        Width { seconds: 86_400 },
    ];
    let errors = Spec::new(vec![dim()], vec![t]).unwrap_err();
    let shown: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert_eq!(
        shown,
        [
            "view t: bucket of 5400s is not a wider multiple of 3600s",
            "view t: bucket of 86400s is not a wider multiple of 86400s",
        ]
    );
}

#[test]
fn every_error_is_reported_and_displayed() {
    let mut z = dim();
    z.attributes = vec![id("name"), id("name")];
    let mut w = dim();
    w.name = id("w");
    w.key = vec![];
    let mut t = fact();
    t.name = id("z");
    t.time.width = Width { seconds: 420 };
    t.by_keys = vec![
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
    t.refs[0].dim = id("nope");
    t.measures = vec![
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
    // A ref on two columns to a key of one, one joining on a column twice,
    // one reading nothing, and one reading an attribute the dim lacks and
    // another twice.
    let mut u = fact();
    u.name = id("u");
    u.by_keys = vec![col("a"), col("z_id"), col("b")];
    u.refs = vec![
        Ref {
            alias: id("r"),
            dim: id("z"),
            via: vec![Via::Fact(id("z_id")), Via::Fact(id("b"))],
            kind: RefKind::Left,
            reads: vec![attr("r_name", "name")],
        },
        Ref {
            alias: id("s"),
            dim: id("z"),
            via: vec![Via::Fact(id("a")), Via::Fact(id("a"))],
            kind: RefKind::Left,
            reads: vec![attr("s_name", "name")],
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
    let mut t = fact();
    let chained = Via::Attribute {
        through: 0,
        column: id("name"),
    };
    t.refs.push(Ref {
        alias: id("c"),
        dim: id("z"),
        via: vec![chained.clone(), chained],
        kind: RefKind::Left,
        reads: vec![attr("c_name", "name")],
    });
    let shown: Vec<String> = (Spec::new(vec![dim()], vec![t]).unwrap_err().iter())
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
    let mut t = fact();
    let again = t.refs[0].clone();
    t.refs.push(again);
    let shown: Vec<String> = (Spec::new(vec![dim()], vec![t]).unwrap_err().iter())
        .map(ToString::to_string)
        .collect();
    assert_eq!(shown, ["t: r declared twice", "t: name declared twice"]);
}

/// A semi join that reads nothing, a join through a column the dim
/// before it carries, and a `WHERE` on a key.
fn chained() -> Fact {
    let mut t = fact();
    t.refs.push(Ref {
        alias: id("through"),
        dim: id("z"),
        via: vec![Via::Attribute {
            through: 0,
            column: id("name"),
        }],
        kind: RefKind::Semi,
        reads: vec![],
    });
    t.filter = vec![Condition {
        key: id("name"),
        values: vec![Literal::Text("x".into())],
    }];
    t
}

/// What Straum SQL refuses in its own words before the spec sees it: a
/// semi or anti join that reads a column, a join through a column no
/// join before it carries, and a `WHERE` on something that is no key.
#[test]
fn joins_and_filters_straum_sql_cannot_write_are_refused() {
    assert!(Spec::new(vec![dim()], vec![chained()]).is_ok());

    let refused = |change: &dyn Fn(&mut Fact)| -> Vec<String> {
        let mut t = chained();
        change(&mut t);
        let errors = Spec::new(vec![dim()], vec![t]).unwrap_err();
        errors.iter().map(ToString::to_string).collect()
    };
    let reads = vec![attr("through_name", "name")];
    assert_eq!(
        refused(&|t| t.refs[1].reads.clone_from(&reads)),
        ["view t: join through is a semi join and reads columns, which it has none of"]
    );
    assert_eq!(
        refused(&|t| {
            t.refs[1].kind = RefKind::Anti;
            t.refs[1].reads.clone_from(&reads);
        }),
        ["view t: join through is an anti join and reads columns, which it has none of"]
    );
    let through = |through, column| {
        move |t: &mut Fact| {
            t.refs[1].via = vec![Via::Attribute {
                through,
                column: id(column),
            }];
        }
    };
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
        refused(&|t| t.filter[0].key = id("n")),
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
            AskKey::Time(TimeKey::Bucket),
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
            AskKey::Time(TimeKey::Bucket),
            AskKey::Time(TimeKey::Bucket),
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
    assert_eq!(TimeKey::Bucket.to_string(), "bucket");
}
