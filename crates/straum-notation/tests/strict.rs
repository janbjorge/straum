//! One spelling per meaning, nothing inferred (ADR-0004). Each case here
//! is one other spelling PostgreSQL would take, and the error must name
//! the construct and say what to write instead.

mod common;

use common::{any_contains, edit, error_lines, rejects};
use straum_domain::{Literal, RefKind};
use straum_notation::parse;

/// The canon: every schema, alias, column owner, output name and join
/// kind written; GROUP BY by position; FILTER on columns.
const CANON: &str = "CREATE VIEW orders AS
SELECT buckets(orders.placed_at, '1 hour', '1 day') AS bucket,
       orders.status AS status,
       orders.customer_id AS customer_id,
       (orders.shipped_at IS NOT NULL) AS shipped,
       customers.country AS country,
       count(*) AS orders,
       sum(orders.amount) AS revenue,
       count(*) FILTER (WHERE orders.status = 'paid') AS paid,
       count(*) FILTER (WHERE orders.shipped_at IS NOT NULL AND customers.country IN ('NO', 'SE')) AS shipped_near
FROM shop.orders AS orders
LEFT OUTER JOIN shop.customers AS customers ON customers.id = orders.customer_id
GROUP BY 1, 2, 3, 4, 5";

fn canon(from: &str, to: &str) -> String {
    edit(CANON, from, to)
}

fn check(cases: &[(&str, &str, &str)]) {
    for (from, to, fragment) in cases {
        let errors = error_lines(&canon(from, to));
        assert!(
            any_contains(&errors, fragment),
            "{to}: {errors:?} has no {fragment:?}"
        );
    }
}

#[test]
fn the_canon_parses() {
    let spec = parse(CANON).unwrap();
    let orders = spec.fact("orders").unwrap();
    let keys: Vec<String> = orders.keys().into_iter().map(ToString::to_string).collect();
    assert_eq!(keys, ["status", "customer_id", "shipped", "country"]);
    assert_eq!(orders.refs[0].alias.as_str(), "customers");
    assert_eq!(spec.dim("customers").unwrap().table.schema.as_str(), "shop");
}

/// Every other spelling of a join, and the one the error names.
#[test]
fn one_join_spelling() {
    let join = "LEFT OUTER JOIN shop.customers AS customers ON customers.id = orders.customer_id";
    let on = "ON customers.id = orders.customer_id";
    let cases = [
        (
            format!("JOIN shop.customers AS customers {on}"),
            "JOIN, write INNER JOIN,",
        ),
        (
            format!("LEFT JOIN shop.customers AS customers {on}"),
            "LEFT JOIN, write LEFT OUTER JOIN,",
        ),
        (
            format!("RIGHT JOIN shop.customers AS customers {on}"),
            "join, write INNER JOIN or LEFT OUTER JOIN,",
        ),
        (
            format!("RIGHT OUTER JOIN shop.customers AS customers {on}"),
            "join, write INNER JOIN or LEFT OUTER JOIN,",
        ),
        (
            format!("FULL JOIN shop.customers AS customers {on}"),
            "join, write INNER JOIN or LEFT OUTER JOIN,",
        ),
        (
            format!("FULL OUTER JOIN shop.customers AS customers {on}"),
            "join, write INNER JOIN or LEFT OUTER JOIN,",
        ),
        (
            "CROSS JOIN shop.customers AS customers".to_owned(),
            "join, write INNER JOIN or LEFT OUTER JOIN,",
        ),
        (
            "NATURAL LEFT OUTER JOIN shop.customers AS customers".to_owned(),
            "join condition, write ON,",
        ),
        (
            "LEFT OUTER JOIN shop.customers AS customers USING (customer_id)".to_owned(),
            "join condition, write ON,",
        ),
        (
            format!("LEFT OUTER JOIN shop.customers AS customers {on} AND customers.active = true"),
            "join ON customers.<key> = orders.<column>, the joined table's column on the left",
        ),
        (
            "LEFT OUTER JOIN shop.customers AS customers ON orders.customer_id = customers.id"
                .to_owned(),
            "join ON customers.<key> = orders.<column>, the joined table's column on the left",
        ),
        (
            format!("LEFT OUTER JOIN shop.customers customers {on}"),
            "table shop.customers must be shop.customers AS <alias>",
        ),
        (
            format!("LEFT OUTER JOIN customers AS customers {on}"),
            "table customers must be schema.table",
        ),
    ];
    for (to, fragment) in cases {
        let errors = error_lines(&canon(join, &to));
        assert!(
            any_contains(&errors, fragment),
            "{to}: {errors:?} has no {fragment:?}"
        );
    }
}

