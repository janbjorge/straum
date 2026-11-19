//! `Spec::new` on hand-built declarations, including shapes Straum SQL can't
//! produce (a ref to a missing dim, a dim attribute listed twice), and
//! `Spec::check_ask` on asks of one view: each check, the error it gives
//! and the text that error shows.

use straum_domain::{
    Aggregate, Ask, AskError, AskKey, ByKey, Condition, Dim, Fact, Fraction, Instead, JoinError,
    Key, Literal, Measure, Origin, Reads, Ref, RefKind, Spec, SpecError, Time, Via, Width,
};

mod common;

const DAY: u32 = 86_400;
const WEEK: u32 = 604_800;
/// [`DAY`] and [`WEEK`] as bounds, in Unix seconds.
const DAY_START: i64 = 86_400;
const WEEK_START: i64 = 604_800;

use common::{attribute, by_key, id, percentile, table};

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
        by_keys: vec![by_key("a"), by_key("z_id")],
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
        (Aggregate::VarSamp(id("x")), Some(Reads::Numbers)),
        (Aggregate::StddevPop(id("x")), Some(Reads::Numbers)),
        (Aggregate::Mode(id("x")), Some(Reads::Numbers)),
        // The state is count(DISTINCT x)'s, which tells any values apart;
        // a sum of them takes numbers.
        (Aggregate::SumDistinct(id("x")), Some(Reads::Numbers)),
        (Aggregate::AvgDistinct(id("x")), Some(Reads::Numbers)),
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
    let keys = view.keys();
    assert_eq!(keys, [&id("a"), &id("z_id"), &id("name")]);
}

/// A row is read from the time column, the by columns, the via columns and
/// the measure inputs, each once, and then holds the ref attributes.
#[test]
fn columns_and_rollup_key() {
    let mut view = fact();
    view.by_keys = vec![by_key("a")];
    view.measures.push(Measure {
        name: id("s"),
        aggregate: Aggregate::Sum(id("z_id")),
        filter: vec![],
    });
    assert_eq!(view.columns(), [id("at"), id("a"), id("z_id")]);
    let mut attributes = Vec::new();
    for (join, attribute) in view.attributes() {
        attributes.push(format!(
            "{}.{} AS {}",
            join.alias, attribute.column, attribute.name
        ));
    }
    assert_eq!(attributes, ["r.name AS name"]);
    assert_eq!(view.rollup_key().len(), 3);
}

/// A via column adds a part to the rollup key unless a `by` key reads the
/// column itself; whether the column is null is another part.
#[test]
fn a_via_column_adds_a_part_unless_a_by_key_reads_it() {
    let mut view = fact();
    assert!(!view.vias_add_parts());
    view.by_keys = vec![by_key("a")];
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
    let mut tables = Vec::new();
    for table in spec.tables() {
        tables.push(format!("{}.{}", table.schema, table.name));
    }
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
            "view t: bucket of 90 minutes is not a wider multiple of 1 hour",
            "view t: bucket of 1 day is not a wider multiple of 1 day",
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
        by_key("id"),
        by_key("at"),
        by_key("name"),
        by_key("id"),
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
    joins.by_keys = vec![by_key("a"), by_key("z_id"), by_key("b")];
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
            "z declared twice: views and dims share one namespace, and a dim goes by its \
             table's name without the schema",
            "z: name declared twice",
            "dim w: no key columns",
            "view z: width of 7 minutes does not divide a day",
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
            "view u: join e reads no column of its dim; a key column reads the column it is \
             joined to, so select another",
            "view u: join q reads nope, which dim z has no column of",
            // q reads name twice, which is fine; it names the second `a`,
            // a key the view has.
            "u: a declared twice",
            "view u: no measures",
        ]
    );
    assert!(matches!(
        errors[7],
        SpecError::Join {
            error: JoinError::UnknownDim { .. },
            ..
        }
    ));
}

/// The errors of a spec whose ref `c` joins twice through `column` of
/// join `through`.
fn joined_twice_through(through: usize, column: &str) -> Vec<String> {
    let mut view = fact();
    let chained = Via::Attribute {
        through,
        column: id(column),
    };
    view.refs.push(Ref {
        alias: id("c"),
        dim: id("z"),
        via: vec![chained.clone(), chained],
        kind: RefKind::Left,
        reads: vec![attribute("c_name", "name")],
    });
    refused(view)
}

