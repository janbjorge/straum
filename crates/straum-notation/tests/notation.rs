//! Specs that parse, read back as the domain's dims and facts: the taxi
//! spec, `BASE` and the edits to it that stay valid, every aggregate,
//! width and key form, joins of every kind; then the errors that are no
//! walker's to name: other statements, the parser's own, and how errors
//! print.

mod common;

use common::{
    BASE, ask_view_t, attribute, base, by_key, condition, described, edit, id, key_text,
    spec_errors, sql_errors,
};
use straum_domain::{Aggregate, ByKey, Fraction, Literal, RefKind, Spec, SpecError, Via, Width};
use straum_notation::{SqlError, parse};

const TAXI: &str = include_str!("taxi.sql");

fn taxi() -> Spec {
    parse(TAXI).unwrap()
}

#[test]
fn taxi_parses() {
    let spec = taxi();
    let trips = spec.fact("trips").unwrap();
    assert_eq!(trips.time.width, Width { seconds: 900 });
    assert_eq!(
        spec.dim("taxi_zones").unwrap().attributes,
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
        trips.refs[0].reads,
        [
            attribute("borough", "borough"),
            attribute("zone", "zone"),
            attribute("service_zone", "service_zone")
        ]
    );
    assert_eq!(
        trips.measure("distance").unwrap().aggregate,
        Aggregate::Avg(id("trip_distance"))
    );
    assert_eq!(
        trips.measure("card").unwrap().filter,
        [condition("payment_type", vec![Literal::Int(1)])]
    );
    assert_eq!(
        trips.measure("manhattan_pickups").unwrap().filter[0].key,
        id("borough")
    );
}

#[test]
fn base_parses() {
    let spec = parse(BASE).unwrap();
    let fact = spec.fact("t").unwrap();
    assert_eq!(fact.table.schema, id("public"));
    assert_eq!(fact.by_keys, [by_key("a"), by_key("z_id")]);
    assert_eq!(fact.refs[0].alias, id("z"));
    assert_eq!(fact.refs[0].via, [Via::Fact(id("z_id"))]);
    assert_eq!(fact.refs[0].reads, [attribute("name", "name")]);
    assert_eq!(fact.time.name, id("bucket"));
    assert_eq!(fact.time.column, id("at"));
    assert_eq!(fact.time.wider, []);
    let z = spec.dim("z").unwrap();
    assert_eq!(
        (&z.key[..], &z.attributes[..]),
        (&[id("id")][..], &[id("name")][..])
    );
}

/// A key goes by the name `AS` gives it, which may differ from the
/// column's; asks use that name, while a FILTER names the column, as
/// PostgreSQL reads the view.
#[test]
fn keys_go_by_their_select_name() {
    let text = base(
        "f.a AS a, f.z_id AS z_id, z.name AS name",
        "f.a AS kind, f.z_id AS zone, z.name AS label",
    )
    .replace(
        "count(*) AS n",
        "count(*) AS n, count(*) FILTER (WHERE f.a = 1 AND z.name = 'x') AS some",
    );
    let spec = parse(&text).unwrap();
    let fact = spec.fact("t").unwrap();
    assert_eq!(
        fact.by_keys,
        [
            ByKey::Column {
                column: id("a"),
                name: id("kind")
            },
            ByKey::Column {
                column: id("z_id"),
                name: id("zone")
            }
        ]
    );
    assert_eq!(fact.refs[0].reads, [attribute("label", "name")]);
    assert_eq!(
        fact.measure("some").unwrap().filter,
        [
            condition("kind", vec![Literal::Int(1)]),
            condition("label", vec![Literal::Text("x".into())])
        ]
    );
    assert!(
        spec.check_ask(&ask_view_t(&["kind", "zone", "label"]))
            .is_ok()
    );
    let shown = described(
        &spec
            .check_ask(&ask_view_t(&["a", "name", "z"]))
            .unwrap_err(),
    );
    assert_eq!(
        shown,
        [
            "a is not a key of the view; the view's keys are bucket, kind, zone, label",
            "name is not a key of the view; the view's keys are bucket, kind, zone, label",
            "z is not a key of the view; the view's keys are bucket, kind, zone, label"
        ]
    );
}