/// `INNER JOIN` is the other spelling that stays, and the view's `WHERE`
/// with `EXISTS` and `NOT EXISTS` are the semi and anti joins.
#[test]
fn inner_join_and_exists_parse() {
    let text = canon(
        "LEFT OUTER JOIN shop.customers AS customers ON customers.id = orders.customer_id",
        "INNER JOIN shop.customers AS customers ON customers.id = orders.customer_id\n\
         WHERE EXISTS (SELECT 1 FROM shop.promos AS promos WHERE promos.sku = orders.sku)\n\
         AND NOT EXISTS (SELECT 1 FROM shop.blocked AS blocked WHERE blocked.id = orders.customer_id)",
    );
    let spec = parse(&text).unwrap();
    let mut aliases = Vec::new();
    let mut kinds = Vec::new();
    for reference in &spec.fact("orders").unwrap().refs {
        aliases.push(reference.alias.to_string());
        kinds.push(reference.kind);
    }
    assert_eq!(aliases, ["customers", "promos", "blocked"]);
    assert_eq!(kinds, [RefKind::Inner, RefKind::Semi, RefKind::Anti]);
    for (to, fragment) in [
        (
            "WHERE orders.customer_id IN (SELECT id FROM shop.blocked AS blocked)",
            "filter condition, write = or IN joined by AND,",
        ),
        (
            "WHERE NOT orders.customer_id IN (SELECT id FROM shop.blocked AS blocked)",
            "filter condition, write = or IN joined by AND,",
        ),
        (
            "WHERE EXISTS (SELECT 1 FROM shop.promos AS promos WHERE promos.sku = orders.sku AND promos.live = true)",
            "join ON promos.<key> = orders.<column>",
        ),
    ] {
        let errors = error_lines(&canon(
            "GROUP BY 1, 2, 3, 4, 5",
            &format!("{to} GROUP BY 1, 2, 3, 4, 5"),
        ));
        assert!(
            any_contains(&errors, fragment),
            "{to}: {errors:?} has no {fragment:?}"
        );
    }
}

/// The fact table is written whole too.
#[test]
fn the_fact_table_is_schema_table_as_alias() {
    check(&[
        (
            "FROM shop.orders AS orders",
            "FROM shop.orders orders",
            "table shop.orders must be shop.orders AS <alias>",
        ),
        (
            "FROM shop.orders AS orders",
            "FROM shop.orders",
            "table shop.orders must be shop.orders AS <alias>",
        ),
        (
            "FROM shop.orders AS orders",
            "FROM orders AS orders",
            "table orders must be schema.table",
        ),
        (
            "FROM shop.orders AS orders",
            "FROM a.shop.orders AS orders",
            "table a.shop.orders must be schema.table",
        ),
    ]);
}

/// Every column names its table, the fact's included, in SELECT, ON and
/// FILTER alike.
#[test]
fn every_column_is_alias_column() {
    check(&[
        (
            "orders.status AS status",
            "status AS status",
            "column, write alias.column, `status`",
        ),
        (
            "buckets(orders.placed_at,",
            "buckets(placed_at,",
            "column, write alias.column, `placed_at`",
        ),
        (
            "sum(orders.amount)",
            "sum(amount)",
            "column, write alias.column, `amount`",
        ),
        (
            "(orders.shipped_at IS NOT NULL)",
            "(shipped_at IS NOT NULL)",
            "column, write alias.column, `shipped_at`",
        ),
        (
            "ON customers.id = orders.customer_id",
            "ON customers.id = customer_id",
            "column, write alias.column, `customer_id`",
        ),
        (
            "FILTER (WHERE orders.status = 'paid')",
            "FILTER (WHERE status = 'paid')",
            "column, write alias.column, `status`",
        ),
        (
            "customers.country IN ('NO', 'SE')",
            "country IN ('NO', 'SE')",
            "column, write alias.column, `country`",
        ),
        (
            "orders.shipped_at IS NOT NULL AND customers.country",
            "shipped = true AND customers.country",
            "column, write alias.column, `shipped`",
        ),
    ]);
}

