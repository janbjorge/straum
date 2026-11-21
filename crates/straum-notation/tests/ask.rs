//! Asks that parse into an `Ask`, and the ones refused: each case names
//! what is wrong, from the statement down to one parameter. Values come
//! only as `$n` parameters, and both bucket bounds are written.

mod common;

use common::{FEB, JAN, Wrong, any_contains, assert_each_contains, condition, id, strings};
use straum_domain::{Ask, AskKey, Comparison, Literal, MeasureCondition, Width};
use straum_notation::parse_ask;

fn key(name: &str) -> AskKey {
    AskKey::Key(id(name))
}

fn text(value: &str) -> Literal {
    Literal::Text(value.into())
}

/// The range parameters most cases end on.
fn range() -> Vec<Literal> {
    vec![Literal::Int(JAN), Literal::Int(FEB)]
}

fn errors(query: &str, parameters: &[Literal]) -> Vec<String> {
    strings(&parse_ask(query, parameters).unwrap_err())
}

#[test]
fn reads_every_part() {
    let ask = parse_ask(
        "SELECT bucket, payment_type, pu_location_id, borough, trips, fare
         FROM trips
         WHERE borough IN ($1, $2) AND payment_type = $3
           AND bucket >= $4 AND shared = $5 AND vendor_id = $3 AND bucket < $6
         GROUP BY borough, bucket, pu_location_id, payment_type",
        &[
            text("Manhattan"),
            text("Queens"),
            Literal::Int(1),
            text("2024-01-01T01:00:00+01:00"),
            Literal::Bool(false),
            Literal::Int(FEB),
        ],
    )
    .unwrap();
    assert_eq!(
        ask,
        Ask {
            view: id("trips"),
            measures: vec![id("trips"), id("fare")],
            keys: vec![
                AskKey::Key(id("bucket")),
                key("payment_type"),
                key("pu_location_id"),
                key("borough"),
            ],
            filter: vec![
                condition("borough", vec![text("Manhattan"), text("Queens")]),
                condition("payment_type", vec![Literal::Int(1)]),
                condition("shared", vec![Literal::Bool(false)]),
                condition("vendor_id", vec![Literal::Int(1)]),
            ],
            having: Vec::new(),
            from: Some(JAN),
            to: Some(FEB),
            bounded: Some(id("bucket")),
        }
    );
}

/// The bounds come in either order, and each may be left out: its end
/// is open.
#[test]
fn a_bound_left_out_leaves_its_end_open() {
    let ask = parse_ask(
        "select trips from trips where bucket < $2 and bucket >= $1",
        &range(),
    )
    .unwrap();
    assert_eq!(
        (ask.view, ask.measures, ask.keys, ask.from, ask.to),
        (
            id("trips"),
            vec![id("trips")],
            Vec::new(),
            Some(JAN),
            Some(FEB)
        )
    );
    let every = parse_ask("SELECT trips FROM t", &[]).unwrap();
    assert_eq!((every.from, every.to), (None, None));
    let filtered = parse_ask("SELECT trips FROM t WHERE kind = $1", &[Literal::Int(1)]).unwrap();
    assert_eq!((filtered.from, filtered.to), (None, None));
    let since = parse_ask(
        "SELECT trips FROM t WHERE kind = $1 AND bucket >= $2",
        &[Literal::Int(1), Literal::Int(JAN)],
    )
    .unwrap();
    assert_eq!((since.from, since.to), (Some(JAN), None));
    let until = parse_ask(
        "SELECT trips FROM t WHERE bucket < $1",
        &[Literal::Int(FEB)],
    )
    .unwrap();
    assert_eq!((until.from, until.to), (None, Some(FEB)));
}

/// No WHERE is the one spelling of every row; a condition that is no
/// condition is refused.
#[test]
fn where_true_is_refused() {
    assert_eq!(
        errors("SELECT trips FROM t WHERE true", &[]),
        ["line 1: filter condition, write = or IN joined by AND, \
          `true` is not in Straum SQL, the PostgreSQL subset made only for Straum"]
    );
}

