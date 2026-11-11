mod common;

use common::{FEB, JAN, cond, id};
use straum_domain::{
    Agg, Ask, AskError, AskKey, Attr, By, Grain, Literal, Origin, RefKind, Slot, Spec, SpecError,
    TimeKey, Via,
};
use straum_notation::{Error, SqlError, parse};

const TAXI: &str = include_str!("taxi.sql");

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

fn taxi() -> Spec {
    parse(TAXI).unwrap()
}

fn spec_errors(src: &str) -> Vec<SpecError> {
    match parse(src) {
        Err(Error::Spec(errors)) => errors,
        other => panic!("expected spec errors, got {other:?}"),
    }
}

fn sql_errors(src: &str) -> Vec<SqlError> {
    match parse(src) {
        Err(Error::Sql(errors)) => errors,
        other => panic!("expected straum SQL errors, got {other:?}"),
    }
}

fn month_ask() -> Ask {
    Ask {
        fact: id("trips"),
        measures: vec![id("trips"), id("fare"), id("tip_max")],
        by: vec![AskKey::Key(id("borough")), AskKey::Time(TimeKey::Bucket)],
        filter: vec![cond("borough", vec![Literal::Text("Manhattan".into())])],
        from: JAN,
        to: FEB,
    }
}

fn ask_view_t(by: &[&str]) -> Ask {
    Ask {
        fact: id("t"),
        measures: vec![id("n")],
        by: by.iter().map(|k| AskKey::Key(id(k))).collect(),
        filter: vec![],
        from: JAN,
        to: FEB,
    }
}

/// The rollup key as text: column names, null-ness tests and `bucket`.
fn key_text(spec: &Spec, fact: &str) -> Vec<String> {
    (spec.fact(fact).unwrap().rollup_key())
        .map(|slot| match slot {
            Slot::Column(c) => c.to_string(),
            Slot::IsNull { column, negated } => {
                format!("{column} is {}null", if negated { "not " } else { "" })
            }
            Slot::Bucket => "bucket".into(),
        })
        .collect()
}

#[test]
fn taxi_parses() {
    let spec = taxi();
    let trips = spec.fact("trips").unwrap();
    assert_eq!(trips.time.grain, Grain(900));
    assert_eq!(
        spec.dim("taxi_zones").unwrap().attrs,
        [id("borough"), id("zone"), id("service_zone")]
    );
    assert_eq!(
        key_text(&spec, "trips"),
        [
            "payment_type",
            "vendor_id",
            "pu_location_id",
            "do_location_id",
            "bucket"
        ]
    );
    assert_eq!(
        trips.refs[0].attrs,
        [
            attr("borough", "borough"),
            attr("zone", "zone"),
            attr("service_zone", "service_zone")
        ]
    );
    assert_eq!(
        trips.measure("distance").unwrap().agg,
        Agg::Avg(id("trip_distance"))
    );
    assert_eq!(
        trips.measure("card").unwrap().filter,
        [cond("payment_type", vec![Literal::Int(1)])]
    );
    assert_eq!(
        trips.measure("manhattan_pickups").unwrap().filter[0].key,
        id("borough")
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
        measures: vec![id("trips"), id("tips"), id("trips")],
        by: vec![
            AskKey::Key(id("fare_amount")),
            AskKey::Key(id("colour")),
            AskKey::Key(id("borough")),
            AskKey::Key(id("borough")),
            AskKey::Time(TimeKey::Bucket),
            AskKey::Time(TimeKey::Bucket),
        ],
        from: JAN + 60,
        ..month_ask()
    };
    assert_eq!(
        spec.check(&ask).unwrap_err(),
        [
            AskError::UnknownMeasure(id("tips")),
            AskError::UnknownKey(id("fare_amount")),
            AskError::UnknownKey(id("colour")),
            AskError::TimeKeyTwice,
            AskError::AskedTwice("borough".into()),
            AskError::AskedTwice("trips".into()),
            AskError::RangeUnaligned {
                at: JAN + 60,
                grain: Grain(900)
            },
        ]
    );
}