#[test]
fn buckets_parse() {
    let text = base("'1 hour')", "'1 hour', '1 day', '7 days')");
    let spec = parse(&text).unwrap();
    assert_eq!(
        spec.fact("t").unwrap().time.wider,
        [Width { seconds: 86_400 }, Width { seconds: 604_800 }]
    );

    // Any number of widths, each a whole multiple of the one before.
    let text = base(
        "buckets(f.at, '1 hour')",
        "buckets(f.at, '15 minutes', '1 hour', '1 day', '7 days', '28 days')",
    );
    let many = parse(&text).unwrap();
    let time = &many.fact("t").unwrap().time;
    assert_eq!(time.width, Width { seconds: 900 });
    assert_eq!(
        time.wider,
        [
            Width { seconds: 3_600 },
            Width { seconds: 86_400 },
            Width { seconds: 604_800 },
            Width { seconds: 2_419_200 },
        ]
    );

    let text = base("'1 hour')", "'1 hour', '90 minutes', '1 day')");
    assert_eq!(
        spec_errors(&text),
        [SpecError::BadBucket {
            fact: id("t"),
            seconds: 5_400,
            below: 3_600
        }]
    );
}

/// The view names its time: the name is the view's own, and asks use it.
#[test]
fn the_time_takes_any_name() {
    let spec = parse(&base("AS bucket,", "AS pickup,")).unwrap();
    let time = &spec.fact("t").unwrap().time;
    assert_eq!(time.name, id("pickup"));
    assert_eq!(time.column, id("at"));
}

/// Every width is said once, in the `buckets` item: a view without one,
/// with two, with no width, or with `WITH` options is refused.
#[test]
fn buckets_are_required_once_with_a_width() {
    let without = base("buckets(f.at, '1 hour') AS bucket,", "").replace("1, 2, 3, 4", "1, 2, 3");
    assert_eq!(
        sql_errors(&without)[0].message,
        "SELECT needs its buckets: buckets(<column>, '<n> <unit>', ...) AS <name>, \
         every width kept, narrowest first"
    );
    let twice = base("AS bucket,", "AS bucket, buckets(f.at, '1 day') AS day,")
        .replace("1, 2, 3, 4", "1, 2, 3, 4, 5");
    assert_eq!(
        sql_errors(&twice)[0].message,
        "SELECT has more than one buckets item"
    );
    let no_width = base("buckets(f.at, '1 hour')", "buckets(f.at)");
    assert_eq!(
        sql_errors(&no_width)[0].message,
        "buckets needs a width after the column: buckets(<column>, '<n> <unit>', ...)"
    );
    let options = base("VIEW t AS", "VIEW t WITH (buckets = '1 hour') AS");
    assert!(
        sql_errors(&options)[0].message.contains("view options"),
        "{:?}",
        sql_errors(&options)
    );
    let bad_width = base("'1 hour')", "'1 hour', '60 minutes')");
    assert_eq!(
        sql_errors(&bad_width)[0].message,
        "bucket width '60 minutes': write '1 hour'"
    );
}

/// A spec may hold comments; one with no view is refused, as no spec
/// Straum could run.
#[test]
fn comments_and_empty_file() {
    let spec = parse(&format!("-- the trips\n{BASE}\n/* end */")).unwrap();
    assert!(spec.fact("t").is_some());
    let error = parse("-- nothing declared yet\n\n")
        .unwrap_err()
        .to_string();
    assert_eq!(
        error,
        "a spec is one or more CREATE VIEW statements; this has none"
    );
}

#[test]
fn literals_and_filters() {
    let text = base(
        "count(*) AS n",
        "count(*) FILTER (WHERE f.a IN (-1, 2) AND z.name = 'it''s' AND f.z_id = 3 \
         AND z.id = 4 AND f.a = true) AS n",
    );
    // An inner-joined dim's key column names the fact column joined to it.
    let text = edit(&text, "LEFT OUTER JOIN", "INNER JOIN");
    let spec = parse(&text).unwrap();
    assert_eq!(
        spec.fact("t").unwrap().measure("n").unwrap().filter,
        [
            condition("a", vec![Literal::Int(-1), Literal::Int(2)]),
            condition("name", vec![Literal::Text("it's".into())]),
            condition("z_id", vec![Literal::Int(3)]),
            condition("z_id", vec![Literal::Int(4)]),
            condition("a", vec![Literal::Bool(true)]),
        ]
    );
}

#[test]
fn a_column_measure_takes_a_filter() {
    let text = base(
        "count(*) AS n",
        "count(*) AS n, sum(f.v) FILTER (WHERE f.a = 1) AS s",
    );
    let spec = parse(&text).unwrap();
    assert_eq!(
        spec.fact("t").unwrap().measure("s").unwrap().filter,
        [condition("a", vec![Literal::Int(1)])]
    );
    let errors = sql_errors(&base(
        "count(*) AS n",
        "count(*) AS n, sum(f.v) FILTER (WHERE f.a > 1) AS s",
    ));
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(
        errors[0]
            .message
            .starts_with("filter condition, write = or IN joined by AND, `f.a > 1`"),
        "{errors:?}"
    );
}