#[test]
fn keys_come_in_select_order() {
    let ask = parse_ask(
        "SELECT borough, kind, trips FROM t
         WHERE bucket >= $1 AND bucket < $2 GROUP BY kind, borough",
        &range(),
    )
    .unwrap();
    assert_eq!(ask.keys, [key("borough"), key("kind")]);
}

/// Each case is one wrong ask; its errors must include the fragment.
fn assert_each_names_what_is_wrong(cases: &[(&str, &[Literal], &str)]) {
    let mut wrong = Vec::with_capacity(cases.len());
    for &(written, parameters, fragment) in cases {
        wrong.push(Wrong {
            written,
            errors: errors(written, parameters),
            fragment,
        });
    }
    assert_each_contains(&wrong);
}

#[test]
fn values_come_as_parameters() {
    assert_each_names_what_is_wrong(&[
        (
            "SELECT trips FROM t WHERE kind = 1 AND bucket >= $1 AND bucket < $2",
            &range(),
            "value, use a parameter such as $1, `1` is not in Straum SQL",
        ),
        (
            "SELECT trips FROM t WHERE kind IN ($3, 'x') AND bucket >= $1 AND bucket < $2",
            &[Literal::Int(JAN), Literal::Int(FEB), Literal::Int(1)],
            "value, use a parameter such as $1, `'x'`",
        ),
        (
            "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2",
            &[Literal::Int(JAN), Literal::Int(FEB), Literal::Int(1)],
            "parameter $3 is not used",
        ),
        (
            "SELECT trips FROM t WHERE kind = $5 AND bucket >= $1 AND bucket < $2",
            &range(),
            "$5 is not a parameter: there are 2, $1 on",
        ),
        (
            "SELECT trips FROM t WHERE kind = $0 AND bucket >= $1 AND bucket < $2",
            &range(),
            "$0 is not a parameter",
        ),
        (
            "SELECT trips FROM t WHERE kind = ? AND bucket >= $1 AND bucket < $2",
            &range(),
            "found: ?",
        ),
        (
            "SELECT trips FROM t WHERE kind = $x AND bucket >= $1 AND bucket < $2",
            &range(),
            "$x is not a parameter",
        ),
    ]);
}

/// A parameter has no leading zero: the error names the one to write, and
/// a name of zeros only is no parameter.
#[test]
fn a_parameter_has_no_leading_zero() {
    let one = [Literal::Int(1)];
    for (name, error) in [
        ("$01", "$01: write $1"),
        ("$007", "$007: write $7"),
        ("$00", "$00 is not a parameter: there are 1, $1 on"),
    ] {
        let query = format!("SELECT trips FROM t WHERE kind = {name}");
        assert_eq!(errors(&query, &one), [format!("line 1: {error}")], "{name}");
    }
    let ask = parse_ask("SELECT trips FROM t WHERE kind = $1", &one).unwrap();
    assert_eq!(ask.filter, [condition("kind", vec![Literal::Int(1)])]);
}

/// A condition written again, the same in key, operator and value, is
/// refused by name in WHERE and HAVING; the same key with another value
/// is read.
#[test]
fn a_condition_comes_once() {
    let two = [text("cab"), text("bus")];
    for (query, error) in [
        (
            "SELECT trips FROM t WHERE kind = $1 AND kind = $1",
            "line 1: kind = $1 given twice",
        ),
        (
            "SELECT trips FROM t WHERE kind IN ($1, $2) AND kind IN ($1, $2)",
            "line 1: kind IN ($1, $2) given twice",
        ),
        (
            "SELECT trips FROM t WHERE kind IN ($1, $2, $1)",
            "line 1: $1 given twice",
        ),
        (
            "SELECT trips FROM t WHERE kind IN ($1, $1)",
            "line 1: $1 given twice",
        ),
        (
            "SELECT trips FROM t HAVING trips > $1 AND trips > $1",
            "line 1: trips > $1 given twice",
        ),
    ] {
        assert_eq!(errors(query, &two), [error], "{query}");
    }
    let ask = parse_ask("SELECT trips FROM t WHERE kind = $1 AND kind = $2", &two).unwrap();
    assert_eq!(ask.filter.len(), 2);
    let ask = parse_ask("SELECT trips FROM t WHERE kind IN ($1, $2)", &two).unwrap();
    assert_eq!(ask.filter, [condition("kind", two.to_vec())]);
    let ask = parse_ask("SELECT trips FROM t HAVING trips > $1 AND trips < $2", &two).unwrap();
    assert_eq!(ask.having.len(), 2);
}