#[test]
fn ask_errors_display() {
    let ask_errors = [
        (AskError::UnknownFact(id("nope")), "unknown fact nope"),
        (AskError::UnknownMeasure(id("tips")), "unknown measure tips"),
        (
            AskError::UnknownKey(id("fare_amount")),
            "fare_amount is not a key of the view",
        ),
        (AskError::NoMeasures, "no measures asked"),
        (AskError::TimeKeyTwice, "more than one time key"),
        (AskError::AskedTwice("trips".into()), "trips asked twice"),
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

    assert_eq!(TimeKey::Bucket.to_string(), "bucket");
}

#[test]
fn ask_every_key_form() {
    let spec = taxi();
    let ask = Ask {
        by: vec![
            AskKey::Key(id("pu_location_id")),
            AskKey::Key(id("zone")),
            AskKey::Key(id("payment_type")),
            AskKey::Time(TimeKey::Bucket),
        ],
        ..month_ask()
    };
    assert!(spec.check(&ask).is_ok());
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
        filter: vec![cond("fare_amount", vec![Literal::Int(1)])],
        from: FEB,
        to: JAN + 60,
        ..month_ask()
    };
    assert_eq!(
        spec.check(&ask).unwrap_err(),
        [
            AskError::NoMeasures,
            AskError::UnknownKey(id("fare_amount")),
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

/// One fact `t` with a ref `z`, valid as written. Tests swap one piece.
const BASE: &str = "CREATE VIEW t WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket,
       f.a AS a, f.z_id AS z_id, z.name AS name, count(*) AS n
FROM public.t AS f
LEFT OUTER JOIN public.z AS z ON z.id = f.z_id
GROUP BY 1, 2, 3, 4";

fn base(from: &str, to: &str) -> String {
    assert!(BASE.contains(from), "{from}");
    BASE.replacen(from, to, 1)
}

#[test]
fn base_parses() {
    let spec = parse(BASE).unwrap();
    let t = spec.fact("t").unwrap();
    assert_eq!(t.table.schema, id("public"));
    assert_eq!(t.by, [col("a"), col("z_id")]);
    assert_eq!(t.refs[0].name, id("z"));
    assert_eq!(t.refs[0].via, [Via::Fact(id("z_id"))]);
    assert_eq!(t.refs[0].attrs, [attr("name", "name")]);
    assert!(t.time.buckets.is_empty());
    assert_eq!(t.time.origin, Origin::Timestamptz);
    let z = spec.dim("z").unwrap();
    assert_eq!(
        (&z.key[..], &z.attrs[..]),
        (&[id("id")][..], &[id("name")][..])
    );
}

/// A key goes by the name `AS` gives it, which may differ from the
/// column's; asks use that name, while a FILTER names the column, as
/// PostgreSQL reads the view.
#[test]
fn keys_go_by_their_select_name() {
    let src = base(
        "f.a AS a, f.z_id AS z_id, z.name AS name",
        "f.a AS kind, f.z_id AS zone, z.name AS label",
    )
    .replace(
        "count(*) AS n",
        "count(*) AS n, count(*) FILTER (WHERE f.a = 1 AND z.name = 'x') AS some",
    );
    let spec = parse(&src).unwrap();
    let t = spec.fact("t").unwrap();
    assert_eq!(
        t.by,
        [
            By::Column {
                column: id("a"),
                name: id("kind")
            },
            By::Column {
                column: id("z_id"),
                name: id("zone")
            }
        ]
    );
    assert_eq!(t.refs[0].attrs, [attr("label", "name")]);
    assert_eq!(
        t.measure("some").unwrap().filter,
        [
            cond("kind", vec![Literal::Int(1)]),
            cond("label", vec![Literal::Text("x".into())])
        ]
    );
    assert!(spec.check(&ask_view_t(&["kind", "zone", "label"])).is_ok());
    let shown: Vec<String> = (spec
        .check(&ask_view_t(&["a", "name", "z"]))
        .unwrap_err()
        .iter())
    .map(ToString::to_string)
    .collect();
    assert_eq!(
        shown,
        [
            "a is not a key of the view",
            "name is not a key of the view",
            "z is not a key of the view"
        ]
    );
}

#[test]
fn buckets_parse() {
    let src = base("buckets = '1 hour'", "buckets = '1 hour, 1 day,7 days'");
    let spec = parse(&src).unwrap();
    assert_eq!(
        spec.fact("t").unwrap().time.buckets,
        [Grain(86_400), Grain(604_800)]
    );

    let src = base(
        "buckets = '1 hour'",
        "buckets = '1 hour, 90 minutes, 1 day'",
    );
    assert_eq!(
        spec_errors(&src),
        [SpecError::BadBucket {
            fact: id("t"),
            seconds: 5_400,
            below: 3_600
        }]
    );
}

/// Every width is said in the view's `WITH`, the `date_bin` width first:
/// a view without it, or one whose list starts elsewhere, is refused.
#[test]
fn buckets_are_required_and_start_at_the_grain() {
    let errors = sql_errors(&base("VIEW t WITH (buckets = '1 hour') AS", "VIEW t AS"));
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(
        errors[0].msg,
        "view t needs WITH (buckets = '<n> <unit>, ...'): every width straum keeps, \
         the date_bin width first"
    );

    for wrong in ["1 day", "30 minutes, 1 hour", "1 day, 1 hour"] {
        let errors = sql_errors(&base("buckets = '1 hour'", &format!("buckets = '{wrong}'")));
        assert_eq!(errors.len(), 1, "{wrong}: {errors:?}");
        assert!(
            errors[0]
                .msg
                .ends_with("but date_bin is 3600s: list every width, the date_bin width first"),
            "{wrong}: {errors:?}"
        );
    }
    assert!(
        sql_errors(&base("buckets = '1 hour'", "buckets = '1 day, 1 hour'"))[0]
            .msg
            .starts_with("view t: buckets starts with 86400s but")
    );
}

/// A column without a zone takes `timestamp 'epoch'`; which one the column
/// is, the catalog checks.
#[test]
fn naive_origin_parses() {
    let spec = parse(&base("timestamptz 'epoch'", "timestamp 'epoch'")).unwrap();
    let time = &spec.fact("t").unwrap().time;
    assert_eq!(time.origin, Origin::Timestamp);
    assert_eq!(time.origin.to_string(), "timestamp 'epoch'");
    assert_eq!(Origin::Timestamptz.to_string(), "timestamptz 'epoch'");
}

#[test]
fn comments_and_empty_file() {
    let spec = parse("-- nothing declared yet\n\n").unwrap();
    assert!(spec.fact("trips").is_none());
}

#[test]
fn literals_and_filters() {
    let src = base(
        "count(*) AS n",
        "count(*) FILTER (WHERE f.a IN (-1, 2) AND (z.name = 'it''s' AND f.z_id = 3) \
         AND z.id = 4 AND f.a = true) AS n",
    );
    let spec = parse(&src).unwrap();
    // A dim key column names the fact column joined to it.
    assert_eq!(
        spec.fact("t").unwrap().measure("n").unwrap().filter,
        [
            cond("a", vec![Literal::Int(-1), Literal::Int(2)]),
            cond("name", vec![Literal::Text("it's".into())]),
            cond("z_id", vec![Literal::Int(3)]),
            cond("z_id", vec![Literal::Int(4)]),
            cond("a", vec![Literal::Bool(true)]),
        ]
    );
}

#[test]
fn every_agg_grain_unit_and_key_form() {
    let src = "CREATE OR REPLACE VIEW T WITH (buckets = '30 seconds') AS
SELECT date_bin('30 seconds', f.at, timestamptz 'epoch') AS bucket,
       f.a AS a, z.id AS z_id, z.name AS name,
       count(*) AS n, sum(f.x) AS s, min(f.x) AS lo, max(f.x) AS hi,
       avg(f.x) AS mean, count(DISTINCT f.x) AS uniq, sum(f.z_id) AS ids
FROM public.t AS f
LEFT OUTER JOIN public.z AS z ON z.id = f.z_id
GROUP BY 1, 2, 3, 4";
    let spec = parse(src).unwrap();
    let t = spec.fact("t").unwrap();
    assert_eq!(t.time.grain, Grain(30));
    // A dim key column names the fact column joined to it.
    assert_eq!(t.by, [col("a"), col("z_id")]);
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
            &Agg::Sum(id("z_id")),
        ]
    );
    assert_eq!(spec.dim("z").unwrap().attrs, [id("name")]);

    for (width, seconds) in [("1 second", 1), ("5 minute", 300), ("1 day", 86_400)] {
        let spec = parse(&base("1 hour", width).replace("1 hour", width)).unwrap();
        assert_eq!(spec.fact("t").unwrap().time.grain, Grain(seconds));
    }
}

/// One dim keyed by two columns, joined by two views in either order.
const GROUPED: &str = "CREATE VIEW t WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket,
       f.org AS org, f.grp AS grp, g.label AS label, count(*) AS n
FROM public.t AS f
LEFT OUTER JOIN public.g AS g ON g.org_id = f.org AND g.grp_id = f.grp
GROUP BY 1, 2, 3, 4";

#[test]
fn a_ref_on_two_columns() {
    let spec = parse(GROUPED).unwrap();
    let t = spec.fact("t").unwrap();
    assert_eq!(t.by, [col("org"), col("grp")]);
    assert_eq!(t.refs[0].via, [Via::Fact(id("org")), Via::Fact(id("grp"))]);
    let g = spec.dim("g").unwrap();
    assert_eq!(g.key, [id("org_id"), id("grp_id")]);
    assert_eq!(g.attrs, [id("label")]);
    // A dim key column names the fact column joined to it.
    let through = GROUPED.replace(
        "f.org AS org, f.grp AS grp",
        "g.org_id AS org, f.grp AS grp",
    );
    assert_eq!(parse(&through).unwrap(), spec);

    // Another view joins the same key in the other order: its via follows
    // the dim's key order.
    let other = GROUPED.replace("VIEW t", "VIEW u").replace(
        "ON g.org_id = f.org AND g.grp_id = f.grp",
        "ON g.grp_id = f.grp AND g.org_id = f.org",
    );
    let spec = parse(&format!(
        "{GROUPED};
{other}"
    ))
    .unwrap();
    assert_eq!(
        spec.fact("u").unwrap().refs[0].via,
        [Via::Fact(id("org")), Via::Fact(id("grp"))]
    );
    assert_eq!(spec.dim("g").unwrap().key, [id("org_id"), id("grp_id")]);

    // The ref has no value of its own: asks name the keys the view named.
    assert!(spec.check(&ask_view_t(&["org", "grp", "label"])).is_ok());
    assert_eq!(
        spec.check(&ask_view_t(&["g"])).unwrap_err(),
        [AskError::UnknownKey(id("g"))]
    );
}

#[test]
fn a_ref_on_two_columns_rejected() {
    // A via column left out is fine: the attribute keys the cells. A join
    // that selects nothing is not.
    let without_grp = parse(
        &GROUPED
            .replace("f.org AS org, f.grp AS grp,", "f.org AS org,")
            .replace("GROUP BY 1, 2, 3, 4", "GROUP BY 1, 2, 3"),
    )
    .unwrap();
    let t = without_grp.fact("t").unwrap();
    assert_eq!(t.by, [col("org")]);
    assert_eq!(t.refs[0].via, [Via::Fact(id("org")), Via::Fact(id("grp"))]);
    assert_eq!(t.refs[0].attrs, [attr("label", "label")]);
    let errors = sql_errors(
        &GROUPED
            .replace(
                "f.org AS org, f.grp AS grp, g.label AS label,",
                "f.org AS org,",
            )
            .replace("GROUP BY 1, 2, 3, 4", "GROUP BY 1, 2"),
    );
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert_eq!(
        errors[0].msg,
        "LEFT OUTER JOIN g selects no column of it; select one or drop the join"
    );
    // Another view joins the dim on another key list: fewer columns, or
    // as many but not the same.
    for (edit, found) in [
        ("", "org_id"),
        ("AND g.other_id = f.grp", "org_id, other_id"),
    ] {
        let other = GROUPED
            .replace("VIEW t", "VIEW u")
            .replace("AND g.grp_id = f.grp", edit);
        let errors = sql_errors(&format!("{GROUPED};\n{other}"));
        assert_eq!(
            errors,
            [SqlError {
                line: 7,
                msg: format!("public.g is joined on {found} here and on org_id, grp_id elsewhere")
            }]
        );
    }
}

/// A ref whose via column is not selected: the via column still keys the
/// cells, and the attribute is read through it.
const ATTR_ONLY: &str = "CREATE VIEW t WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket,
       f.a AS a, z.name AS name, count(*) AS n,
       count(*) FILTER (WHERE z.name = 'x') AS named
FROM public.t AS f
LEFT OUTER JOIN public.z AS z ON z.id = f.z_id
GROUP BY 1, 2, 3";

#[test]
fn a_ref_reads_what_the_view_selects() {
    let spec = parse(ATTR_ONLY).unwrap();
    let t = spec.fact("t").unwrap();
    assert_eq!(t.by, [col("a")]);
    assert_eq!(t.refs[0].via, [Via::Fact(id("z_id"))]);
    assert_eq!(t.refs[0].attrs, [attr("name", "name")]);
    // The via column keys the cells, selected or not.
    assert_eq!(t.columns(), [id("at"), id("a"), id("z_id")]);
    assert_eq!(key_text(&spec, "t"), ["a", "z_id", "bucket"]);
    assert_eq!(
        t.measure("named").unwrap().filter,
        [cond("name", vec![Literal::Text("x".into())])]
    );
    assert_eq!(spec.dim("z").unwrap().attrs, [id("name")]);

    // Asks name the attribute as the view named it; the ref itself and
    // its unread columns are no keys.
    assert!(spec.check(&ask_view_t(&["a", "name"])).is_ok());
    for missing in ["z", "id", "z_id"] {
        assert_eq!(
            spec.check(&ask_view_t(&[missing])).unwrap_err(),
            [AskError::UnknownKey(id(missing))],
            "{missing}"
        );
    }

    // Another view may select the via column too; the attribute must
    // still be grouped by.
    let both = format!("{ATTR_ONLY};\n{}", BASE.replace("VIEW t", "VIEW u"));
    let spec = parse(&both).unwrap();
    let u = spec.fact("u").unwrap();
    assert_eq!(u.by, [col("a"), col("z_id")]);
    assert_eq!(u.refs[0].attrs, [attr("name", "name")]);
    let errors = sql_errors(&ATTR_ONLY.replace("GROUP BY 1, 2, 3", "GROUP BY 1, 2"));
    assert_eq!(
        errors,
        [SqlError {
            line: 2,
            msg: "GROUP BY 1, 2 must list the keys' positions in SELECT: GROUP BY 1, 2, 3".into()
        }]
    );
}

/// Null-ness keys: whether a column is null, grouped like a column, and
/// what a FILTER's `IS NULL` counts through.
const NULLS: &str = "CREATE VIEW t WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket,
       f.a AS a,
       (f.done_at IS NOT NULL) AS done,
       f.gone_at IS NULL AS open,
       count(*) AS n,
       count(*) FILTER (WHERE f.done_at IS NOT NULL AND f.gone_at IS NOT NULL) AS closed,
       count(*) FILTER (WHERE f.done_at IS NULL) AS pending
FROM public.t AS f
GROUP BY 1, 2, 3, 4";

#[test]
fn null_ness_keys() {
    let spec = parse(NULLS).unwrap();
    let t = spec.fact("t").unwrap();
    assert_eq!(
        t.by,
        [
            col("a"),
            By::IsNull {
                column: id("done_at"),
                name: id("done"),
                negated: true
            },
            By::IsNull {
                column: id("gone_at"),
                name: id("open"),
                negated: false
            },
        ]
    );
    assert_eq!(
        t.columns(),
        [id("at"), id("a"), id("done_at"), id("gone_at")]
    );
    assert_eq!(
        key_text(&spec, "t"),
        ["a", "done_at is not null", "gone_at is null", "bucket"]
    );
    let filter = |m: &str| t.measure(m).unwrap().filter.clone();
    assert_eq!(
        filter("closed"),
        [
            cond("done", vec![Literal::Bool(true)]),
            cond("open", vec![Literal::Bool(false)]),
        ]
    );
    assert_eq!(
        filter("pending"),
        [cond("done", vec![Literal::Bool(false)])]
    );
    // A FILTER may come before the key it names.
    let filter_first = NULLS
        .replace(
            "AS bucket,\n",
            "AS bucket,\n       count(*) FILTER (WHERE f.done_at IS NULL) AS pending,\n",
        )
        .replace(
            "AS closed,\n       count(*) FILTER (WHERE f.done_at IS NULL) AS pending\n",
            "AS closed\n",
        )
        .replace("GROUP BY 1, 2, 3, 4", "GROUP BY 1, 3, 4, 5");
    let early = parse(&filter_first).unwrap();
    let names = |spec: &Spec| -> Vec<String> {
        (spec.fact("t").unwrap().measures.iter())
            .map(|m| m.name.to_string())
            .collect()
    };
    assert_eq!(names(&early), ["pending", "n", "closed"]);
    assert_eq!(early.fact("t").unwrap().by, t.by);
    assert_eq!(
        early.fact("t").unwrap().measure("pending").unwrap().filter,
        filter("pending")
    );
    // Asks name the key.
    let ask = Ask {
        fact: id("t"),
        measures: vec![id("n")],
        by: vec![AskKey::Key(id("done"))],
        filter: vec![cond("open", vec![Literal::Bool(true)])],
        from: JAN,
        to: FEB,
    };
    assert!(spec.check(&ask).is_ok());
}

#[test]
fn null_ness_keys_rejected() {
    let cases = [
        (
            "(f.done_at IS NOT NULL) AS done,",
            "(f.done_at IS NOT NULL),",
            "name every SELECT item: (f.done_at IS NOT NULL) AS <name>",
        ),
        // A FILTER names the column, as PostgreSQL reads the view, never
        // the key's name.
        (
            "count(*) FILTER (WHERE f.done_at IS NULL) AS pending",
            "count(*) FILTER (WHERE done = false) AS pending",
            "column, write alias.column, `done`",
        ),
        (
            "count(*) FILTER (WHERE f.done_at IS NULL) AS pending",
            "count(*) FILTER (WHERE f.a IS NULL) AS pending",
            "FILTER (WHERE f.a IS NULL) needs the key (f.a IS NULL) AS <name> in SELECT",
        ),
        (
            "count(*) FILTER (WHERE f.done_at IS NULL) AS pending",
            "count(*) FILTER (WHERE f.done_at = true) AS pending",
            "FILTER on f.done_at needs f.done_at AS <name> in SELECT",
        ),
    ];
    for (from, to, fragment) in cases {
        assert!(NULLS.contains(from), "{from}");
        let errors = sql_errors(&NULLS.replacen(from, to, 1));
        assert!(
            errors.iter().any(|e| e.msg.contains(fragment)),
            "{to}: {errors:?}"
        );
    }
    // Over a dim column, and over the time column.
    let errors = sql_errors(&base("f.a AS a,", "f.a AS a, (z.name IS NULL) AS unnamed,"));
    assert!(
        errors
            .iter()
            .any(|e| e.msg == "the IS NULL column must be a f column"),
        "{errors:?}"
    );
    let no_time = base("f.a AS a,", "f.a AS a, (f.at IS NULL) AS no_time,")
        .replace("GROUP BY 1, 2, 3, 4", "GROUP BY 1, 2, 3, 4, 5");
    assert_eq!(
        spec_errors(&no_time),
        [SpecError::NullTime {
            fact: id("t"),
            name: id("no_time"),
            column: id("at")
        }]
    );
}

#[test]
fn views_share_a_dim() {
    let src = format!(
        "{BASE};\n{}",
        BASE.replace("VIEW t", "VIEW u")
            .replace("z.name AS name", "z.code AS code, z.kind AS kind")
            .replace("GROUP BY 1, 2, 3, 4", "GROUP BY 1, 2, 3, 4, 5")
            .replace(
                "count(*) AS n",
                "count(*) AS n, count(*) FILTER (WHERE z.kind = 'x') AS m"
            )
    );
    let spec = parse(&src).unwrap();
    assert_eq!(
        spec.dim("z").unwrap().attrs,
        [id("name"), id("code"), id("kind")]
    );
    assert!(spec.fact("u").is_some());

    let clash = format!(
        "{BASE};\n{}",
        BASE.replace("VIEW t", "VIEW u")
            .replace("z.id =", "z.code =")
    );
    assert_eq!(
        sql_errors(&clash),
        [SqlError {
            line: 7,
            msg: "public.z is joined on code here and on id elsewhere".into()
        }]
    );
}

#[test]
fn spec_errors_pass_through() {
    let src = base("'1 hour'", "'7 minutes'")
        .replace("date_bin('1 hour'", "date_bin('7 minutes'")
        .replace("f.a AS a,", "f.id AS id, f.z_id AS z,");
    assert!(matches!(parse(&src), Err(Error::Sql(_))));

    let src = base("'1 hour'", "'7 minutes'")
        .replace("date_bin('1 hour'", "date_bin('7 minutes'")
        .replace("count(*) AS n", "count(*) AS n, count(*) AS n");
    assert_eq!(
        spec_errors(&src),
        [
            SpecError::BadGrain {
                fact: id("t"),
                seconds: 420
            },
            SpecError::Duplicate {
                scope: Some(id("t")),
                name: id("n")
            },
        ]
    );

    let clash = base("f.a AS a,", "f.z AS z_id,");
    assert_eq!(
        spec_errors(&clash),
        [SpecError::Duplicate {
            scope: Some(id("t")),
            name: id("z_id")
        }]
    );

    for src in [
        base("f.a AS a,", "f.a AS bucket,"),
        base("count(*) AS n", "count(*) AS bucket"),
        base("z.name AS name", "z.name AS bucket"),
    ] {
        assert_eq!(
            spec_errors(&src),
            [SpecError::NamedBucket { fact: id("t") }],
            "{src}"
        );
    }
    assert_eq!(
        SpecError::NamedBucket { fact: id("t") }.to_string(),
        "fact t: bucket is what asks call the time bucket; \
         name the column, ref or measure otherwise"
    );
}

/// Every construct outside straum SQL is an error that names it; none is
/// ignored. `(from, to, line, message fragment)` edits to `BASE`.
fn rejects(cases: &[(&str, &str, u64, &str)]) {
    let mut failed = Vec::new();
    for &(from, to, line, fragment) in cases {
        let errors = match parse(&base(from, to)) {
            Err(Error::Sql(errors)) => errors,
            other => {
                failed.push(format!("{to}: got {other:?}"));
                continue;
            }
        };
        if !errors
            .iter()
            .any(|e| e.line == line && e.msg.contains(fragment))
        {
            failed.push(format!(
                "{to}: expected line {line}: {fragment}, got {errors:?}"
            ));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// Statements and view shape.
#[test]
fn view_shapes_outside_the_subset_are_rejected() {
    rejects(&[
        (
            "CREATE VIEW t",
            "CREATE MATERIALIZED VIEW t",
            1,
            "MATERIALIZED is not in straum SQL",
        ),
        (
            "CREATE VIEW t",
            "CREATE TEMPORARY VIEW t",
            1,
            "TEMPORARY is not in straum SQL",
        ),
        (
            "CREATE VIEW t",
            "CREATE VIEW IF NOT EXISTS t",
            1,
            "IF NOT EXISTS is not in straum SQL",
        ),
        (
            "CREATE VIEW t",
            "CREATE OR ALTER VIEW t",
            1,
            "OR ALTER is not in straum SQL",
        ),
        (
            "CREATE VIEW t",
            "CREATE SECURE VIEW t",
            1,
            "SECURE is not in straum SQL",
        ),
        (
            "WITH (buckets = '1 hour') AS",
            "COPY GRANTS AS",
            1,
            "COPY GRANTS is not in straum SQL",
        ),
        (
            "VIEW t WITH",
            "VIEW t (a, b) WITH",
            1,
            "a view column list is not in straum SQL",
        ),
        (
            "VIEW t WITH",
            "VIEW s.t WITH",
            1,
            "view s.t must be a bare name",
        ),
        (
            "VIEW t WITH",
            "VIEW \"t\" WITH",
            1,
            "quoted identifier `\"t\"` is not in straum SQL",
        ),
    ]);
}

/// Every view option but buckets. `key` was required once; a spec that
/// still has it says so.
#[test]
fn view_options_other_than_buckets_are_rejected() {
    rejects(&[
        (
            "WITH (buckets = '1 hour')",
            "WITH (key = id)",
            1,
            "view option key = id is gone: straum reads the whole old row \
             under REPLICA IDENTITY FULL, so drop it",
        ),
        (
            "WITH (buckets = '1 hour')",
            "WITH (colour = 1)",
            1,
            "view option `colour = 1` is not in straum SQL",
        ),
        (
            "WITH (buckets = '1 hour')",
            "WITH (append = true)",
            1,
            "view option `append = true` is not in straum SQL",
        ),
        (
            "WITH (buckets = '1 hour')",
            "WITH (buckets = 1)",
            1,
            "buckets, use '<n> <unit>, ...', `1`",
        ),
        (
            "buckets = '1 hour'",
            "buckets = '1 hour, 2 weeks'",
            1,
            "bucket width '2 weeks': use '<n> seconds|minutes|hours|days'",
        ),
        (
            "buckets = '1 hour'",
            "buckets = ''",
            1,
            "bucket width '': use",
        ),
        (
            "buckets = '1 hour'",
            "buckets = '1 hour', buckets = '2 days'",
            1,
            "view option buckets given twice",
        ),
        (
            "WITH (buckets = '1 hour')",
            "WITH (\"buckets\" = '1 hour')",
            1,
            "quoted identifier `\"buckets\"`",
        ),
        (
            "WITH (buckets = '1 hour')",
            &format!("WITH ({} = 1)", "k".repeat(64)),
            1,
            "bad identifier kkkk",
        ),
    ]);
}

/// Query.
#[test]
fn query_clauses_outside_the_subset_are_rejected() {
    rejects(&[
        (
            "AS\nSELECT",
            "AS\nWITH c AS (SELECT 1) SELECT",
            2,
            "WITH (a CTE) is not in straum SQL",
        ),
        (
            "SELECT date_bin",
            "SELECT /*+ x */ date_bin",
            2,
            "an optimizer hint is not",
        ),
        (
            "SELECT date_bin",
            "SELECT ALL date_bin",
            2,
            "SELECT ALL is not",
        ),
        ("SELECT date_bin", "SELECT TOP 1 date_bin", 2, "TOP is not"),
        (
            "count(*) AS n",
            "count(*) AS n INTO x",
            2,
            "SELECT INTO is not",
        ),
        (
            "z ON z.id = f.z_id",
            "z ON z.id = f.z_id LATERAL VIEW explode(x) e AS y",
            2,
            "LATERAL VIEW is not",
        ),
        (
            "z ON z.id = f.z_id",
            "z ON z.id = f.z_id CONNECT BY a = b",
            2,
            "CONNECT BY is not",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 CLUSTER BY 1",
            2,
            "CLUSTER BY is not",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 DISTRIBUTE BY 1",
            2,
            "DISTRIBUTE BY is not",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 SORT BY 1",
            2,
            "SORT BY is not",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 QUALIFY n > 1",
            2,
            "QUALIFY is not",
        ),
        (
            "FROM public.t AS f",
            "FROM public.t(1) AS f",
            4,
            "a table function is not",
        ),
        (
            "FROM public.t AS f",
            "FROM public.t(1) WITH ORDINALITY AS f",
            4,
            "WITH ORDINALITY is not",
        ),
        (
            "FROM public.t AS f",
            "FROM public.t AS f WITH (NOLOCK)",
            4,
            "a table hint is not",
        ),
        (
            "LEFT OUTER JOIN",
            "GLOBAL LEFT OUTER JOIN",
            5,
            "GLOBAL is not",
        ),
        (
            "count(*) AS n",
            "count(*) RESPECT NULLS AS n",
            3,
            "IGNORE or RESPECT NULLS is not",
        ),
        (
            "count(*) AS n",
            "{fn count(*)} AS n",
            3,
            "ODBC call syntax is not",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 ORDER BY 1",
            2,
            "ORDER BY is not",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 LIMIT 5",
            2,
            "LIMIT or OFFSET is not",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 FOR UPDATE",
            2,
            "FOR UPDATE or FOR SHARE",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 UNION SELECT 1",
            2,
            "view body, use one SELECT,",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 FETCH FIRST 1 ROWS ONLY",
            2,
            "FETCH is not",
        ),
    ]);
}

/// Select clauses.
#[test]
fn select_clauses_outside_the_subset_are_rejected() {
    rejects(&[
        (
            "SELECT date_bin",
            "SELECT DISTINCT date_bin",
            2,
            "SELECT DISTINCT is not",
        ),
        (
            "\nGROUP BY",
            "\nWHERE f.b = 1\nGROUP BY",
            6,
            "FILTER on f.b needs f.b AS <name> in SELECT",
        ),
        (
            "\nGROUP BY",
            "\nWHERE a = 1\nGROUP BY",
            6,
            "column, write alias.column, `a`",
        ),
        (
            "\nGROUP BY",
            "\nWHERE f.a = 1 OR f.a = 2\nGROUP BY",
            6,
            "filter condition, write = or IN joined by AND,",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 HAVING count(*) > 1",
            2,
            "HAVING is not",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 WINDOW w AS ()",
            2,
            "WINDOW is not",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY ROLLUP (1, 2, 3, 4)",
            6,
            "GROUP BY item, write the key's position in SELECT, `ROLLUP (1, 2, 3, 4)`",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY ALL",
            2,
            "GROUP BY ALL is not",
        ),
    ]);
}

/// FROM: one table, written whole.
#[test]
fn from_forms_outside_the_subset_are_rejected() {
    rejects(&[
        (
            "FROM public.t AS f",
            "FROM public.t AS f, public.u AS u",
            2,
            "exactly one FROM table",
        ),
        (
            "FROM public.t AS f",
            "FROM (SELECT 1) AS f",
            4,
            "table `(SELECT 1) AS f` is not",
        ),
        (
            "FROM public.t AS f",
            "FROM public.t AS f(a)",
            4,
            "an alias column list is not",
        ),
        (
            "FROM public.t AS f",
            "FROM public.t AS f TABLESAMPLE SYSTEM (10)",
            4,
            "TABLESAMPLE is not",
        ),
    ]);
}

/// Semi and anti joins: one shape of EXISTS.
#[test]
fn exists_has_one_shape() {
    rejects(&[
        (
            "\nGROUP BY",
            "\nWHERE EXISTS (SELECT * FROM public.p AS p WHERE p.k = f.a)\nGROUP BY",
            6,
            "EXISTS, write EXISTS (SELECT 1 FROM ...),",
        ),
        (
            "\nGROUP BY",
            "\nWHERE EXISTS (SELECT 1 FROM public.p AS p)\nGROUP BY",
            6,
            "EXISTS without WHERE alias.key = ...,",
        ),
        (
            "\nGROUP BY",
            "\nWHERE EXISTS (SELECT 1 FROM public.p AS p WHERE p.k = 1)\nGROUP BY",
            6,
            "join ON p.<key> = f.<column>",
        ),
        (
            "\nGROUP BY",
            "\nWHERE EXISTS (SELECT 1 FROM public.p AS p, public.q AS q WHERE p.k = f.a)\nGROUP BY",
            6,
            "EXISTS, write EXISTS (SELECT 1 FROM one table),",
        ),
        (
            "\nGROUP BY",
            "\nWHERE EXISTS (SELECT 1 FROM public.p WHERE p.k = f.a)\nGROUP BY",
            6,
            "table public.p must be public.p AS <alias>",
        ),
        (
            "\nGROUP BY",
            "\nWHERE EXISTS (SELECT 1 FROM public.p AS z WHERE z.k = f.a)\nGROUP BY",
            6,
            "alias z used twice",
        ),
    ]);
}

/// A join ON compares a dim key to a fact column, never through a dim
/// that keeps no row or a column the near dim is not read for.
#[test]
fn join_on_names_a_dim_key_and_a_fact_column() {
    rejects(&[
        (
            "LEFT OUTER JOIN public.z AS z ON z.id = f.z_id",
            "LEFT OUTER JOIN public.z AS z ON z.id = f.z_id \
             LEFT OUTER JOIN public.w AS w ON w.id = q.x",
            5,
            "join ON w.<key> = f.<column>",
        ),
        (
            "ON z.id = f.z_id",
            "ON z.id = 1",
            5,
            "join ON z.<key> = f.<column>, the joined table's column on the left, one or \
             more joined by AND; found z.id = 1",
        ),
        (
            "ON z.id = f.z_id",
            "ON z.id = f.z_id OR z.id = f.a",
            5,
            "join ON z.<key> = f.<column>",
        ),
        (
            "ON z.id = f.z_id",
            "ON z.id > f.z_id",
            5,
            "join ON z.<key> = f.<column>",
        ),
        (
            "ON z.id = f.z_id",
            "ON z.id = z.z_id",
            5,
            "join ON z.<key> = f.<column>",
        ),
        (
            "public.z AS z ON z.id",
            "public.z AS f ON f.id",
            5,
            "alias f used twice",
        ),
    ]);
}

/// SELECT items and columns: every item named, every column owned.
#[test]
fn select_items_are_named_and_columns_owned() {
    rejects(&[
        (
            "f.a AS a, f.z_id AS z_id",
            "*, f.z_id AS z_id",
            3,
            "SELECT item `*` is not",
        ),
        (
            "f.a AS a, f.z_id AS z_id",
            "f.*, f.z_id AS z_id",
            3,
            "SELECT item `f.*` is not",
        ),
        (
            "f.a AS a, f.z_id AS z_id",
            "public.f.a AS a, f.z_id AS z_id",
            3,
            "column, write alias.column, `public.f.a`",
        ),
        (
            "f.a AS a, f.z_id AS z_id",
            "q.a AS a, f.z_id AS z_id",
            3,
            "unknown table alias q",
        ),
        (
            "f.a AS a, f.z_id AS z_id",
            "f.a + 1 AS a, f.z_id AS z_id",
            3,
            "column, write alias.column, `f.a + 1`",
        ),
        (
            "f.a AS a, f.z_id AS z_id",
            "f.a AS \"A\", f.z_id AS z_id",
            3,
            "quoted identifier `\"A\"` is not",
        ),
        (
            "f.z_id AS z_id, z.name AS name, ",
            "f.z_id AS z_id, ",
            2,
            "LEFT OUTER JOIN z selects no column of it; select one or drop the join",
        ),
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE z.code = 'x') AS n",
            3,
            "FILTER on z.code needs z.code AS <name> in SELECT",
        ),
    ]);
}

/// Aggregates.
#[test]
fn aggregates_outside_the_subset_are_rejected() {
    rejects(&[
        (
            "count(*) AS n",
            "median(f.x) AS n",
            3,
            "aggregate `median(f.x)` is not",
        ),
        (
            "count(*) AS n",
            "sum(DISTINCT f.x) AS n",
            3,
            "DISTINCT outside count",
        ),
        (
            "count(*) AS n",
            "sum(ALL f.x) AS n",
            3,
            "ALL inside a call is not",
        ),
        (
            "count(*) AS n",
            "sum(z.name) AS n",
            3,
            "an aggregated column must be a f column",
        ),
        (
            "count(*) AS n",
            "sum(*) AS n",
            3,
            "arguments `sum(*)` is not",
        ),
        (
            "count(*) AS n",
            "sum(f.x, f.y) AS n",
            3,
            "arguments `sum(f.x, f.y)` is not",
        ),
        (
            "count(*) AS n",
            "count(*) OVER () AS n",
            3,
            "a window function (OVER) is not",
        ),
        (
            "count(*) AS n",
            "sum(f.x ORDER BY f.x) AS n",
            3,
            "ORDER BY or LIMIT inside a call",
        ),
        (
            "count(*) AS n",
            "percentile_cont(0.5) WITHIN GROUP (ORDER BY f.x) AS n",
            3,
            "WITHIN GROUP is not",
        ),
        (
            "count(*) AS n",
            "\"count\"(*) AS n",
            3,
            "aggregate `\"count\"(*)` is not",
        ),
        (
            "count(*) AS n",
            "current_timestamp AS n",
            3,
            "function call `current_timestamp` is not",
        ),
    ]);
}

/// Measure filters: one condition form.
#[test]
fn measure_filters_take_one_condition_form() {
    rejects(&[
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE f.a > 1) AS n",
            3,
            "filter condition",
        ),
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE f.a = f.b) AS n",
            3,
            "literal, use an integer, 'text', true or false, `f.b`",
        ),
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE f.a = NULL) AS n",
            3,
            "literal",
        ),
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE f.a = 1.5) AS n",
            3,
            "literal",
        ),
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE f.a = -'x') AS n",
            3,
            "literal",
        ),
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE f.a = 99999999999999999999) AS n",
            3,
            "literal",
        ),
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE f.a IN (1, x)) AS n",
            3,
            "literal, use an integer, 'text', true or false, `x`",
        ),
    ]);
}