#[test]
fn every_agg_grain_unit_and_key_form() {
    let text = "CREATE VIEW t AS
SELECT buckets(f.at, '30 seconds') AS bucket,
       f.a AS a, z.id AS z_id, z.name AS name,
       count(*) AS n, sum(f.x) AS s, min(f.x) AS lo, max(f.x) AS hi,
       avg(f.x) AS mean, count(DISTINCT f.x) AS uniq, sum(f.z_id) AS ids,
       count(f.x) AS known, bool_and(f.a) AS every_a, bool_or(f.a) AS any_a,
       var_samp(f.x) AS var_s, var_pop(f.x) AS var_p,
       stddev_samp(f.x) AS sd_s, stddev_pop(f.x) AS sd_p,
       mode() WITHIN GROUP (ORDER BY f.x) AS common,
       sum(DISTINCT f.x) AS uniq_sum, avg(DISTINCT f.x) AS uniq_avg
FROM public.t AS f
INNER JOIN public.z AS z ON z.id = f.z_id
GROUP BY 1, 2, 3, 4";
    let spec = parse(text).unwrap();
    let fact = spec.fact("t").unwrap();
    assert_eq!(fact.time.width, Width { seconds: 30 });
    // An inner-joined dim's key column names the fact column joined to it.
    assert_eq!(fact.by_keys, [by_key("a"), by_key("z_id")]);
    let mut aggregates = Vec::new();
    for measure in &fact.measures {
        aggregates.push(&measure.aggregate);
    }
    assert_eq!(
        aggregates,
        [
            &Aggregate::Count,
            &Aggregate::Sum(id("x")),
            &Aggregate::Min(id("x")),
            &Aggregate::Max(id("x")),
            &Aggregate::Avg(id("x")),
            &Aggregate::CountDistinct(id("x")),
            &Aggregate::Sum(id("z_id")),
            &Aggregate::CountNotNull(id("x")),
            &Aggregate::BoolAnd(id("a")),
            &Aggregate::BoolOr(id("a")),
            &Aggregate::VarSamp(id("x")),
            &Aggregate::VarPop(id("x")),
            &Aggregate::StddevSamp(id("x")),
            &Aggregate::StddevPop(id("x")),
            &Aggregate::Mode(id("x")),
            &Aggregate::SumDistinct(id("x")),
            &Aggregate::AvgDistinct(id("x")),
        ]
    );
    assert_eq!(spec.dim("z").unwrap().attributes, [id("name")]);

    for (width, seconds) in [("1 second", 1), ("5 minutes", 300), ("1 day", 86_400)] {
        let spec = parse(&base("1 hour", width).replace("1 hour", width)).unwrap();
        assert_eq!(spec.fact("t").unwrap().time.width, Width { seconds });
    }
}

/// The fraction as written, digits and places, in its one spelling.
#[test]
fn percentile_disc_takes_a_fraction_and_one_column() {
    let cases = [
        ("0.5", 5, 1),
        ("0.99", 99, 2),
        ("0", 0, 0),
        ("1", 1, 0),
        ("0.0000000000000000001", 1, 19),
    ];
    for (written, units, places) in cases {
        let measure = format!("percentile_disc({written}) WITHIN GROUP (ORDER BY f.x) AS n");
        let spec = parse(&base("count(*) AS n", &measure)).unwrap();
        assert_eq!(
            spec.fact("t").unwrap().measures[0].aggregate,
            Aggregate::PercentileDisc {
                fraction: Fraction::new(units, places).unwrap(),
                column: id("x"),
            },
            "{written}"
        );
    }
    let filtered = "percentile_disc(0.9) WITHIN GROUP (ORDER BY f.x) \
                    FILTER (WHERE f.a = 1) AS n";
    let spec = parse(&base("count(*) AS n", filtered)).unwrap();
    assert_eq!(
        spec.fact("t").unwrap().measures[0].filter,
        [condition("a", vec![Literal::Int(1)])]
    );
}

/// What a ref joins and reads, for comparing.
#[derive(Debug, PartialEq)]
struct ShownRef {
    alias: String,
    kind: RefKind,
    via: Vec<Via>,
    reads: usize,
}

fn shown_ref(alias: &str, kind: RefKind, via: Vec<Via>, reads: usize) -> ShownRef {
    ShownRef {
        alias: alias.into(),
        kind,
        via,
        reads,
    }
}

/// A via column read through the first ref, `z`.
fn through_z(column: &str) -> Via {
    Via::Attribute {
        through: 0,
        column: id(column),
    }
}