#[test]
fn bucket_bounds_come_once_each() {
    assert_each_names_what_is_wrong(&[
        (
            "SELECT trips FROM t WHERE bucket >= $1 AND bucket >= $1 AND bucket < $2",
            &range(),
            "bucket >= given twice",
        ),
        (
            "SELECT trips FROM t WHERE bucket < $1 AND bucket >= $1 AND bucket < $2",
            &range(),
            "bucket < given twice",
        ),
        (
            "SELECT trips FROM t WHERE bucket >= $1 AND day < $2",
            &range(),
            "the range bounds one time, not bucket and day",
        ),
        (
            "SELECT trips FROM t WHERE bucket > $1 AND bucket < $2",
            &range(),
            "comparison, WHERE takes = or IN on a key, >= and < on the time; \
             compare a measure in HAVING, `bucket > $1`",
        ),
        (
            "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2",
            &[Literal::Bool(true), Literal::Int(FEB)],
            "range bound $1 must be RFC 3339 or Unix seconds: is a boolean",
        ),
        (
            "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2",
            &[text("2024-01-01"), Literal::Int(FEB)],
            "range bound $1 must be RFC 3339 or Unix seconds: \"2024-01-01\":",
        ),
        (
            "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2",
            &[
                Literal::Decimal {
                    units: 17_040_672_005,
                    scale: 1,
                },
                Literal::Int(FEB),
            ],
            "range bound $1 must be RFC 3339 or Unix seconds: 1704067200.5 is not a whole second",
        ),
    ]);
}

/// `bucket` is a name like any other: in upper case it is refused as one,
/// in either bound, and in lowercase it bounds the range.
#[test]
fn bucket_is_written_in_lowercase() {
    for operator in [">=", "<"] {
        let ask = format!("SELECT trips FROM t WHERE BUCKET {operator} $1");
        assert_eq!(
            errors(&ask, &[Literal::Int(JAN)]),
            ["line 1: BUCKET: write names in lowercase, bucket"],
            "{ask}"
        );
    }
    let ask = parse_ask(
        "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2",
        &range(),
    )
    .unwrap();
    assert_eq!((ask.from, ask.to), (Some(JAN), Some(FEB)));
}

/// A bound has one spelling: no fraction of a second, and `Z` for a zero
/// offset.
#[test]
fn a_bound_has_no_fraction_and_writes_z_for_a_zero_offset() {
    let ask = "SELECT trips FROM t WHERE bucket >= $1";
    let form = "write YYYY-MM-DDTHH:MM:SS, then Z or an offset such as +01:00";
    for (bound, error) in [
        ("2026-11-13T09:00:00.000Z", form),
        ("2026-11-13T09:00:00.5Z", form),
        ("2026-11-13T09Z", form),
        ("2026-11-13T23:59:60Z", form),
        ("2026-11-13 09:00:00Z", form),
        ("2026-11-13t09:00:00z", form),
        ("2026-11-13T09:00:00+00:00", "write Z for a zero offset"),
        ("2026-11-13T09:00:00-00:00", "write Z for a zero offset"),
        (
            "2026-02-30T09:00:00Z",
            "parsed date is not valid: parameter 'day' for `2026-02` is invalid, \
             must be in range `1..=28`",
        ),
    ] {
        assert_eq!(
            errors(ask, &[text(bound)]),
            [format!(
                "line 1: range bound $1 must be RFC 3339 or Unix seconds: {bound:?}: {error}"
            )],
        );
    }
    let nine = 1_794_560_400;
    for bound in ["2026-11-13T09:00:00Z", "2026-11-13T10:00:00+01:00"] {
        let at = parse_ask(ask, &[text(bound)]).unwrap();
        assert_eq!(at.from, Some(nine), "{bound}");
    }
    let both = [
        text("2026-11-13T09:00:00Z"),
        text("2026-11-14T00:00:00+01:00"),
    ];
    assert!(parse_ask("SELECT n FROM t WHERE bucket >= $1 AND bucket < $2", &both,).is_ok());
    assert_each_names_what_is_wrong(&[(
        "SELECT N FROM t WHERE bucket >= $1 AND bucket < $2",
        &both,
        "N: write names in lowercase, n",
    )]);
}