/// The bucket.
#[test]
fn the_bucket_is_one_date_bin() {
    rejects(&[
        (
            "date_bin('1 hour'",
            "date_bin('15x'",
            2,
            "date_bin width '15x': use '<n> seconds|minutes|hours|days'",
        ),
        (
            "date_bin('1 hour'",
            "date_bin('1 week'",
            2,
            "date_bin width",
        ),
        (
            "date_bin('1 hour'",
            "date_bin('99999999 days'",
            2,
            "date_bin width",
        ),
        (
            "date_bin('1 hour'",
            "date_bin(interval '1 hour'",
            2,
            "date_bin width",
        ),
        (
            "timestamptz 'epoch'",
            "'epoch'",
            2,
            "date_bin origin 'epoch': use timestamptz 'epoch' or timestamp 'epoch'",
        ),
        (
            "timestamptz 'epoch'",
            "timestamp with time zone 'epoch'",
            2,
            "date_bin origin",
        ),
        (
            "timestamptz 'epoch'",
            "timestamp 'origin'",
            2,
            "date_bin origin",
        ),
        (
            "timestamptz 'epoch'",
            "timestamptz '2000-01-01'",
            2,
            "date_bin origin",
        ),
        (
            "f.at, timestamptz 'epoch')",
            "f.at)",
            2,
            "date_bin arguments",
        ),
        (
            "date_bin('1 hour', f.at",
            "date_bin('1 hour', z.at",
            2,
            "the date_bin column must be a f column",
        ),
        (
            "timestamptz 'epoch')",
            "timestamptz 'epoch') FILTER (WHERE f.a = 1)",
            2,
            "FILTER on date_bin",
        ),
        (
            "date_bin('1 hour',",
            "date_bin(DISTINCT '1 hour',",
            2,
            "DISTINCT on date_bin",
        ),
        ("date_bin(", "date_trunc(", 2, "aggregate `date_trunc"),
        (
            "SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket,",
            "SELECT",
            2,
            "SELECT needs a bucket",
        ),
        (
            "AS bucket,",
            "AS bucket, date_bin('1 day', f.at, timestamptz 'epoch') AS bucket,",
            2,
            "more than one date_bin bucket",
        ),
    ]);
}

