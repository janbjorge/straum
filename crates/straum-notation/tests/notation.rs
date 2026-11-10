use straum_domain::{
    Agg, Ask, AskError, AskKey, Cond, Grain, Ident, KeyPath, Literal, Slot, Spec, SpecError,
    TimeKey,
};
use straum_notation::{Error, SqlError, parse};

const TAXI: &str = include_str!("taxi.sql");

// 2024-01-01 and 2024-02-01, 00:00 UTC.
const JAN: i64 = 1_704_067_200;
const FEB: i64 = 1_706_745_600;

fn id(s: &str) -> Ident {
    Ident::new(s).unwrap()
}

fn path(s: &str) -> KeyPath {
    match s.split_once('.') {
        Some((head, attr)) => KeyPath {
            head: id(head),
            attr: Some(id(attr)),
        },
        None => KeyPath {
            head: id(s),
            attr: None,
        },
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
        by: vec![
            AskKey::Key(path("pickup.borough")),
            AskKey::Time(TimeKey::Bucket),
        ],
        filter: vec![Cond {
            path: path("dropoff.borough"),
            values: vec![Literal::Text("Manhattan".into())],
        }],
        from: JAN,
        to: FEB,
    }
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

    let key: Vec<String> = trips
        .rollup_key()
        .map(|slot| match slot {
            Slot::Column(c) => c.to_string(),
            Slot::Ref(r) => r.name.to_string(),
            Slot::Bucket => "bucket".into(),
        })
        .collect();
    assert_eq!(
        key,
        ["payment_type", "vendor_id", "pickup", "dropoff", "bucket"]
    );

    assert_eq!(
        trips.measure("distance").unwrap().agg,
        Agg::Avg(id("trip_distance"))
    );
    assert_eq!(
        trips.measure("card").unwrap().filter,
        [Cond {
            path: path("payment_type"),
            values: vec![Literal::Int(1)]
        }]
    );
    assert_eq!(
        trips.measure("manhattan_pickups").unwrap().filter[0].path,
        path("pickup.borough")
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
            AskKey::Key(path("fare_amount")),
            AskKey::Key(path("pickup.colour")),
            AskKey::Key(path("pickup.borough")),
            AskKey::Key(path("pickup.borough")),
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
            AskError::UnknownKey(path("fare_amount")),
            AskError::UnknownKey(path("pickup.colour")),
            AskError::TimeKeyTwice,
            AskError::AskedTwice("pickup.borough".into()),
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
            AskError::UnknownKey(path("pickup.colour")),
            "pickup.colour is not a by column, ref or ref attribute",
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
            AskKey::Key(path("pickup")),
            AskKey::Key(path("dropoff.location_id")),
            AskKey::Key(path("payment_type")),
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
        filter: vec![Cond {
            path: path("fare_amount"),
            values: vec![Literal::Int(1)],
        }],
        from: FEB,
        to: JAN + 60,
        ..month_ask()
    };
    assert_eq!(
        spec.check(&ask).unwrap_err(),
        [
            AskError::NoMeasures,
            AskError::UnknownKey(path("fare_amount")),
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
const BASE: &str = "CREATE VIEW t WITH (key = id) AS
SELECT date_bin('1 hour', f.at, timestamptz 'epoch') AS bucket,
       f.a, f.z_id, z.name, count(*) AS n
FROM public.t f
LEFT JOIN public.z z ON z.id = f.z_id
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
    assert_eq!(t.key, id("id"));
    assert_eq!(t.by, [id("a")]);
    assert_eq!(t.refs[0].name, id("z"));
    assert_eq!(t.refs[0].via, id("z_id"));
    assert!(t.time.buckets.is_empty());
    let z = spec.dim("z").unwrap();
    assert_eq!((&z.key, &z.attrs[..]), (&id("id"), &[id("name")][..]));
}

#[test]
fn buckets_parse() {
    let src = base("key = id", "key = id, buckets = '1 day,7 days'");
    let spec = parse(&src).unwrap();
    assert_eq!(
        spec.fact("t").unwrap().time.buckets,
        [Grain(86_400), Grain(604_800)]
    );

    let src = base("key = id", "key = id, buckets = '90 minutes, 1 day'");
    assert_eq!(
        spec_errors(&src),
        [SpecError::BadBucket {
            fact: id("t"),
            seconds: 5_400,
            below: 3_600
        }]
    );
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
    assert_eq!(
        spec.fact("t").unwrap().measure("n").unwrap().filter,
        [
            Cond {
                path: path("a"),
                values: vec![Literal::Int(-1), Literal::Int(2)]
            },
            Cond {
                path: path("z.name"),
                values: vec![Literal::Text("it's".into())]
            },
            Cond {
                path: path("z"),
                values: vec![Literal::Int(3)]
            },
            Cond {
                path: path("z"),
                values: vec![Literal::Int(4)]
            },
            Cond {
                path: path("a"),
                values: vec![Literal::Bool(true)]
            },
        ]
    );
}

#[test]
fn every_agg_grain_unit_and_key_form() {
    let src = "CREATE OR REPLACE VIEW T WITH (KEY = ID) AS
SELECT date_bin('30 seconds', f.at, timestamptz 'epoch'),
       f.a, z.id,
       count(*) AS n, sum(f.x) AS s, min(f.x) AS lo, max(f.x) AS hi,
       avg(f.x) AS mean, count(DISTINCT f.x) AS uniq, sum(f.z_id) AS ids
FROM public.t f
LEFT OUTER JOIN public.z z ON f.z_id = z.id
GROUP BY f.a, date_bin('30 seconds', f.at, timestamptz 'epoch'), 3";
    let spec = parse(src).unwrap();
    let t = spec.fact("t").unwrap();
    assert_eq!(t.time.grain, Grain(30));
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
    assert!(spec.dim("z").unwrap().attrs.is_empty());

    for (width, seconds) in [("1 second", 1), ("5 minute", 300), ("1 day", 86_400)] {
        let spec = parse(&base("1 hour", width).replace("1 hour", width)).unwrap();
        assert_eq!(spec.fact("t").unwrap().time.grain, Grain(seconds));
    }
}

#[test]
fn views_share_a_dim() {
    let src = format!(
        "{BASE};\n{}",
        BASE.replace("VIEW t", "VIEW u")
            .replace("z.name", "z.code")
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
    let src = base("'1 hour'", "'7 minutes'").replace("f.a,", "f.id, f.z_id AS z,");
    assert!(matches!(parse(&src), Err(Error::Sql(_))));

    let src = base("'1 hour'", "'7 minutes'")
        .replace("f.a, f.z_id", "f.id, f.z_id")
        .replace("count(*) AS n", "count(*) AS n, count(*) AS n");
    assert_eq!(
        spec_errors(&src),
        [
            SpecError::BadGrain {
                fact: id("t"),
                seconds: 420
            },
            SpecError::KeyInRollup {
                fact: id("t"),
                column: id("id")
            },
            SpecError::Duplicate {
                scope: Some(id("t")),
                name: id("n")
            },
        ]
    );

    let clash = base("f.a,", "f.z,");
    assert_eq!(
        spec_errors(&clash),
        [SpecError::Duplicate {
            scope: Some(id("t")),
            name: id("z")
        }]
    );

    let named_ref = BASE
        .replace("z.name", "bucket.name")
        .replace("public.z z ON z.id", "public.z bucket ON bucket.id");
    for src in [
        base("f.a,", "f.bucket,"),
        base("count(*) AS n", "count(*) AS bucket"),
        named_ref,
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
#[test]
fn rejects_everything_outside_the_subset() {
    let cases = [
        // Statements and view shape.
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
            "VIEW t WITH",
            "VIEW t COPY GRANTS WITH",
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
        // Options.
        (
            " WITH (key = id)",
            "",
            1,
            "view t needs WITH (key = <column>)",
        ),
        (
            "key = id",
            "key = id, colour = 1",
            1,
            "view option `colour = 1` is not in straum SQL",
        ),
        (
            "key = id",
            "key = id, key = id",
            1,
            "view option key given twice",
        ),
        (
            "key = id",
            "key = 'id'",
            1,
            "key `'id'` is not in straum SQL",
        ),
        (
            "key = id",
            "key = id, append = true",
            1,
            "view option `append = true` is not in straum SQL",
        ),
        (
            "key = id",
            "key = id, buckets = 1",
            1,
            "buckets, use '<n> <unit>, ...', `1`",
        ),
        (
            "key = id",
            "key = id, buckets = '1 day, 2 weeks'",
            1,
            "bucket width '2 weeks': use '<n> seconds|minutes|hours|days'",
        ),
        (
            "key = id",
            "key = id, buckets = ''",
            1,
            "bucket width '': use",
        ),
        (
            "key = id",
            "key = id, buckets = '1 day', buckets = '2 days'",
            1,
            "view option buckets given twice",
        ),
        (
            "key = id",
            "key = id, \"key\" = id",
            1,
            "quoted identifier `\"key\"`",
        ),
        (
            "key = id",
            &format!("key = {}", "k".repeat(64)),
            1,
            "bad identifier kkkk",
        ),
        // Query.
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
            "FROM public.t f",
            "FROM public.t(1) f",
            4,
            "a table function is not",
        ),
        (
            "FROM public.t f",
            "FROM public.t(1) WITH ORDINALITY f",
            4,
            "WITH ORDINALITY is not",
        ),
        (
            "FROM public.t f",
            "FROM public.t f WITH (NOLOCK)",
            4,
            "a table hint is not",
        ),
        ("LEFT JOIN", "GLOBAL LEFT JOIN", 5, "GLOBAL is not"),
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
        // Select clauses.
        (
            "SELECT date_bin",
            "SELECT DISTINCT date_bin",
            2,
            "SELECT DISTINCT is not",
        ),
        (
            "\nGROUP BY",
            "\nWHERE f.a = 1\nGROUP BY",
            2,
            "WHERE, filter measures instead,",
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
            "GROUP BY item `ROLLUP (1, 2, 3, 4)` is not",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY ALL",
            2,
            "GROUP BY ALL is not",
        ),
        // FROM and joins.
        (
            "FROM public.t f",
            "FROM public.t f, public.u u",
            2,
            "exactly one FROM table",
        ),
        (
            "FROM public.t f",
            "FROM (SELECT 1) f",
            4,
            "table `(SELECT 1) f` is not",
        ),
        (
            "FROM public.t f",
            "FROM public.t",
            4,
            "table public.t needs an alias",
        ),
        (
            "FROM public.t f",
            "FROM t f",
            4,
            "table t must be schema.table",
        ),
        (
            "FROM public.t f",
            "FROM public.t f(a)",
            4,
            "an alias column list is not",
        ),
        (
            "FROM public.t f",
            "FROM public.t f TABLESAMPLE SYSTEM (10)",
            4,
            "TABLESAMPLE is not",
        ),
        ("LEFT JOIN", "JOIN", 5, "join, use LEFT JOIN,"),
        ("LEFT JOIN", "INNER JOIN", 5, "join, use LEFT JOIN,"),
        ("LEFT JOIN", "FULL JOIN", 5, "join, use LEFT JOIN,"),
        (
            "LEFT JOIN public.z z ON z.id = f.z_id",
            "CROSS JOIN public.z z",
            5,
            "join, use LEFT JOIN,",
        ),
        (
            "ON z.id = f.z_id",
            "USING (id)",
            5,
            "join condition, use ON,",
        ),
        (
            "ON z.id = f.z_id",
            "ON z.id = 1",
            5,
            "join on z.<key> = f.<column>, found z.id = 1",
        ),
        (
            "ON z.id = f.z_id",
            "ON z.id > f.z_id",
            5,
            "join on z.<key> = f.<column>",
        ),
        (
            "ON z.id = f.z_id",
            "ON z.id = z.z_id",
            5,
            "join on z.<key> = f.<column>",
        ),
        (
            "public.z z ON z.id",
            "public.z f ON f.id",
            5,
            "alias f used twice",
        ),
        // SELECT items and columns.
        ("f.a, f.z_id", "*, f.z_id", 3, "SELECT item `*` is not"),
        ("f.a, f.z_id", "f.*, f.z_id", 3, "SELECT item `f.*` is not"),
        (
            "f.a, f.z_id",
            "f.a AS b, f.z_id",
            3,
            "alias on a key column `b` is not",
        ),
        (
            "f.a, f.z_id",
            "a, f.z_id",
            3,
            "qualify column a with its table alias",
        ),
        (
            "f.a, f.z_id",
            "public.f.a, f.z_id",
            3,
            "column, use alias.column, `public.f.a`",
        ),
        ("f.a, f.z_id", "q.a, f.z_id", 3, "unknown table alias q"),
        (
            "f.a, f.z_id",
            "f.a + 1, f.z_id",
            3,
            "column `f.a + 1` is not",
        ),
        (
            "count(*) AS n",
            "count(*)",
            3,
            "name the measure: count(*) AS <name>",
        ),
        (
            ", f.z_id,",
            ",",
            2,
            "select f.z_id so the view groups by ref z",
        ),
        // Aggregates.
        (
            "count(*) AS n",
            "median(f.x) AS n",
            3,
            "aggregate `median(f.x)` is not",
        ),
        (
            "count(*) AS n",
            "count(f.x) AS n",
            3,
            "count, use count(*) or count(DISTINCT column),",
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
        // Measure filters.
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE f.a = 1 OR f.a = 2) AS n",
            3,
            "filter condition, use = or IN joined by AND,",
        ),
        (
            "count(*) AS n",
            "count(*) FILTER (WHERE f.a NOT IN (1, 2)) AS n",
            3,
            "filter condition",
        ),
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
        // The bucket.
        (
            "'1 hour'",
            "'15x'",
            2,
            "date_bin width '15x': use '<n> seconds|minutes|hours|days'",
        ),
        ("'1 hour'", "'1 week'", 2, "date_bin width"),
        ("'1 hour'", "'99999999 days'", 2, "date_bin width"),
        ("'1 hour'", "interval '1 hour'", 2, "date_bin width"),
        (
            "timestamptz 'epoch'",
            "'epoch'",
            2,
            "date_bin origin 'epoch': use timestamptz 'epoch'",
        ),
        (
            "timestamptz 'epoch'",
            "timestamp 'epoch'",
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
            "AS bucket, date_bin('1 day', f.at, timestamptz 'epoch'),",
            2,
            "more than one date_bin bucket",
        ),
        // GROUP BY.
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3",
            2,
            "z.name is selected but not in GROUP BY",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4, f.b",
            2,
            "GROUP BY f.b is not a selected key column",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4, 5",
            2,
            "GROUP BY count(*) is not a selected key column",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4, 9",
            6,
            "GROUP BY 9 is not a SELECT position",
        ),
        (
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 0, 1, 2, 3, 4",
            6,
            "GROUP BY 0 is not a SELECT position",
        ),
    ];
    let mut failed = Vec::new();
    for (from, to, line, fragment) in cases {
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
    let src = base("key = id", "key = id, key = id").replace("f.a,", "a,");
    assert_eq!(
        parse(&src).unwrap_err().to_string(),
        "line 1: view option key given twice\n\
         line 3: qualify column a with its table alias"
    );
    let src = base("count(*) AS n", "count(*) AS n, count(*) AS n");
    assert_eq!(parse(&src).unwrap_err().to_string(), "t: n declared twice");
}