#[test]
fn conditions_are_equalities_joined_by_and() {
    assert_each_names_what_is_wrong(&[
        (
            "SELECT trips FROM t WHERE kind IN ($3) AND bucket >= $1 AND bucket < $2",
            &[Literal::Int(JAN), Literal::Int(FEB), Literal::Int(1)],
            "IN with one value, write =, `kind IN ($3)` is not in Straum SQL",
        ),
        (
            "SELECT trips FROM t WHERE kind = $1 OR kind = $1 AND bucket >= $1 AND bucket < $2",
            &range(),
            "filter condition, write = or IN joined by AND, `kind = $1 OR kind = $1",
        ),
        (
            "SELECT trips FROM t WHERE (kind = $1) AND bucket >= $1 AND bucket < $2",
            &range(),
            "parentheses, write the condition without them, `(kind = $1)`",
        ),
        (
            "SELECT trips FROM t WHERE kind = $1 AND (bucket >= $1 AND bucket < $2)",
            &range(),
            "parentheses, write the condition without them, `(bucket >= $1 AND bucket < $2)`",
        ),
        (
            "SELECT trips FROM t WHERE kind NOT IN ($1, $2) AND bucket >= $1 AND bucket < $2",
            &range(),
            "filter condition, write = or IN joined by AND, `kind NOT IN ($1, $2)`",
        ),
        (
            "SELECT trips FROM t WHERE kind <> $1 AND bucket >= $1 AND bucket < $2",
            &range(),
            "comparison, WHERE takes = or IN on a key, >= and < on the time; \
             compare a measure in HAVING, `kind <> $1`",
        ),
        (
            "SELECT trips FROM t WHERE NOT kind = $1 AND bucket >= $1 AND bucket < $2",
            &range(),
            "filter condition, write = or IN joined by AND, `NOT kind = $1`",
        ),
        // A null-ness key is asked for by name, `done = $n`.
        (
            "SELECT trips FROM t WHERE done_at IS NULL AND bucket >= $1 AND bucket < $2",
            &range(),
            "filter condition, write = or IN joined by AND, `done_at IS NULL`",
        ),
        (
            "SELECT trips FROM t WHERE done_at IS NOT NULL AND bucket >= $1 AND bucket < $2",
            &range(),
            "filter condition, write = or IN joined by AND, `done_at IS NOT NULL`",
        ),
    ]);
}

#[test]
fn an_ask_is_one_select_from_one_view() {
    assert_each_names_what_is_wrong(&[
        ("SELECT 1; SELECT 2", &[], "an ask is one SELECT"),
        ("", &[], "an ask is one SELECT"),
        (
            "DELETE FROM t",
            &[],
            "statement `DELETE FROM t` is not in Straum SQL",
        ),
        (
            "SELECT trips FROM t JOIN u ON t.a = u.a WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "join in an ask, read FROM one view, `JOIN u ON t.a = u.a` is not in Straum SQL",
        ),
        (
            "SELECT trips FROM t x WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "alias in an ask, read FROM the view by name, `x` is not in Straum SQL",
        ),
        (
            "SELECT trips FROM t, u WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "an ask reads FROM one view",
        ),
        (
            "SELECT trips WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "an ask reads FROM one view",
        ),
        (
            "SELECT trips FROM public.t WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "view public.t must be a bare name",
        ),
        (
            "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2 ORDER BY trips",
            &range(),
            "ORDER BY is not in Straum SQL",
        ),
        (
            "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2 LIMIT 3",
            &range(),
            "LIMIT or OFFSET is not in Straum SQL",
        ),
        (
            "WITH x AS (SELECT 1) SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "WITH (a CTE) is not in Straum SQL",
        ),
        (
            "SELECT trips FROM t UNION SELECT trips FROM t",
            &[],
            "ask, use one SELECT,",
        ),
        (
            "SELECT DISTINCT trips FROM t WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "SELECT DISTINCT is not in Straum SQL",
        ),
        ("SELECT trips FROM t WHERE", &[], "sql parser error"),
    ]);
}