#[test]
fn joins_of_every_kind_parse() {
    // Inner and left outer joins, a join through a dim, and both EXISTS
    // forms, in one view.
    let text = "CREATE VIEW t AS
SELECT buckets(f.at, '1 hour') AS bucket,
       f.a AS a, z.name AS name, w.label AS label, count(*) AS n
FROM public.t AS f
INNER JOIN public.z AS z ON z.id = f.z_id
LEFT OUTER JOIN public.w AS w ON w.id = z.w_id
WHERE EXISTS (SELECT 1 FROM public.p AS p WHERE p.k = f.a)
  AND NOT EXISTS (SELECT 1 FROM public.b AS b WHERE b.x = f.a AND b.y = z.name)
  AND f.a IN (1, 2) AND z.name = 'x'
GROUP BY 1, 2, 3, 4";
    let spec = parse(text).unwrap();
    let fact = spec.fact("t").unwrap();
    let mut shown = Vec::new();
    for reference in &fact.refs {
        shown.push(ShownRef {
            alias: reference.alias.to_string(),
            kind: reference.kind,
            via: reference.via.clone(),
            reads: reference.reads.len(),
        });
    }
    assert_eq!(
        shown,
        [
            shown_ref("z", RefKind::Inner, vec![Via::Fact(id("z_id"))], 1),
            shown_ref("w", RefKind::Left, vec![through_z("w_id")], 1),
            shown_ref("p", RefKind::Semi, vec![Via::Fact(id("a"))], 0),
            shown_ref(
                "b",
                RefKind::Anti,
                vec![Via::Fact(id("a")), through_z("name")],
                0
            ),
        ]
    );
    // The dim joined through carries the column read for the key.
    assert_eq!(spec.dim("z").unwrap().attributes, [id("name"), id("w_id")]);
    assert_eq!(spec.dim("b").unwrap().key, [id("x"), id("y")]);
    assert_eq!(
        fact.filter,
        [
            condition("a", vec![Literal::Int(1), Literal::Int(2)]),
            condition("name", vec![Literal::Text("x".into())]),
        ]
    );
    // Only the fact-side join columns key the cells.
    assert_eq!(key_text(&spec, "t"), ["a", "z_id", "bucket"]);
    assert_eq!(fact.columns(), [id("at"), id("a"), id("z_id")]);
}

#[test]
fn other_statements_are_rejected() {
    assert_eq!(
        sql_errors("SELECT 1;\nCREATE TABLE t (a int)"),
        [
            SqlError {
                line: Some(1),
                message:
                    "statement `SELECT 1` is not in Straum SQL, the PostgreSQL subset made only for Straum"
                        .into()
            },
            SqlError {
                line: Some(2),
                message: "statement `CREATE TABLE t (a INT)` is not in Straum SQL, \
                      the PostgreSQL subset made only for Straum"
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
    assert!(
        errors[0].message.contains("f.a = 1 OR f.bbbb"),
        "{errors:?}"
    );
    assert!(errors[0].message.contains("...`"), "{errors:?}");
}

#[test]
fn parser_errors_and_deep_nesting_are_errors() {
    let errors = sql_errors("CREATE VIEW t AS SELEC 1");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].line, None);
    assert!(errors[0].message.contains("Line: 1"), "{errors:?}");
    assert_eq!(
        parse("CREATE VIEW t AS SELEC 1").unwrap_err().to_string(),
        errors[0].message
    );

    // The tokenizer's own error comes as the parser's would.
    let errors = sql_errors("CREATE VIEW t AS SELECT 'unclosed");
    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].line, None);
    assert!(
        errors[0].message.contains("Unterminated string"),
        "{errors:?}"
    );

    let deep = format!("{}1{}", "(".repeat(500), ")".repeat(500));
    let errors = sql_errors(&base(
        "count(*) AS n",
        &format!("count(*) FILTER (WHERE f.a = {deep}) AS n"),
    ));
    assert!(errors[0].message.contains("recursion limit"), "{errors:?}");
}

#[test]
fn error_display() {
    let text = base("'1 hour'", "'60 minutes'").replace("f.a AS a,", "q.a AS a,");
    assert_eq!(
        parse(&text).unwrap_err().to_string(),
        "line 2: bucket width '60 minutes': write '1 hour'\n\
         line 3: unknown table alias q"
    );
    let text = base("count(*) AS n", "count(*) AS n, count(*) AS n");
    assert_eq!(parse(&text).unwrap_err().to_string(), "t: n declared twice");
    let text = base(
        "count(*) AS n",
        "count(*) AS n, count(*) AS n, count(*) AS m, count(*) AS m",
    );
    assert_eq!(
        parse(&text).unwrap_err().to_string(),
        "t: n declared twice\nt: m declared twice"
    );
}