/// Every SELECT item is named, the time too, and nothing twice.
#[test]
fn every_select_item_is_named() {
    check(&[
        (
            "orders.status AS status",
            "orders.status",
            "name every SELECT item: orders.status AS <name>",
        ),
        (
            "customers.country AS country",
            "customers.country",
            "name every SELECT item: customers.country AS <name>",
        ),
        (
            "count(*) AS orders",
            "count(*)",
            "name every SELECT item: count(*) AS <name>",
        ),
        ("'1 day') AS bucket", "'1 day')", "name every SELECT item"),
        ("AS bucket", "AS status", "orders: status declared twice"),
        (
            "orders.customer_id AS customer_id",
            "orders.customer_id AS status",
            "orders: status declared twice",
        ),
        (
            "customers.country AS country",
            "customers.country AS status",
            "orders: status declared twice",
        ),
        (
            "count(*) AS orders",
            "count(*) AS status",
            "orders: status declared twice",
        ),
    ]);
}

/// Each width is its own argument: a list in one string is refused.
#[test]
fn each_bucket_width_is_its_own_argument() {
    let listed = canon("'1 hour', '1 day'", "'1 hour, 1 day'");
    assert_eq!(
        error_lines(&listed),
        ["line 2: bucket width '1 hour, 1 day': use '<n> seconds|minutes|hours|days'"],
    );
    let spec = parse(CANON).unwrap();
    assert_eq!(spec.fact("orders").unwrap().time.wider.len(), 1);
}

/// A GROUP BY position has no leading zero: the error names the one to
/// write, and the position without it parses.
#[test]
fn a_group_by_position_has_no_leading_zero() {
    for (group_by, error) in [
        ("GROUP BY 01, 2, 3, 4, 5", "line 13: GROUP BY 01: write 1"),
        ("GROUP BY 1, 2, 3, 004, 5", "line 13: GROUP BY 004: write 4"),
    ] {
        let zeros = canon("GROUP BY 1, 2, 3, 4, 5", group_by);
        assert_eq!(error_lines(&zeros), [error], "{group_by}");
    }
    // Zeros only are no position at all.
    let zero = canon("GROUP BY 1, 2, 3, 4, 5", "GROUP BY 00, 2, 3, 4, 5");
    assert!(any_contains(
        &error_lines(&zero),
        "GROUP BY item, write the key's position in SELECT, `00`"
    ));
    assert!(parse(&canon("GROUP BY 1, 2, 3, 4, 5", "GROUP BY 1, 2, 3, 4, 5")).is_ok());
}

/// A fraction is written its shortest way: one leading zero, no
/// trailing one. The error names the one to write, which parses.
#[test]
fn a_fraction_has_one_spelling() {
    for (written, canonical) in [
        ("1.0", "1"),
        ("0.50", "0.5"),
        ("00.5", "0.5"),
        ("0.0", "0"),
        ("01", "1"),
        ("00", "0"),
    ] {
        let measure = format!("percentile_disc({written}) WITHIN GROUP (ORDER BY orders.amount)");
        let spec = canon("sum(orders.amount)", &measure);
        assert_eq!(
            error_lines(&spec),
            [format!("line 8: fraction {written}: write {canonical}")],
            "{written}"
        );
        let measure = format!("percentile_disc({canonical}) WITHIN GROUP (ORDER BY orders.amount)");
        assert!(parse(&canon("sum(orders.amount)", &measure)).is_ok());
    }
}

/// An integer literal has no leading zero, and zero no sign. The error
/// names the one to write, which parses.
#[test]
fn an_integer_literal_has_one_spelling() {
    for (written, canonical) in [("007", "7"), ("-007", "-7"), ("-0", "0"), ("00", "0")] {
        let filter = format!("FILTER (WHERE orders.customer_id = {written})");
        let spec = canon("FILTER (WHERE orders.status = 'paid')", &filter);
        assert_eq!(
            error_lines(&spec),
            [format!("line 9: literal {written}: write {canonical}")],
            "{written}"
        );
        let filter = format!("FILTER (WHERE orders.customer_id = {canonical})");
        assert!(parse(&canon("FILTER (WHERE orders.status = 'paid')", &filter)).is_ok());
    }
    let filter = "FILTER (WHERE orders.customer_id IN (1, 02))";
    let spec = canon("FILTER (WHERE orders.status = 'paid')", filter);
    assert_eq!(error_lines(&spec), ["line 9: literal 02: write 2"]);
}