#[test]
fn select_items_go_by_name() {
    assert_each_names_what_is_wrong(&[
        (
            "SELECT trips AS n FROM t WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "SELECT item `trips AS n` is not in Straum SQL",
        ),
        (
            "SELECT * FROM t WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "SELECT item `*` is not in Straum SQL",
        ),
        (
            "SELECT count(*) FROM t WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "expression, write the name the view gives the key or measure, `count(*)`",
        ),
        // A dim column goes by the name the view gave it, never by
        // `alias.column`.
        (
            "SELECT pickup.borough, trips FROM t WHERE bucket >= $1 AND bucket < $2 \
             GROUP BY pickup.borough",
            &range(),
            "expression, write the name the view gives the key or measure, `pickup.borough`",
        ),
        (
            "SELECT trips FROM t WHERE pickup.borough = $1 AND bucket >= $1 AND bucket < $2",
            &range(),
            "expression, write the name the view gives the key or measure, `pickup.borough`",
        ),
        (
            "SELECT \"Trips\" FROM t WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "quoted identifier `\"Trips\"` is not in Straum SQL",
        ),
    ]);
}

#[test]
fn group_by_names_the_selected_keys() {
    assert_each_names_what_is_wrong(&[
        (
            "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2 GROUP BY kind",
            &range(),
            "GROUP BY kind is not selected",
        ),
        (
            "SELECT kind, trips FROM t WHERE bucket >= $1 AND bucket < $2 GROUP BY kind, kind",
            &range(),
            "GROUP BY kind given twice",
        ),
        (
            "SELECT kind, trips FROM t WHERE bucket >= $1 AND bucket < $2 GROUP BY 1",
            &range(),
            "expression, write the name the view gives the key or measure, `1`",
        ),
        (
            "SELECT trips, kind FROM t WHERE bucket >= $1 AND bucket < $2 GROUP BY kind",
            &range(),
            "select kind before the measures",
        ),
        (
            "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2 GROUP BY ALL",
            &range(),
            "GROUP BY ALL is not in Straum SQL",
        ),
        (
            "SELECT kind, trips FROM t WHERE bucket >= $1 AND bucket < $2 GROUP BY ROLLUP (kind)",
            &range(),
            "is not in Straum SQL",
        ),
    ]);
}

#[test]
fn buckets_asks_rows_at_a_wider_width() {
    let ask = parse_ask(
        "SELECT buckets(bucket, '1 day'), borough, trips FROM trips
         WHERE bucket >= $1 AND bucket < $2
         GROUP BY borough, buckets(bucket, '1 day')",
        &range(),
    )
    .unwrap();
    let day = AskKey::WiderBucket {
        name: id("bucket"),
        width: Width { seconds: 86_400 },
    };
    assert_eq!(ask.keys, [day, key("borough")]);
    // A name is lowercase, a function's too.
    assert_eq!(
        errors(
            &grouped("buckets(bucket, '1 day')", "BUCKETS(bucket, '1 day')"),
            &[]
        ),
        ["line 1: BUCKETS: write names in lowercase, buckets"]
    );
    // The spec decides which widths a view keeps; the parse reads any.
    let weeks = parse_ask(
        "SELECT buckets(bucket, '7 days'), trips FROM trips GROUP BY buckets(bucket, '7 days')",
        &[],
    )
    .unwrap();
    assert_eq!(
        weeks.keys,
        [AskKey::WiderBucket {
            name: id("bucket"),
            width: Width { seconds: 604_800 }
        }]
    );
}

/// An ask that selects `select` and groups by `group_by`.
fn grouped(select: &str, group_by: &str) -> String {
    format!("SELECT {select}, trips FROM t GROUP BY {group_by}")
}

