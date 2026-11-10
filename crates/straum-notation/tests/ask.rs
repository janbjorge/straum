use straum_domain::{Ask, AskKey, Cond, Ident, KeyPath, Literal, TimeKey};
use straum_notation::parse_ask;

// 2024-01-01 and 2024-02-01, 00:00 UTC.
const JAN: i64 = 1_704_067_200;
const FEB: i64 = 1_706_745_600;

fn id(s: &str) -> Ident {
    Ident::new(s).unwrap()
}

fn key(head: &str, attr: Option<&str>) -> AskKey {
    AskKey::Key(KeyPath {
        head: id(head),
        attr: attr.map(id),
    })
}

fn text(s: &str) -> Literal {
    Literal::Text(s.into())
}

/// The range parameters most cases end on.
fn range() -> Vec<Literal> {
    vec![Literal::Int(JAN), Literal::Int(FEB)]
}

fn errors(query: &str, parameters: &[Literal]) -> Vec<String> {
    let errors = parse_ask(query, parameters).unwrap_err();
    errors.iter().map(ToString::to_string).collect()
}

#[test]
fn reads_every_part() {
    let ask = parse_ask(
        "SELECT bucket, payment_type, pickup, pickup.borough, trips, fare
         FROM trips
         WHERE (pickup.borough IN ($1, $2) AND payment_type = $3)
           AND bucket >= $4 AND shared = $5 AND vendor_id = $3 AND bucket < $6
         GROUP BY pickup.borough, bucket, pickup, payment_type",
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
            fact: id("trips"),
            measures: vec![id("trips"), id("fare")],
            by: vec![
                AskKey::Time(TimeKey::Bucket),
                key("payment_type", None),
                key("pickup", None),
                key("pickup", Some("borough")),
            ],
            filter: vec![
                Cond {
                    path: KeyPath {
                        head: id("pickup"),
                        attr: Some(id("borough")),
                    },
                    values: vec![text("Manhattan"), text("Queens")],
                },
                Cond {
                    path: KeyPath {
                        head: id("payment_type"),
                        attr: None,
                    },
                    values: vec![Literal::Int(1)],
                },
                Cond {
                    path: KeyPath {
                        head: id("shared"),
                        attr: None,
                    },
                    values: vec![Literal::Bool(false)],
                },
                Cond {
                    path: KeyPath {
                        head: id("vendor_id"),
                        attr: None,
                    },
                    values: vec![Literal::Int(1)],
                },
            ],
            from: JAN,
            to: FEB,
        }
    );
}

#[test]
fn measures_alone_and_keys_in_select_order() {
    let ask = parse_ask(
        "select TRIPS from Trips where bucket < $2 and bucket >= $1",
        &range(),
    )
    .unwrap();
    assert_eq!(
        (ask.fact, ask.measures, ask.by, ask.from, ask.to),
        (id("trips"), vec![id("trips")], Vec::new(), JAN, FEB)
    );

    let ask = parse_ask(
        "SELECT zone.borough, kind, trips FROM t
         WHERE bucket >= $1 AND bucket < $2 GROUP BY kind, zone.borough",
        &range(),
    )
    .unwrap();
    assert_eq!(ask.by, [key("zone", Some("borough")), key("kind", None)]);
}

/// Each case is one wrong ask; its errors must include the fragment.
#[test]
fn every_error_names_what_is_wrong() {
    let cases: &[(&str, &[Literal], &str)] = &[
        (
            "SELECT trips FROM t WHERE kind = 1 AND bucket >= $1 AND bucket < $2",
            &range(),
            "value, use a parameter such as $1, `1` is not in straum SQL",
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
        (
            "SELECT trips FROM t",
            &[],
            "an ask needs WHERE bucket >= $n AND bucket < $m",
        ),
        (
            "SELECT trips FROM t WHERE bucket >= $1",
            &[Literal::Int(JAN)],
            "an ask needs WHERE bucket >= $n AND bucket < $m",
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
            "filter condition, use = or IN joined by AND, `bucket > $1`",
        ),
        (
            "SELECT trips FROM t WHERE (kind = $1 OR kind = $1) AND bucket >= $1 AND bucket < $2",
            &range(),
            "filter condition, use = or IN joined by AND, `kind = $1 OR kind = $1`",
        ),
        (
            "SELECT trips FROM t WHERE kind NOT IN ($1) AND bucket >= $1 AND bucket < $2",
            &range(),
            "filter condition, use = or IN joined by AND, `kind NOT IN ($1)`",
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
            "statement `DELETE FROM t` is not in straum SQL",
        ),
        (
            "SELECT trips FROM t JOIN u ON t.a = u.a WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "an ask reads FROM one view by name, with no alias or join",
        ),
        (
            "SELECT trips FROM t x WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "an ask reads FROM one view by name, with no alias or join",
        ),
        (
            "SELECT trips FROM t, u WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "an ask reads FROM one view by name, with no alias or join",
        ),
        (
            "SELECT trips WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "an ask reads FROM one view by name, with no alias or join",
        ),
        (
            "SELECT trips FROM public.t WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "view public.t must be a bare name",
        ),
        (
            "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2 ORDER BY trips",
            &range(),
            "ORDER BY is not in straum SQL",
        ),
        (
            "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2 LIMIT 3",
            &range(),
            "LIMIT or OFFSET is not in straum SQL",
        ),
        (
            "WITH x AS (SELECT 1) SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "WITH (a CTE) is not in straum SQL",
        ),
        (
            "SELECT trips FROM t UNION SELECT trips FROM t",
            &[],
            "ask, use one SELECT,",
        ),
        (
            "SELECT DISTINCT trips FROM t WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "SELECT DISTINCT is not in straum SQL",
        ),
        (
            "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2 HAVING trips = $1",
            &range(),
            "HAVING is not in straum SQL",
        ),
        (
            "SELECT trips AS n FROM t WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "SELECT item `trips AS n` is not in straum SQL",
        ),
        (
            "SELECT * FROM t WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "SELECT item `*` is not in straum SQL",
        ),
        (
            "SELECT count(*) FROM t WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "expression, use column, ref or ref.attribute, `count(*)`",
        ),
        (
            "SELECT a.b.c FROM t WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "name, use column, ref or ref.attribute, `a.b.c`",
        ),
        (
            "SELECT \"Trips\" FROM t WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "quoted identifier `\"Trips\"` is not in straum SQL",
        ),
        (
            "SELECT zone.borough, trips FROM t WHERE bucket >= $1 AND bucket < $2",
            &range(),
            "zone.borough is selected but not in GROUP BY",
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
            "SELECT trips, kind FROM t WHERE bucket >= $1 AND bucket < $2 GROUP BY kind",
            &range(),
            "select kind before the measures",
        ),
        (
            "SELECT trips FROM t WHERE bucket >= $1 AND bucket < $2 GROUP BY ALL",
            &range(),
            "GROUP BY ALL is not in straum SQL",
        ),
        (
            "SELECT kind, trips FROM t WHERE bucket >= $1 AND bucket < $2 GROUP BY ROLLUP (kind)",
            &range(),
            "is not in straum SQL",
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