/// GROUP BY lists the keys' positions in SELECT, in order, nothing else.
#[test]
fn group_by_lists_the_key_positions() {
    let keys = "GROUP BY 1, 2, 3, 4, 5";
    check(&[
        (
            keys,
            "GROUP BY bucket, status, customer_id, shipped, country",
            "GROUP BY item, write the key's position in SELECT, `bucket`",
        ),
        (
            keys,
            "GROUP BY buckets(orders.placed_at, '1 hour', '1 day'), 2, 3, 4, 5",
            "GROUP BY item, write the key's position in SELECT, `buckets(",
        ),
        (
            keys,
            "GROUP BY 1, orders.status, 3, 4, 5",
            "GROUP BY item, write the key's position in SELECT, `orders.status`",
        ),
        (
            keys,
            "GROUP BY 2, 1, 3, 4, 5",
            "GROUP BY 2, 1, 3, 4, 5 must list the keys' positions in SELECT: GROUP BY 1, 2, 3, 4, 5",
        ),
        (
            keys,
            "GROUP BY 1, 2, 3, 4",
            "GROUP BY 1, 2, 3, 4 must list the keys' positions in SELECT: GROUP BY 1, 2, 3, 4, 5",
        ),
        (
            keys,
            "GROUP BY 1, 2, 3, 4, 5, 6",
            "GROUP BY 1, 2, 3, 4, 5, 6 must list the keys' positions in SELECT",
        ),
        (keys, "", "must list the keys' positions in SELECT"),
    ]);
}

/// A measure has one form.
#[test]
fn one_measure_spelling() {
    check(&[
        (
            "count(*) AS orders",
            "count(1) AS orders",
            "count of a constant, use count(*), `count(1)`",
        ),
        (
            "count(*) AS orders",
            "count('x') AS orders",
            "count of a constant, use count(*), `count('x')`",
        ),
        (
            "count(*) AS orders",
            "every(orders.id) AS orders",
            "every, write bool_and, `every(orders.id)`",
        ),
        (
            "count(*) AS orders",
            "variance(orders.id) AS orders",
            "variance, write var_samp, `variance(orders.id)`",
        ),
        (
            "count(*) AS orders",
            "stddev(orders.id) AS orders",
            "stddev, write stddev_samp, `stddev(orders.id)`",
        ),
        (
            "count(*) AS orders",
            "mode(orders.id) AS orders",
            "arguments, write mode() WITHIN GROUP (ORDER BY alias.column), `mode(orders.id)`",
        ),
        (
            "count(*) AS orders",
            "mode() AS orders",
            "mode() without WITHIN GROUP (ORDER BY alias.column) `mode()`",
        ),
        (
            "count(*) AS orders",
            "mode() WITHIN GROUP (ORDER BY orders.id DESC) AS orders",
            "ASC, DESC or USING in WITHIN GROUP",
        ),
        (
            "count(*) AS orders",
            "var_samp(DISTINCT orders.id) AS orders",
            "DISTINCT outside count, sum and avg `var_samp(DISTINCT orders.id)`",
        ),
        (
            "count(*) AS orders",
            "bool_or(DISTINCT orders.id) AS orders",
            "DISTINCT outside count, sum and avg `bool_or(DISTINCT orders.id)`",
        ),
        (
            "count(*) AS orders",
            "count(orders.id + 1) AS orders",
            "column, write alias.column, `orders.id + 1`",
        ),
    ]);
}