/// `buckets` in an ask is a plain call: FILTER and WITHIN GROUP are
/// refused by name, and the call without them is read.
#[test]
fn buckets_in_an_ask_takes_no_filter_or_within_group() {
    for (call, refused) in [
        (
            "buckets(bucket, '1 day') FILTER (WHERE kind = $1)",
            "FILTER on buckets is not in Straum SQL",
        ),
        (
            "buckets(bucket, '1 day') WITHIN GROUP (ORDER BY kind)",
            "WITHIN GROUP is not in Straum SQL",
        ),
    ] {
        let found = errors(&grouped(call, "kind"), &[text("cab")]);
        assert!(any_contains(&found, refused), "{call}: {found:?}");
    }
    let plain = grouped("buckets(bucket, '1 day')", "buckets(bucket, '1 day')");
    assert!(parse_ask(&plain, &[]).is_ok());
}

#[test]
fn buckets_in_an_ask_is_written_one_way() {
    assert_each_names_what_is_wrong(&[
        (
            &grouped("buckets(bucket, '24 hours')", "buckets(bucket, '24 hours')"),
            &[],
            "bucket width '24 hours': write '1 day'",
        ),
        (
            &grouped("buckets(bucket, $1)", "buckets(bucket, $1)"),
            &[Literal::Text("1 day".into())],
            "bucket width $1: use '<n> seconds|minutes|hours|days'",
        ),
        (
            &grouped("buckets(bucket + 1, '1 day')", "kind"),
            &[],
            "buckets in an ask reads the view's time by name, not bucket + 1",
        ),
        (
            &grouped(
                "buckets(bucket, '1 day', '7 days')",
                "buckets(bucket, '1 day', '7 days')",
            ),
            &[],
            "buckets arguments in an ask, write buckets(<time>, '<width>'),",
        ),
        (
            &grouped("buckets('1 day', bucket)", "kind"),
            &[],
            "buckets in an ask reads the view's time by name, not '1 day'",
        ),
        // PostgreSQL's spelling, in either form, names the one to write.
        (
            &grouped("date_bin('1 day', bucket)", "date_bin('1 day', bucket)"),
            &[],
            "date_bin in an ask: write buckets(<time>, '<width>')",
        ),
        (
            &grouped("date_bin('1 day', bucket, timestamptz 'epoch')", "kind"),
            &[],
            "date_bin in an ask: write buckets(<time>, '<width>')",
        ),
        (
            &grouped("buckets(DISTINCT bucket, '1 day')", "kind"),
            &[],
            "DISTINCT on buckets is not in Straum SQL",
        ),
        (
            &grouped("buckets(bucket, '1 day')", "buckets(bucket, '7 days')"),
            &[],
            "buckets(bucket, '1 day') is selected but not in GROUP BY",
        ),
        (
            &grouped("buckets(bucket, '1 day')", "buckets(bucket, '7 days')"),
            &[],
            "GROUP BY bucket is not selected",
        ),
        (
            "SELECT trips FROM t WHERE buckets(bucket, '1 day') = $1",
            &[Literal::Int(0)],
            "buckets goes only in SELECT and GROUP BY",
        ),
        (
            "SELECT trips FROM t HAVING buckets(bucket, '1 day') > $1",
            &[Literal::Int(0)],
            "buckets goes only in SELECT and GROUP BY",
        ),
        (
            &grouped("lower(bucket)", "lower(bucket)"),
            &[],
            "expression, write the name the view gives the key or measure, `lower(bucket)`",
        ),
    ]);
}

fn compared(measure: &str, comparison: Comparison, value: &Literal) -> MeasureCondition {
    MeasureCondition {
        measure: id(measure),
        comparison,
        value: value.clone(),
    }
}

#[test]
fn having_compares_measures_with_parameters() {
    let ask = parse_ask(
        "SELECT kind, trips, fare FROM t WHERE bucket >= $1 AND bucket < $2 GROUP BY kind
         HAVING trips = $3 AND trips <> $3 AND fare < $4 AND fare <= $4
            AND fare > $4 AND trips >= $3",
        &[
            Literal::Int(JAN),
            Literal::Int(FEB),
            Literal::Int(7),
            Literal::Decimal {
                units: 205,
                scale: 1,
            },
        ],
    )
    .unwrap();
    let seven = Literal::Int(7);
    let fare = Literal::Decimal {
        units: 205,
        scale: 1,
    };
    assert_eq!(
        ask.having,
        [
            compared("trips", Comparison::Equal, &seven),
            compared("trips", Comparison::NotEqual, &seven),
            compared("fare", Comparison::Less, &fare),
            compared("fare", Comparison::LessOrEqual, &fare),
            compared("fare", Comparison::Greater, &fare),
            compared("trips", Comparison::GreaterOrEqual, &seven),
        ]
    );
}