/// GROUP BY: the keys' positions, in order.
#[test]
fn group_by_takes_the_key_positions() {
    rejects(&[
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4, 9",
            2,
            "GROUP BY 1, 2, 3, 4, 9 must list the keys' positions in SELECT",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 0, 1, 2, 3, 4",
            2,
            "GROUP BY 0, 1, 2, 3, 4 must list the keys' positions in SELECT",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, (2), 3, 4",
            6,
            "GROUP BY item, write the key's position in SELECT, `(2)`",
        ),
    ]);
}

#[test]
fn joins_of_every_kind_parse() {
    // Inner and left outer joins, a join through a dim, and both EXISTS
    // forms, in one view.
    let src = "CREATE VIEW t WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket,
       f.a AS a, z.name AS name, w.label AS label, count(*) AS n
FROM public.t AS f
INNER JOIN public.z AS z ON z.id = f.z_id
LEFT OUTER JOIN public.w AS w ON w.id = z.w_id
WHERE EXISTS (SELECT 1 FROM public.p AS p WHERE p.k = f.a)
  AND NOT EXISTS (SELECT 1 FROM public.b AS b WHERE b.x = f.a AND b.y = z.name)
  AND f.a IN (1, 2) AND z.name = 'x'
GROUP BY 1, 2, 3, 4";
    let spec = parse(src).unwrap();
    let t = spec.fact("t").unwrap();
    let shown: Vec<(String, RefKind, Vec<Via>, usize)> = (t.refs.iter())
        .map(|r| (r.name.to_string(), r.kind, r.via.clone(), r.attrs.len()))
        .collect();
    assert_eq!(
        shown,
        [
            ("z".into(), RefKind::Inner, vec![Via::Fact(id("z_id"))], 1),
            (
                "w".into(),
                RefKind::Left,
                vec![Via::Attr {
                    through: 0,
                    column: id("w_id")
                }],
                1
            ),
            ("p".into(), RefKind::Semi, vec![Via::Fact(id("a"))], 0),
            (
                "b".into(),
                RefKind::Anti,
                vec![
                    Via::Fact(id("a")),
                    Via::Attr {
                        through: 0,
                        column: id("name")
                    }
                ],
                0
            ),
        ]
    );
    // The dim joined through carries the column read for the key.
    assert_eq!(spec.dim("z").unwrap().attrs, [id("name"), id("w_id")]);
    assert_eq!(spec.dim("b").unwrap().key, [id("x"), id("y")]);
    assert_eq!(
        t.filter,
        [
            cond("a", vec![Literal::Int(1), Literal::Int(2)]),
            cond("name", vec![Literal::Text("x".into())]),
        ]
    );
    // Only the fact-side join columns key the cells.
    assert_eq!(key_text(&spec, "t"), ["a", "z_id", "bucket"]);
    assert_eq!(t.columns(), [id("at"), id("a"), id("z_id")]);
}