/// A condition has one form.
#[test]
fn one_condition_spelling() {
    check(&[
        (
            "customers.country IN ('NO', 'SE')",
            "customers.country IN ('NO')",
            "IN with one value, write =, `customers.country IN ('NO')`",
        ),
        (
            "customers.country IN ('NO', 'SE')",
            "customers.country = 'NO' OR customers.country = 'SE'",
            "filter condition, write = or IN joined by AND,",
        ),
        (
            "customers.country IN ('NO', 'SE')",
            "customers.country <> 'NO'",
            "filter condition, write = or IN joined by AND,",
        ),
        (
            "customers.country IN ('NO', 'SE')",
            "customers.country NOT IN ('NO', 'SE')",
            "filter condition, write = or IN joined by AND,",
        ),
        (
            "customers.country IN ('NO', 'SE')",
            "NOT customers.country = 'NO'",
            "filter condition, write = or IN joined by AND,",
        ),
        (
            "customers.country IN ('NO', 'SE')",
            "customers.country BETWEEN 'NO' AND 'SE'",
            "filter condition, write = or IN joined by AND,",
        ),
        // A FILTER reaches only what the view selects as a key.
        (
            "FILTER (WHERE orders.status = 'paid')",
            "FILTER (WHERE orders.amount = 1)",
            "FILTER on orders.amount needs orders.amount AS <name> in SELECT",
        ),
        (
            "FILTER (WHERE orders.status = 'paid')",
            "FILTER (WHERE orders.placed_at IS NULL)",
            "FILTER (WHERE orders.placed_at IS NULL) needs the key (orders.placed_at IS NULL) \
             AS <name> in SELECT",
        ),
    ]);
}

/// A parenthesis is written where the canon has one, around a null-ness
/// key, and nowhere else: AND needs none, and neither does a column.
#[test]
fn parentheses_are_written_where_the_canon_has_them() {
    check(&[
        (
            "(orders.shipped_at IS NOT NULL) AS shipped",
            "orders.shipped_at IS NOT NULL AS shipped",
            "null-ness key without parentheses, write (orders.shipped_at IS NOT NULL),",
        ),
        (
            "(orders.shipped_at IS NOT NULL) AS shipped",
            "orders.shipped_at IS NULL AS shipped",
            "null-ness key without parentheses, write (orders.shipped_at IS NULL),",
        ),
        (
            "(orders.shipped_at IS NOT NULL) AS shipped",
            "((orders.shipped_at IS NOT NULL)) AS shipped",
            "parentheses, write alias.column or (alias.column IS NULL), \
             `((orders.shipped_at IS NOT NULL))`",
        ),
        (
            "orders.status AS status",
            "(orders.status) AS status",
            "parentheses, write alias.column or (alias.column IS NULL), `(orders.status)`",
        ),
        (
            "FILTER (WHERE orders.status = 'paid')",
            "FILTER (WHERE (orders.status = 'paid'))",
            "parentheses, write the condition without them, `(orders.status = 'paid')`",
        ),
        (
            "WHERE orders.shipped_at IS NOT NULL AND",
            "WHERE (orders.shipped_at IS NOT NULL) AND",
            "parentheses, write the condition without them, `(orders.shipped_at IS NOT NULL)`",
        ),
        (
            "WHERE orders.shipped_at IS NOT NULL AND customers.country IN ('NO', 'SE')",
            "WHERE (orders.shipped_at IS NOT NULL AND customers.country IN ('NO', 'SE'))",
            "parentheses, write the condition without them, `(orders.shipped_at IS NOT NULL AND",
        ),
        (
            "WHERE orders.shipped_at IS NOT NULL AND customers.country IN ('NO', 'SE')",
            "WHERE orders.shipped_at IS NOT NULL AND (customers.country IN ('NO', 'SE'))",
            "parentheses, write the condition without them, `(customers.country IN ('NO', 'SE'))`",
        ),
        (
            "ON customers.id = orders.customer_id",
            "ON (customers.id = orders.customer_id)",
            "the joined table's column on the left, one or more joined by AND; found \
             (customers.id = orders.customer_id)",
        ),
        (
            "GROUP BY",
            "WHERE (orders.status = 'paid') GROUP BY",
            "parentheses, write the condition without them, `(orders.status = 'paid')`",
        ),
        (
            "GROUP BY",
            "WHERE (EXISTS (SELECT 1 FROM shop.promos AS promos WHERE promos.id = orders.id)) \
             GROUP BY",
            "parentheses, write the condition without them, `(EXISTS (SELECT 1 FROM",
        ),
        (
            "GROUP BY",
            "WHERE EXISTS (SELECT 1 FROM shop.promos AS promos WHERE (promos.id = orders.id)) \
             GROUP BY",
            "one or more joined by AND; found (promos.id = orders.id)",
        ),
    ]);
}