#[test]
fn having_takes_one_spelling() {
    let three = [Literal::Int(JAN), Literal::Int(FEB), Literal::Int(7)];
    let ask = "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2 HAVING ";
    let cases = [
        ("trips != $3", "write <> for not equal, not !="),
        (
            "trips > $3 OR trips < $3",
            "HAVING condition, write a measure, = <> < <= > or >=, and $n, joined by AND, `trips > $3 OR trips < $3`",
        ),
        (
            "(trips > $3)",
            "parentheses, write the condition without them, `(trips > $3)`",
        ),
        (
            "trips IS NULL AND trips = $3",
            "HAVING condition, write a measure, = <> < <= > or >=, and $n, joined by AND, `trips IS NULL`",
        ),
        (
            "trips + 1 > $3",
            "expression, write the name the view gives the key or measure, `trips + 1`",
        ),
        (
            "count(*) > $3",
            "expression, write the name the view gives the key or measure, `count(*)`",
        ),
        (
            "$3 < trips",
            "expression, write the name the view gives the key or measure, `$3`",
        ),
        (
            "trips > 7 AND trips > $3",
            "value, use a parameter such as $1, `7`",
        ),
        ("trips > $4", "$4 is not a parameter: there are 3, $1 on"),
    ];
    for (having, error) in cases {
        let query = format!("{ask}{having}");
        let found = errors(&query, &three);
        assert!(any_contains(&found, error), "{query}: {found:?}");
    }
    // A refused != still uses its parameter.
    assert_eq!(
        errors(&format!("{ask}trips != $3"), &three),
        ["line 1: write <> for not equal, not !="]
    );
}

/// A refused ask names only what is wrong with it: a parameter it never
/// reached is not reported unused, and a comment is refused as itself,
/// whatever it holds.
#[test]
fn a_refused_ask_names_only_its_fault() {
    let three = [Literal::Int(JAN), Literal::Int(FEB), Literal::Int(7)];
    for refused in [
        "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2 AND (kind = $3)",
        "DELETE FROM t WHERE kind = $3",
    ] {
        let found = errors(refused, &three);
        assert_eq!(found.len(), 1, "{refused}: {found:?}");
        assert!(!any_contains(&found, "is not used"), "{refused}: {found:?}");
    }
    let ask = "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2 HAVING ";
    // A comment is refused as itself, not read for what it says.
    for having in ["trips /* != */ <> $3", "trips -- !=\n <> $3"] {
        let query = format!("{ask}{having}");
        assert_eq!(
            errors(&query, &three),
            [
                "line 1: a comment, write the ask without it, is not in Straum SQL, \
              the PostgreSQL subset made only for Straum"
            ],
            "{query}"
        );
    }
    // Unused, in an ask with nothing else wrong.
    let unused = "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2";
    assert_eq!(errors(unused, &three), ["parameter $3 is not used"]);
}

/// A comment or a trailing `;` is refused as any construct is, saying
/// Straum SQL is Straum's own subset; the ask without it parses.
#[test]
fn a_comment_or_a_trailing_semicolon_is_not_in_straum_sql() {
    for (query, what) in [
        ("SELECT trips FROM t -- trips", "a comment"),
        ("SELECT trips FROM t;", "a ;"),
    ] {
        assert_eq!(
            errors(query, &[]),
            [format!(
                "line 1: {what}, write the ask without it, is not in Straum SQL, \
                 the PostgreSQL subset made only for Straum"
            )],
        );
    }
    assert!(parse_ask("SELECT trips FROM t", &[]).is_ok());
}

/// An ask with `terms` more conditions after the first, each on a key
/// of its own, since a condition comes once.
fn chain(terms: usize) -> String {
    let mut conditions = vec!["kind = $1".to_owned()];
    for term in 0..terms {
        conditions.push(format!("kind_{term} = $1"));
    }
    format!("SELECT trips FROM t WHERE {}", conditions.join(" AND "))
}

