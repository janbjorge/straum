//! Refs and the keys over them: a dim keyed by two columns, a ref read
//! through its via column, null-ness keys and the FILTERs that count
//! through them, a dim shared by views, and the spec's own errors.

mod common;

use common::{
    BASE, FEB, JAN, any_message, ask_view_t, attribute, base, by_key, condition, id, key_text,
    spec_errors, sql_errors,
};
use straum_domain::{Ask, AskError, AskKey, ByKey, Literal, Spec, SpecError, Via};
use straum_notation::{Error, SqlError, parse};

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
    let fact = spec.fact("t").unwrap();
    assert_eq!(fact.by_keys, [by_key("org"), by_key("grp")]);
    assert_eq!(
        fact.refs[0].via,
        [Via::Fact(id("org")), Via::Fact(id("grp"))]
    );
    let dim = spec.dim("g").unwrap();
    assert_eq!(dim.key, [id("org_id"), id("grp_id")]);
    assert_eq!(dim.attributes, [id("label")]);
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
    assert!(
        spec.check_ask(&ask_view_t(&["org", "grp", "label"]))
            .is_ok()
    );
    assert_eq!(
        spec.check_ask(&ask_view_t(&["g"])).unwrap_err(),
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
    let fact = without_grp.fact("t").unwrap();
    assert_eq!(fact.by_keys, [by_key("org")]);
    assert_eq!(
        fact.refs[0].via,
        [Via::Fact(id("org")), Via::Fact(id("grp"))]
    );
    assert_eq!(fact.refs[0].reads, [attribute("label", "label")]);
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
        errors[0].message,
        "g is a left outer join and selects no column of it; select one or drop the join"
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
                message: format!(
                    "public.g is joined on {found} here and on org_id, grp_id elsewhere"
                )
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
    let fact = spec.fact("t").unwrap();
    assert_eq!(fact.by_keys, [by_key("a")]);
    assert_eq!(fact.refs[0].via, [Via::Fact(id("z_id"))]);
    assert_eq!(fact.refs[0].reads, [attribute("name", "name")]);
    // The via column keys the cells, selected or not.
    assert_eq!(fact.columns(), [id("at"), id("a"), id("z_id")]);
    assert_eq!(key_text(&spec, "t"), ["a", "z_id", "bucket"]);
    assert_eq!(
        fact.measure("named").unwrap().filter,
        [condition("name", vec![Literal::Text("x".into())])]
    );
    assert_eq!(spec.dim("z").unwrap().attributes, [id("name")]);

    // Asks name the attribute as the view named it; the ref itself and
    // its unread columns are no keys.
    assert!(spec.check_ask(&ask_view_t(&["a", "name"])).is_ok());
    for missing in ["z", "id", "z_id"] {
        assert_eq!(
            spec.check_ask(&ask_view_t(&[missing])).unwrap_err(),
            [AskError::UnknownKey(id(missing))],
            "{missing}"
        );
    }

    // Another view may select the via column too; the attribute must
    // still be grouped by.
    let both = format!("{ATTR_ONLY};\n{}", BASE.replace("VIEW t", "VIEW u"));
    let spec = parse(&both).unwrap();
    let fact = spec.fact("u").unwrap();
    assert_eq!(fact.by_keys, [by_key("a"), by_key("z_id")]);
    assert_eq!(fact.refs[0].reads, [attribute("name", "name")]);
    let errors = sql_errors(&ATTR_ONLY.replace("GROUP BY 1, 2, 3", "GROUP BY 1, 2"));
    assert_eq!(
        errors,
        [SqlError {
            line: 2,
            message: "GROUP BY 1, 2 must list the keys' positions in SELECT: GROUP BY 1, 2, 3"
                .into()
        }]
    );
}

/// Null-ness keys: whether a column is null, grouped like a column, and
/// what a FILTER's `IS NULL` counts through.
const NULLS: &str = "CREATE VIEW t WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket,
       f.a AS a,
       (f.done_at IS NOT NULL) AS done,
       (f.gone_at IS NULL) AS open,
       count(*) AS n,
       count(*) FILTER (WHERE f.done_at IS NOT NULL AND f.gone_at IS NOT NULL) AS closed,
       count(*) FILTER (WHERE f.done_at IS NULL) AS pending