fn cast_canon() -> String {
    let cast = canon("orders.status AS status", "orders.status::text AS status");
    let cast = cast.replacen(
        "FILTER (WHERE orders.status = 'paid')",
        "FILTER (WHERE orders.status::text = 'paid')",
        1,
    );
    let cast = cast.replacen(
        "customers.country AS country",
        "customers.country::text AS country",
        1,
    );
    let cast = cast.replacen(
        "(orders.shipped_at IS NOT NULL) AS shipped",
        "(orders.shipped_at::text IS NOT NULL) AS shipped",
        1,
    );
    cast.replacen(
        "WHERE orders.shipped_at IS NOT NULL AND customers.country IN",
        "WHERE orders.shipped_at::text IS NOT NULL AND customers.country::text IN",
        1,
    )
}

/// An enum column is read as its label by writing `alias.column::text`
/// (ADR-0004): on a key, its null-ness, or a FILTER column, spelled alike
/// everywhere the view reads the column, and nowhere else.
#[test]
fn enum_columns_are_cast_to_text() {
    let spec = parse(&cast_canon()).unwrap();
    let orders = spec.fact("orders").unwrap();
    let as_text: Vec<&str> = orders
        .as_text
        .iter()
        .map(straum_domain::Identifier::as_str)
        .collect();
    assert_eq!(as_text, ["status", "shipped_at"]);
    let customers = spec.dim("customers").unwrap();
    assert_eq!(customers.as_text, [customers.attributes[0].clone()]);
    assert_eq!(
        orders.measures[2].filter[0],
        straum_domain::Condition {
            key: orders.by_keys[0].name().clone(),
            values: vec![Literal::Text("paid".into())],
        }
    );
}

#[test]
fn a_dim_column_is_cast_alike_in_every_view() {
    // A second view reads the dim column without the cast.
    let two = format!(
        "{};\n{}",
        cast_canon(),
        CANON.replace("VIEW orders", "VIEW orders_again")
    );
    let message = "shop.customers.country is read without ::text here and with it in another \
                   view; write it alike";
    assert!(any_contains(&error_lines(&two), message));
}

#[test]
fn a_dim_column_cast_in_a_later_view_is_named() {
    // The first view reads it plain, the second as text.
    let two = format!(
        "{};\n{}",
        CANON,
        cast_canon().replace("VIEW orders", "VIEW orders_again")
    );
    let message = "shop.customers.country is read ::text here and without it in another \
                   view; write it alike";
    assert!(any_contains(&error_lines(&two), message));
}

/// A bucket-list width has one spelling. The error names the one to write.
#[test]
fn a_bucket_width_has_one_spelling() {
    rejects(&[
        (
            "'1 hour') AS",
            "'60 minutes') AS",
            2,
            "bucket width '60 minutes': write '1 hour'",
        ),
        (
            "'1 hour') AS",
            "'+1 hour') AS",
            2,
            "bucket width '+1 hour': use '<n>",
        ),
        (
            "'1 hour') AS",
            "'1 hours') AS",
            2,
            "bucket width '1 hours': write '1 hour'",
        ),
    ]);
}

#[test]
fn a_cast_is_text_and_only_on_a_label() {
    check(&[
        (
            "orders.status AS status",
            "CAST(orders.status AS text) AS status",
            "CAST, write alias.column::text,",
        ),
        (
            "orders.status AS status",
            "orders.status::varchar AS status",
            ", write ::text,",
        ),
        (
            "ON customers.id = orders.customer_id",
            "ON customers.id::text = orders.customer_id",
            "::text outside a key or a FILTER column,",
        ),
        (
            "sum(orders.amount)",
            "sum(orders.amount::text)",
            "an aggregated column orders.amount::text can't be ::text; it goes on a key or a \
             FILTER column",
        ),
        (
            "buckets(orders.placed_at,",
            "buckets(orders.placed_at::text,",
            "the buckets column orders.placed_at::text can't be ::text",
        ),
        (
            "orders.status AS status",
            "orders.status::text AS status",
            "orders.status: SELECT spells the column orders.status::text; write it alike",
        ),
        (
            "FILTER (WHERE orders.status = 'paid')",
            "FILTER (WHERE orders.status::text = 'paid')",
            "orders.status::text: SELECT spells the column orders.status; write it alike",
        ),
        (
            "WHERE orders.shipped_at IS NOT NULL AND",
            "WHERE orders.shipped_at::text IS NOT NULL AND",
            "orders.shipped_at::text: SELECT spells the column orders.shipped_at; write it alike",
        ),
    ]);
}