/// A chain of operators is a tree as deep as the chain, which reading it
/// recurses down: past 512 operators in one statement the ask is refused
/// before it is parsed, whatever the operators, and a chain within it is
/// read.
#[test]
fn a_long_chain_of_operators_is_refused_before_it_is_parsed() {
    let refused = "more than 512 operators in one statement; Straum SQL takes fewer";
    // 1 + 2 per AND: 511 operators, then 513.
    let ask = parse_ask(&chain(255), &[text("cab")]).unwrap();
    assert_eq!(ask.filter.len(), 256);
    assert_eq!(
        errors(&chain(256), &[text("cab")]),
        [format!("line 1: {refused}")]
    );
    for deep in [
        chain(2_500),
        format!("SELECT {} AS n FROM t", vec!["1"; 3_000].join(" + ")),
        format!(
            "SELECT trips FROM t HAVING trips > $1{}",
            " OR trips > $1".repeat(3_000)
        ),
        format!("SELECT trips FROM t WHERE kind{}", " IS NULL".repeat(3_000)),
        format!("SELECT trips FROM t WHERE kind{}", "::text".repeat(3_000)),
    ] {
        let found = errors(&deep, &[text("cab")]);
        assert_eq!(found, [format!("line 1: {refused}")], "{}", &deep[..60]);
    }
}

/// An ask that names `$1` `times`, each on a key of its own: one IN list
/// takes each value once.
fn named(times: usize) -> String {
    let mut conditions = Vec::with_capacity(times);
    for index in 0..times {
        conditions.push(format!("k{index} = $1"));
    }
    format!("SELECT trips FROM t WHERE {}", conditions.join(" AND "))
}

/// Each use of a parameter is a copy of it, so the text they come to is
/// bounded by what one ask's body holds: a long text named many times is
/// refused once, a short one named many times is read.
#[test]
fn the_text_parameters_come_to_at_most_a_mebibyte() {
    let long = text(&"x".repeat(50_000));
    let ask = parse_ask(&named(20), std::slice::from_ref(&long)).unwrap();
    assert_eq!(ask.filter.len(), 20);
    assert_eq!(
        errors(&named(200), &[long]),
        [
            "line 1: the text parameters, as the ask uses them, come to more than 1048576 bytes; \
          use each once"
        ]
    );
    let ask = parse_ask(&named(200), &[text("cab")]).unwrap();
    assert_eq!(ask.filter.len(), 200);
}

/// An ask keeps its first 50 errors and counts the rest, so its reply
/// stays small however many mistakes it holds.
#[test]
fn an_ask_says_its_first_fifty_errors_and_counts_the_rest() {
    let items = vec!["1"; 256].join(", ");
    let found = errors(&format!("SELECT {items} FROM t"), &[]);
    assert_eq!(found.len(), 51, "{found:?}");
    assert_eq!(found[50], "and 206 more errors");
    let few = errors("SELECT 1, 1 FROM t", &[]);
    assert_eq!(few.len(), 2, "{few:?}");
}

/// `count` names, `a0` on, as a SELECT or GROUP BY list.
fn names(count: usize) -> String {
    let mut names = Vec::new();
    for index in 0..count {
        names.push(format!("a{index}"));
    }
    names.join(", ")
}

/// An ask names at most 256 items in SELECT and in GROUP BY, each name
/// once: past that it is refused whole, before any name is read.
#[test]
fn an_ask_names_at_most_256_items() {
    let refused = "line 1: an ask selects and groups by at most 256 names each";
    let many = names(20_000);
    let wide = format!("SELECT {many}, trips FROM t GROUP BY {many}");
    assert_eq!(errors(&wide, &[]), [refused]);
    let selected = format!("SELECT {}, trips FROM t", names(256));
    assert_eq!(errors(&selected, &[]), [refused]);
    let allowed = format!("SELECT {} FROM t", names(256));
    let ask = parse_ask(&allowed, &[]).unwrap();
    assert_eq!(ask.measures.len(), 256);
}