#[test]
fn joins_of_every_kind_rejected() {
    let base = "CREATE VIEW t WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket,
       f.a AS a, z.name AS name, count(*) AS n
FROM public.t AS f
INNER JOIN public.z AS z ON z.id = f.z_id
WHERE EXISTS (SELECT 1 FROM public.p AS p WHERE p.k = f.a)
GROUP BY 1, 2, 3";
    for (from, to, fragment) in [
        // A semi join has no columns to select.
        (
            "z.name AS name,",
            "z.name AS name, p.x AS x,",
            "p is a semi join and has no columns to select; select none of it",
        ),
        // Nor to join through.
        (
            "GROUP BY",
            "AND EXISTS (SELECT 1 FROM public.q AS q WHERE q.k = p.x)\nGROUP BY",
            "join ON q.<key> = f.<column>",
        ),
        // An inner join that selects nothing.
        (
            "z.name AS name,",
            "",
            "INNER JOIN z selects no column of it; select one or drop the join",
        ),
        // The same FILTER on every measure is the view's WHERE.
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE f.a = 1) AS n, count(*) FILTER (WHERE f.a = 1) AS m",
            "every measure has the same FILTER; write it as the view's WHERE",
        ),
    ] {
        assert!(base.contains(from), "{from}");
        let src = base.replacen(from, to, 1).replace(
            "GROUP BY 1, 2, 3",
            if to.is_empty() {
                "GROUP BY 1, 2"
            } else {
                "GROUP BY 1, 2, 3"
            },
        );
        let errors = match parse(&src) {
            Err(Error::Sql(errors)) => errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
            Err(Error::Spec(errors)) => errors.iter().map(ToString::to_string).collect(),
            Ok(_) => panic!("{to}: parsed"),
        };
        assert!(
            errors.iter().any(|e| e.contains(fragment)),
            "{to}: {errors:?} has no {fragment:?}"
        );
    }
    // One FILTER on the one measure is fine: there is nothing to share.
    let one = base.replace("count(*) AS n", "count(*) FILTER (WHERE f.a = 1) AS n");
    assert!(parse(&one).is_ok());
}