FROM public.t AS f
GROUP BY 1, 2, 3, 4";

fn measure_names(spec: &Spec) -> Vec<String> {
    let mut names = Vec::new();
    for measure in &spec.fact("t").unwrap().measures {
        names.push(measure.name.to_string());
    }
    names
}

#[test]
fn null_ness_keys() {
    let spec = parse(NULLS).unwrap();
    let fact = spec.fact("t").unwrap();
    assert_eq!(
        fact.by_keys,
        [
            by_key("a"),
            ByKey::IsNull {
                column: id("done_at"),
                name: id("done"),
                negated: true
            },
            ByKey::IsNull {
                column: id("gone_at"),
                name: id("open"),
                negated: false
            },
        ]
    );
    assert_eq!(
        fact.columns(),
        [id("at"), id("a"), id("done_at"), id("gone_at")]
    );
    assert_eq!(
        key_text(&spec, "t"),
        ["a", "done_at is not null", "gone_at is null", "bucket"]
    );
    assert_eq!(
        fact.measure("closed").unwrap().filter,
        [
            condition("done", vec![Literal::Bool(true)]),
            condition("open", vec![Literal::Bool(false)]),
        ]
    );
    assert_eq!(
        fact.measure("pending").unwrap().filter,
        [condition("done", vec![Literal::Bool(false)])]
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
    assert_eq!(measure_names(&early), ["pending", "n", "closed"]);
    assert_eq!(early.fact("t").unwrap().by_keys, fact.by_keys);
    assert_eq!(
        early.fact("t").unwrap().measure("pending").unwrap().filter,
        fact.measure("pending").unwrap().filter
    );
    // Asks name the key.
    let ask = Ask {
        view: id("t"),
        measures: vec![id("n")],
        keys: vec![AskKey::Key(id("done"))],
        filter: vec![condition("open", vec![Literal::Bool(true)])],
        from: JAN,
        to: FEB,
    };
    assert!(spec.check_ask(&ask).is_ok());
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
        assert!(any_message(&errors, None, fragment), "{to}: {errors:?}");
    }
    // Over a dim column, and over the time column.
    let errors = sql_errors(&base("f.a AS a,", "f.a AS a, (z.name IS NULL) AS unnamed,"));
    let message = "the IS NULL column must be a f column";
    assert!(any_message(&errors, None, message), "{errors:?}");
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
    let text = format!(
        "{BASE};\n{}",
        BASE.replace("VIEW t", "VIEW u")
            .replace("z.name AS name", "z.code AS code, z.kind AS kind")
            .replace("GROUP BY 1, 2, 3, 4", "GROUP BY 1, 2, 3, 4, 5")
            .replace(
                "count(*) AS n",
                "count(*) AS n, count(*) FILTER (WHERE z.kind = 'x') AS m"
            )
    );
    let spec = parse(&text).unwrap();
    assert_eq!(
        spec.dim("z").unwrap().attributes,
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
            message: "public.z is joined on code here and on id elsewhere".into()
        }]
    );
}

#[test]
fn spec_errors_pass_through() {
    let text = base("'1 hour'", "'7 minutes'")
        .replace("date_bin('1 hour'", "date_bin('7 minutes'")
        .replace("f.a AS a,", "f.id AS id, f.z_id AS z,");
    assert!(matches!(parse(&text), Err(Error::Sql(_))));

    let text = base("'1 hour'", "'7 minutes'")
        .replace("date_bin('1 hour'", "date_bin('7 minutes'")
        .replace("count(*) AS n", "count(*) AS n, count(*) AS n");
    assert_eq!(
        spec_errors(&text),
        [
            SpecError::BadWidth {
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

    for text in [
        base("f.a AS a,", "f.a AS bucket,"),
        base("count(*) AS n", "count(*) AS bucket"),
        base("z.name AS name", "z.name AS bucket"),
    ] {
        assert_eq!(
            spec_errors(&text),
            [SpecError::NamedBucket { fact: id("t") }],
            "{text}"
        );
    }
    assert_eq!(
        SpecError::NamedBucket { fact: id("t") }.to_string(),
        "view t: bucket is what asks call the time bucket; \
         name the column, join or measure otherwise"
    );
}
