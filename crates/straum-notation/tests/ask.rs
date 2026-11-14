mod common;

use common::{FEB, JAN, cond, id, strings};
use straum_domain::{Ask, AskKey, Literal, TimeKey};
use straum_notation::parse_ask;

fn key(name: &str) -> AskKey {
    AskKey::Key(id(name))
}

fn text(s: &str) -> Literal {
    Literal::Text(s.into())
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
                AskKey::Time(TimeKey::Bucket),
                key("payment_type"),
                key("pu_location_id"),
                key("borough"),
            ],
            filter: vec![
                cond("borough", vec![text("Manhattan"), text("Queens")]),
                cond("payment_type", vec![Literal::Int(1)]),
                cond("shared", vec![Literal::Bool(false)]),
                cond("vendor_id", vec![Literal::Int(1)]),
            ],
            from: JAN,
            to: FEB,
        }
    );
}

/// Both bounds are written, whichever order; an ask says what it reads.
#[test]
fn both_bounds_are_required() {
    let ask = parse_ask(
        "select TRIPS from Trips where bucket < $2 and bucket >= $1",
        &range(),
    )
    .unwrap();
    assert_eq!(
        (ask.view, ask.measures, ask.keys, ask.from, ask.to),
        (id("trips"), vec![id("trips")], Vec::new(), JAN, FEB)
    );
    let missing = |bound: &str| format!("line 1: an ask writes both bounds; {bound} is missing");
    assert_eq!(
        errors("SELECT trips FROM t", &[]),
        [missing("bucket >= $n"), missing("bucket < $n")]
    );
    assert_eq!(
        errors(
            "SELECT trips FROM t WHERE kind = $1 AND bucket >= $2",
            &[Literal::Int(1), Literal::Int(JAN)]
        ),
        [missing("bucket < $n")]
    );
    assert_eq!(
        errors(
            "SELECT trips FROM t WHERE bucket < $1",
            &[Literal::Int(FEB)]
        ),
        [missing("bucket >= $n")]
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
#[test]
fn every_error_names_what_is_wrong() {
    let cases: &[(&str, &[Literal], &str)] = &[
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
            "SELECT trips FROM t WHERE kind IN ($3) AND bucket >= $1 AND bucket < $2",
            &[Literal::Int(JAN), Literal::Int(FEB), Literal::Int(1)],
            "IN with one value, write =, `kind IN ($3)` is not in Straum SQL",
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
            "SELECT trips FROM t WHERE bucket = $1 AND bucket >= $1 AND bucket < $2",
            &range(),
            "bucket takes only bucket >= $n AND bucket < $m",
        ),
        (
            "SELECT trips FROM t WHERE bucket > $1 AND bucket < $2",
            &range(),
            "filter condition, write = or IN joined by AND, `bucket > $1`",
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
            "filter condition, write = or IN joined by AND, `kind <> $1`",
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
        (
            "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2",
            &[Literal::Bool(true), Literal::Int(FEB)],
            "bucket bound $1 must be RFC 3339 or Unix seconds: is a boolean",
        ),
        (
            "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2",
            &[text("2024-01-01"), Literal::Int(FEB)],
            "bucket bound $1 must be RFC 3339 or Unix seconds: \"2024-01-01\":",
        ),
        (
            "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2",
            &[text("2024-01-01T00:00:00.5Z"), Literal::Int(FEB)],
            "2024-01-01T00:00:00.5Z is not a whole second",
        ),
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
        (
            "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2 HAVING trips = $1",
            &range(),
            "HAVING is not in Straum SQL",
        ),
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
        (
            "SELECT bucket, trips FROM t WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "bucket is selected but not in GROUP BY",
        ),
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
        ("SELECT trips FROM t WHERE", &[], "sql parser error"),
    ];
    for (query, parameters, fragment) in cases {
        let errors = errors(query, parameters);
        assert!(
            errors.iter().any(|e| e.contains(fragment)),
            "{query}: {errors:?} has no {fragment:?}"
        );
    }
}
