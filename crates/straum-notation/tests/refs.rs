//! Refs and the keys over them: a dim keyed by two columns, a ref read
//! through its via column, null-ness keys and the FILTERs that count
//! through them, a dim shared by views, and the spec's own errors; then
//! joins through a dim joined before, and the one shape of `EXISTS`.

mod common;

use common::{
    BASE, FEB, JAN, any_contains, any_message, ask_view_t, attribute, base, by_key, condition,
    edit, error_lines, id, key_text, rejects, spec_errors, sql_errors, strings,
};
use straum_domain::{Ask, AskError, AskKey, ByKey, Instead, Literal, Spec, SpecError, Via};
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
    // An inner-joined dim's key column names the fact column joined to it.
    let inner = GROUPED.replace("LEFT OUTER JOIN", "INNER JOIN");
    let through = inner.replace(
        "f.org AS org, f.grp AS grp",
        "g.org_id AS org, f.grp AS grp",
    );
    assert_eq!(parse(&through).unwrap(), parse(&inner).unwrap());

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
        [AskError::UnknownKey {
            name: id("g"),
            instead: Instead::Every(vec![id("bucket"), id("org"), id("grp"), id("label")])
        }]
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
    let errors = spec_errors(
        &GROUPED
            .replace(
                "f.org AS org, f.grp AS grp, g.label AS label,",
                "f.org AS org,",
            )
            .replace("GROUP BY 1, 2, 3, 4", "GROUP BY 1, 2"),
    );
    assert_eq!(
        strings(&errors),
        [
            "view t: join g reads no column of its dim; a key column reads the column it is joined to, \
         so select another"
        ]
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
                line: Some(7),
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
            [AskError::UnknownKey {
                name: id(missing),
                instead: Instead::Every(vec![id("bucket"), id("a"), id("name")])
            }],
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
            line: Some(2),
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
        having: Vec::new(),
        from: Some(JAN),
        to: Some(FEB),
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
            line: Some(7),
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

/// A join's near side may be the key column of an inner-joined dim: it
/// reads the fact column that dim was joined on.
#[test]
fn a_join_through_a_dim_key_reads_the_fact_column() {
    let chain = base(
        "LEFT OUTER JOIN public.z AS z ON z.id = f.z_id",
        "INNER JOIN public.z AS z ON z.id = f.z_id\n\
         LEFT OUTER JOIN public.w AS w ON w.id = z.id",
    )
    .replace(
        "z.name AS name, count(*)",
        "z.name AS name, w.label AS label, count(*)",
    )
    .replace("GROUP BY 1, 2, 3, 4", "GROUP BY 1, 2, 3, 4, 5");
    let spec = parse(&chain).unwrap();
    let refs = &spec.fact("t").unwrap().refs;
    assert_eq!(refs[1].via, [Via::Fact(id("z_id"))]);
    // Through a dim column that is no key, the dim carries the column.
    let through = chain.replace("w.id = z.id", "w.id = z.name");
    let spec = parse(&through).unwrap();
    assert_eq!(
        spec.fact("t").unwrap().refs[1].via,
        [Via::Attribute {
            through: 0,
            column: id("name")
        }]
    );
}

/// A left-joined dim's key column is null where no dim row matches, and
/// the fact column it was joined on is not, so no read stands in for the
/// other: a key, a null-ness key, a FILTER, a measure or a join through it
/// is refused, naming the fact column.
#[test]
fn a_left_joined_dim_key_is_refused() {
    let named = "z.id is null where no z row matches, and f.z_id is not; \
                 write f.z_id, or join z with INNER JOIN";
    let reads = [
        base("f.z_id AS z_id", "z.id AS z_id"),
        base("f.z_id AS z_id", "(z.id IS NULL) AS orphan"),
        base("count(*) AS n", "count(*) AS n, count(z.id) AS matched"),
        base(
            "count(*) AS n",
            "count(*) AS n, count(*) FILTER (WHERE z.id = 1) AS ones",
        ),
        base(
            "LEFT OUTER JOIN public.z AS z ON z.id = f.z_id",
            "LEFT OUTER JOIN public.z AS z ON z.id = f.z_id\n\
             LEFT OUTER JOIN public.w AS w ON w.id = z.id",
        )
        .replace(
            "z.name AS name, count(*)",
            "z.name AS name, w.label AS label, count(*)",
        )
        .replace("GROUP BY 1, 2, 3, 4", "GROUP BY 1, 2, 3, 4, 5"),
    ];
    for text in reads {
        let errors = error_lines(&text);
        assert!(any_contains(&errors, named), "{text}\n{errors:?}");
    }
    // Under INNER JOIN each read is the fact column, and parses.
    let inner = base("f.z_id AS z_id", "z.id AS z_id").replace("LEFT OUTER JOIN", "INNER JOIN");
    assert_eq!(
        parse(&inner).unwrap().fact("t").unwrap().by_keys[1],
        by_key("z_id")
    );
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

/// No column of an `EXISTS` table is read outside it, its key column
/// neither: the semi or anti join keeps or drops the fact row and holds no
/// row of its own.
#[test]
fn an_exists_table_has_no_column_outside_it() {
    for exists in ["EXISTS", "NOT EXISTS"] {
        let semi = base(
            "\nGROUP BY",
            &format!("\nWHERE {exists} (SELECT 1 FROM public.p AS p WHERE p.k = f.a)\nGROUP BY"),
        );
        assert!(parse(&semi).is_ok(), "{semi}");
        let kind = if exists == "EXISTS" {
            "a semi"
        } else {
            "an anti"
        };
        for (from, to, column) in [
            ("f.a AS a", "p.k AS a", "p.k"),
            ("f.a AS a", "(p.k IS NULL) AS a", "p.k"),
            ("count(*) AS n", "count(*) AS n, sum(p.k) AS total", "p.k"),
            (
                "count(*) AS n",
                "count(*) AS n, count(p.note) AS notes",
                "p.note",
            ),
            (
                "count(*) AS n",
                "count(*) AS n, count(*) FILTER (WHERE p.k = 1) AS ones",
                "p.k",
            ),
        ] {
            let text = edit(&semi, from, to);
            let errors = error_lines(&text);
            let named = format!(
                "{column} is read outside its EXISTS; p is {kind} join, which keeps no row to read"
            );
            assert!(any_contains(&errors, &named), "{text}\n{errors:?}");
        }
    }
}

/// An `EXISTS` is one table and one condition: no GROUP BY, no join.
#[test]
fn exists_groups_by_nothing_and_joins_nothing() {
    rejects(&[
        (
            "GROUP BY 1, 2, 3, 4",
            "WHERE EXISTS (SELECT 1 FROM public.p AS p WHERE p.k = f.a GROUP BY p.k)\n\
             GROUP BY 1, 2, 3, 4",
            6,
            "GROUP BY inside EXISTS is not in Straum SQL",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "WHERE EXISTS (SELECT 1 FROM public.p AS p WHERE p.k = f.a GROUP BY ALL)\n\
             GROUP BY 1, 2, 3, 4",
            6,
            "GROUP BY inside EXISTS is not in Straum SQL",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "WHERE EXISTS (SELECT 1 FROM public.p AS p JOIN public.q AS q ON q.a = p.a \
             WHERE p.k = f.a)\nGROUP BY 1, 2, 3, 4",
            6,
            "a join inside EXISTS is not in Straum SQL",
        ),
    ]);
}

/// The guide's chain: `regions` joined through `accounts`.
const CHAIN: &str = "CREATE VIEW usage WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', e.at, timestamptz 'epoch') AS bucket,
       a.region AS region, r.code AS code, count(*) AS n
FROM public.events AS e
INNER JOIN public.accounts AS a ON a.id = e.account_id
LEFT OUTER JOIN public.regions AS r ON r.name = a.region
GROUP BY 1, 2, 3";

/// A chained dim's key column reads the dim column it is joined to, as
/// a dim's joined to the fact reads the fact column: under INNER JOIN it
/// is that column, under LEFT OUTER JOIN it is refused for it.
#[test]
fn a_chained_key_column_reads_the_column_it_is_joined_to() {
    let named = CHAIN
        .replace("r.code AS code", "r.code AS code, r.name AS region_name")
        .replace("GROUP BY 1, 2, 3", "GROUP BY 1, 2, 3, 4");
    let error = sql_errors(&named);
    assert!(
        any_message(
            &error,
            Some(3),
            "r.name is null where no r row matches, and a.region is not; \
             write a.region, or join r with INNER JOIN"
        ),
        "{error:?}"
    );
    let inner = named.replace(
        "LEFT OUTER JOIN public.regions",
        "INNER JOIN public.regions",
    );
    let spec = parse(&inner).unwrap();
    let reads = &spec.fact("usage").unwrap().refs[0].reads;
    assert_eq!(
        reads,
        &[
            attribute("region", "region"),
            attribute("region_name", "region")
        ]
    );
}

/// One dim column selected under two names is two keys, as a fact
/// column is: the dim holds the column once.
#[test]
fn a_dim_column_may_be_selected_twice() {
    let twice = CHAIN
        .replace("r.code AS code", "r.code AS code, r.code AS code_again")
        .replace("GROUP BY 1, 2, 3", "GROUP BY 1, 2, 3, 4");
    let spec = parse(&twice).unwrap();
    assert_eq!(spec.dim("regions").unwrap().attributes, [id("code")]);
    let reads = &spec.fact("usage").unwrap().refs[1].reads;
    assert_eq!(
        reads,
        &[attribute("code", "code"), attribute("code_again", "code")]
    );
    // Two keys still may not share a name.
    let clash = CHAIN
        .replace("r.code AS code", "r.code AS code, r.name AS code")
        .replace("GROUP BY 1, 2, 3", "GROUP BY 1, 2, 3, 4");
    assert!(parse(&clash).is_err());
}

/// An inner join that selects only its key column reads nothing of its
/// dim, and the error says why.
#[test]
fn a_join_selecting_only_its_key_says_why_it_reads_nothing() {
    let only_key = "CREATE VIEW usage WITH (buckets = '1 hour') AS
SELECT date_bin('1 hour', e.at, timestamptz 'epoch') AS bucket,
       a.id AS account, count(*) AS n
FROM public.events AS e
INNER JOIN public.accounts AS a ON a.id = e.account_id
GROUP BY 1, 2";
    let error = parse(only_key).unwrap_err().to_string();
    assert!(
        error.contains(
            "join a reads no column of its dim; a key column reads the column it is joined to"
        ),
        "{error}"
    );
}