#[test]
fn a_chained_via_used_twice_names_its_ref() {
    let shown = joined_twice_through(0, "name");
    assert!(
        shown.contains(&"view t: join c joins on r.name twice".to_owned()),
        "{shown:?}"
    );
}

/// A join through a ref the view has not got: the message can't name
/// that ref, and puts a question mark in its place.
#[test]
fn a_chained_via_through_no_ref_is_named_with_a_question_mark() {
    let shown = joined_twice_through(9, "name");
    assert!(
        shown.contains(&"view t: join c joins on ?.name twice".to_owned()),
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

/// The errors of a spec of the dim and `view`.
fn refused(view: Fact) -> Vec<String> {
    let errors = Spec::new(vec![dim()], vec![view]).unwrap_err();
    errors.iter().map(ToString::to_string).collect()
}

/// [`chained`], its second ref joined through `column` of join `through`.
fn chained_through(through: usize, column: &str) -> Fact {
    let mut view = chained();
    view.refs[1].via = vec![Via::Attribute {
        through,
        column: id(column),
    }];
    view
}

/// What Straum SQL refuses in its own words before the spec sees it: a
/// semi or anti join that reads a column, a join through a column no
/// join before it carries, and a `WHERE` on something that is no key.
#[test]
fn joins_and_filters_straum_sql_cannot_write_are_refused() {
    assert!(Spec::new(vec![dim()], vec![chained()]).is_ok());

    let reads = vec![attribute("through_name", "name")];
    let mut semi_reading = chained();
    semi_reading.refs[1].reads.clone_from(&reads);
    assert_eq!(
        refused(semi_reading),
        ["view t: join through is a semi join and reads columns, which it has none of"]
    );
    let mut anti_reading = chained();
    anti_reading.refs[1].kind = RefKind::Anti;
    anti_reading.refs[1].reads.clone_from(&reads);
    assert_eq!(
        refused(anti_reading),
        ["view t: join through is an anti join and reads columns, which it has none of"]
    );
    // A column the dim before it doesn't carry, then the join itself
    // and one that isn't there.
    assert_eq!(
        refused(chained_through(0, "id")),
        ["view t: join through joins through column id of join 0, which no join before it carries"]
    );
    for not_before in [1, 7] {
        assert_eq!(
            refused(chained_through(not_before, "name")),
            [format!(
                "view t: join through joins through column name of join {not_before}, \
                 which no join before it carries"
            )]
        );
    }
    let mut unknown_filter = chained();
    unknown_filter.filter[0].key = id("n");
    assert_eq!(
        refused(unknown_filter),
        ["view t: WHERE filters on n, which names no key of the view"]
    );
}

/// Two shapes no Straum SQL writes and the store can't answer: a ref that
/// reads its dim's key column, of which the projection holds no value,
/// and a chain through a semi join, which keeps no row to read through.
#[test]
fn a_key_read_and_a_chain_through_exists_are_refused() {
    let mut key_read = fact();
    key_read.refs[0].reads = vec![attribute("name", "name"), attribute("zid", "id")];
    assert_eq!(
        refused(key_read),
        ["view t: join r reads key column id, which the column it joins on holds"]
    );

    let mut through_semi = chained();
    through_semi.refs.push(Ref {
        alias: id("far"),
        dim: id("z"),
        via: vec![Via::Attribute {
            through: 1,
            column: id("name"),
        }],
        kind: RefKind::Left,
        reads: vec![attribute("far_name", "name")],
    });
    assert_eq!(
        refused(through_semi.clone()),
        ["view t: join far joins through join through, which is a semi join and keeps no row"]
    );
    // Through the left join before it, the same chain stands.
    through_semi.refs[2].via = vec![Via::Attribute {
        through: 0,
        column: id("name"),
    }];
    assert!(Spec::new(vec![dim()], vec![through_semi]).is_ok());
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
        having: Vec::new(),
        from: Some(0),
        to: Some(86_400),
    }
}

#[test]
fn an_ask_by_every_key_form_checks() {
    let spec = Spec::new(vec![dim()], vec![fact()]).unwrap();
    assert_eq!(spec.check_ask(&ask()).unwrap().name, id("t"));
}

/// `t` keeping days and weeks beside its hours.
fn days_and_weeks() -> Spec {
    let mut kept = fact();
    kept.time.wider = vec![Width { seconds: DAY }, Width { seconds: WEEK }];
    Spec::new(vec![dim()], vec![kept]).unwrap()
}

/// [`ask`] by the bucket at `width`, over `[from, to)`.
fn rows_of(width: u32, from: i64, to: i64) -> Ask {
    let mut keys = ask().keys;
    keys.pop();
    keys.push(AskKey::WiderBucket(Width { seconds: width }));
    Ask {
        keys,
        from: Some(from),
        to: Some(to),
        ..ask()
    }
}

#[test]
fn rows_at_a_kept_width_on_its_edges_check() {
    let spec = days_and_weeks();
    // 1970-01-01 was a Thursday, so a week from the epoch is a whole one.
    for (width, from, to) in [(DAY, DAY_START, 3 * DAY_START), (WEEK, 0, 2 * WEEK_START)] {
        let asked = rows_of(width, from, to);
        assert_eq!(spec.check_ask(&asked).unwrap().name, id("t"), "{width}");
    }
    // Open ends need no edge.
    let open = Ask {
        from: None,
        to: None,
        ..rows_of(WEEK, 0, WEEK_START)
    };
    assert!(spec.check_ask(&open).is_ok());
}

#[test]
fn rows_at_a_width_not_kept_or_the_date_bin_width_are_refused() {
    let spec = days_and_weeks();
    let two_days = rows_of(2 * DAY, 0, 2 * DAY_START);
    assert_eq!(
        spec.check_ask(&two_days).unwrap_err(),
        [AskError::WidthNotKept {
            width: Width { seconds: 2 * DAY },
            kept: vec![
                Width { seconds: 3_600 },
                Width { seconds: DAY },
                Width { seconds: WEEK },
            ],
        }]
    );
    // One spelling: the view's own width is plain `bucket`.
    let hours = rows_of(3_600, 0, DAY_START);
    assert_eq!(
        spec.check_ask(&hours).unwrap_err(),
        [AskError::DateBinOwnWidth(Width { seconds: 3_600 })]
    );
    // A view that keeps no wider width keeps none to ask by.
    let only_hours = Spec::new(vec![dim()], vec![fact()]).unwrap();
    assert_eq!(
        only_hours
            .check_ask(&rows_of(DAY, 0, DAY_START))
            .unwrap_err(),
        [AskError::WidthNotKept {
            width: Width { seconds: DAY },
            kept: vec![Width { seconds: 3_600 }],
        }]
    );
    let twice = Ask {
        keys: vec![AskKey::Bucket, AskKey::WiderBucket(Width { seconds: DAY })],
        ..rows_of(DAY, 0, DAY_START)
    };
    assert_eq!(
        spec.check_ask(&twice).unwrap_err(),
        [AskError::TimeKeyTwice]
    );
}

#[test]
fn rows_wider_than_a_bound_s_bucket_are_refused() {
    let spec = days_and_weeks();
    // On an hour edge, but inside a day: the first row would be part of
    // a day.
    let mid_day = rows_of(DAY, 3_600, 2 * DAY_START);
    assert_eq!(
        spec.check_ask(&mid_day).unwrap_err(),
        [AskError::RangeUnaligned {
            at: 3_600,
            width: Width { seconds: DAY },
        }]
    );
    // A Monday is a day edge, not a week edge.
    let monday = 4 * DAY_START;
    let from_monday = rows_of(WEEK, monday, monday + WEEK_START);
    assert_eq!(
        spec.check_ask(&from_monday).unwrap_err(),
        [
            AskError::RangeUnaligned {
                at: monday,
                width: Width { seconds: WEEK },
            },
            AskError::RangeUnaligned {
                at: monday + WEEK_START,
                width: Width { seconds: WEEK },
            },
        ]
    );
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
        from: Some(60),
        ..ask()
    };
    assert_eq!(
        spec.check_ask(&unheld).unwrap_err(),
        [
            AskError::UnknownMeasure {
                name: id("tips"),
                instead: Instead::Every(vec![id("n")])
            },
            AskError::UnknownKey {
                name: id("fare"),
                instead: Instead::Every(vec![id("bucket"), id("a"), id("z_id"), id("name")])
            },
            AskError::UnknownKey {
                name: id("colour"),
                instead: Instead::Every(vec![id("bucket"), id("a"), id("z_id"), id("name")])
            },
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
fn a_key_selected_without_group_by_says_to_group_by_it() {
    let spec = Spec::new(vec![dim()], vec![fact()]).unwrap();
    let ungrouped = Ask {
        measures: vec![id("n"), id("name"), id("tips")],
        keys: vec![AskKey::Key(id("a"))],
        ..ask()
    };
    assert_eq!(
        spec.check_ask(&ungrouped).unwrap_err(),
        [
            AskError::KeyNotGrouped(id("name")),
            AskError::UnknownMeasure {
                name: id("tips"),
                instead: Instead::Every(vec![id("n")])
            },
        ]
    );
}

#[test]
fn an_unknown_name_offers_the_one_near_it_else_every_name() {
    let spec = Spec::new(vec![dim()], vec![fact()]).unwrap();
    let slipped = Ask {
        view: id("tt"),
        ..ask()
    };
    // One letter off a name of one is no clear slip: list them instead.
    let every = Instead::Every(vec![id("t")]);
    let error = spec.check_ask(&slipped).unwrap_err();
    let unknown = AskError::UnknownView {
        name: id("tt"),
        instead: every,
    };
    assert_eq!(error, [unknown]);
    let slipped = Ask {
        keys: vec![AskKey::Key(id("nmae"))],
        ..ask()
    };
    let near = Instead::Closest(id("name"));
    let error = spec.check_ask(&slipped).unwrap_err();
    assert_eq!(
        error,
        [AskError::UnknownKey {
            name: id("nmae"),
            instead: near
        }]
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
        [AskError::UnknownView {
            name: id("nope"),
            instead: Instead::Every(vec![id("t")])
        }]
    );

    let empty = Ask {
        measures: vec![],
        filter: vec![Condition {
            key: id("fare"),
            values: vec![Literal::Int(1)],
        }],
        from: Some(86_400),
        to: Some(60),
        ..ask()
    };
    assert_eq!(
        spec.check_ask(&empty).unwrap_err(),
        [
            AskError::NoMeasures,
            AskError::UnknownKey {
                name: id("fare"),
                instead: Instead::Every(vec![id("bucket"), id("a"), id("z_id"), id("name")])
            },
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
        from: Some(3_600),
        to: Some(7_200),
        ..ask()
    };
    assert!(spec.check_ask(&narrowest).is_ok());
    let none = Ask {
        to: Some(3_600),
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
fn an_open_end_needs_no_bound_and_the_other_is_still_checked() {
    let spec = Spec::new(vec![dim()], vec![fact()]).unwrap();
    for (from, to) in [(None, None), (Some(3_600), None), (None, Some(3_600))] {
        let open = Ask { from, to, ..ask() };
        assert!(spec.check_ask(&open).is_ok(), "{from:?}..{to:?}");
    }
    // Against an open end no range is empty, but a bound must sit on an
    // edge.
    let unaligned = Ask {
        from: Some(60),
        to: None,
        ..ask()
    };
    assert_eq!(
        spec.check_ask(&unaligned).unwrap_err(),
        [AskError::RangeUnaligned {
            at: 60,
            width: Width { seconds: 3_600 },
        }]
    );
}

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
        assert_eq!(error.to_string(), shown);
    }
    assert_eq!(AskKey::Bucket.to_string(), "bucket");
    let days = AskKey::WiderBucket(Width { seconds: DAY });
    assert_eq!(days.to_string(), "bucket");
    assert!(days.is_time() && AskKey::Bucket.is_time());
    assert!(!AskKey::Key(id("a")).is_time());
}

#[test]
fn width_and_range_errors_display() {
    let cases = [
        (
            AskError::DateBinOwnWidth(Width { seconds: 3_600 }),
            "date_bin('1 hour', bucket) is the view's own width; write bucket",
        ),
        (
            AskError::WidthNotKept {
                width: Width { seconds: 2 * DAY },
                kept: vec![Width { seconds: 3_600 }, Width { seconds: DAY }],
            },
            "date_bin width '2 days' is not kept by the view; it keeps 1 hour, 1 day",
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
    ];
    for (error, shown) in cases {
        assert_eq!(error.to_string(), shown);
    }
}

#[test]
fn a_width_is_written_in_the_largest_unit_that_divides_it() {
    let cases = [
        (1, "1 second"),
        (90, "90 seconds"),
        (900, "15 minutes"),
        (3_600, "1 hour"),
        (5_400, "90 minutes"),
        (DAY, "1 day"),
        (WEEK, "7 days"),
        (0, "0 seconds"),
    ];
    for (seconds, written) in cases {
        assert_eq!(Width { seconds }.to_string(), written);
    }
    assert!(Width { seconds: DAY }.is_edge(-i64::from(DAY)));
    assert!(!Width { seconds: DAY }.is_edge(3_600));
}