#[test]
fn other_statements_are_rejected() {
    assert_eq!(
        sql_errors("SELECT 1;\nCREATE TABLE t (a int)"),
        [
            SqlError {
                line: 1,
                msg:
                    "statement `SELECT 1` is not in straum SQL, the PostgreSQL subset made only for straum"
                        .into()
            },
            SqlError {
                line: 2,
                msg: "statement `CREATE TABLE t (a INT)` is not in straum SQL, \
                      the PostgreSQL subset made only for straum"
                    .into()
            },
        ]
    );
}

#[test]
fn long_sql_is_cut_in_errors() {
    let long = format!(
        "count(*) FILTER (WHERE f.a = 1 OR f.{} = 2) AS n",
        "b".repeat(80)
    );
    let errors = sql_errors(&base("count(*) AS n", &long));
    assert!(errors[0].msg.contains("f.a = 1 OR f.bbbb"), "{errors:?}");
    assert!(errors[0].msg.contains("...`"), "{errors:?}");
}

#[test]
fn parser_errors_and_deep_nesting_are_errors() {
    let errors = sql_errors("CREATE VIEW t AS SELEC 1");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].line, 0);
    assert!(errors[0].msg.contains("Line: 1"), "{errors:?}");
    assert_eq!(
        parse("CREATE VIEW t AS SELEC 1").unwrap_err().to_string(),
        errors[0].msg
    );

    let deep = format!("{}1{}", "(".repeat(500), ")".repeat(500));
    let errors = sql_errors(&base(
        "count(*) AS n",
        &format!("count(*) FILTER (WHERE f.a = {deep}) AS n"),
    ));
    assert!(errors[0].msg.contains("recursion limit"), "{errors:?}");
}

#[test]
fn error_display() {
    let src = base(
        "buckets = '1 hour'",
        "buckets = '1 hour', buckets = '2 days'",
    )
    .replace("f.a AS a,", "q.a AS a,");
    assert_eq!(
        parse(&src).unwrap_err().to_string(),
        "line 1: view option buckets given twice\n\
         line 3: unknown table alias q"
    );
    let src = base("count(*) AS n", "count(*) AS n, count(*) AS n");
    assert_eq!(parse(&src).unwrap_err().to_string(), "t: n declared twice");
}
