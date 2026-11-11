//! One spelling per meaning, nothing inferred (ADR-0029). Each case here
//! is one other spelling PostgreSQL would take, and the error must name
//! the construct and say what to write instead.

use straum_domain::Literal;
use straum_notation::{Error, parse, parse_ask};

/// The canon: every schema, alias, column owner, output name and join
/// kind written; GROUP BY by position; FILTER on columns.
const CANON: &str = "CREATE VIEW orders WITH (buckets = '1 hour, 1 day') AS
SELECT date_bin('1 hour', orders.placed_at, timestamptz 'epoch') AS bucket,
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
    assert!(CANON.contains(from), "{from}");
    CANON.replacen(from, to, 1)
}

fn errors(src: &str) -> Vec<String> {
    match parse(src) {
        Err(Error::Sql(errors)) => errors.iter().map(ToString::to_string).collect(),
        Err(Error::Spec(errors)) => errors.iter().map(ToString::to_string).collect(),
        Ok(_) => panic!("parsed:\n{src}"),
    }
}

fn check(cases: &[(&str, &str, &str)]) {
    for (from, to, fragment) in cases {
        let errors = errors(&canon(from, to));
        assert!(
            errors.iter().any(|e| e.contains(fragment)),
            "{to}: {errors:?} has no {fragment:?}"
        );
    }
}

#[test]
fn the_canon_parses() {
    let spec = parse(CANON).unwrap();
    let orders = spec.fact("orders").unwrap();
    let keys: Vec<String> = orders.keys().map(ToString::to_string).collect();
    assert_eq!(keys, ["status", "customer_id", "shipped", "country"]);
    assert_eq!(orders.refs[0].name.as_str(), "customers");
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
            "join, write LEFT OUTER JOIN,",
        ),
        (
            format!("INNER JOIN shop.customers AS customers {on}"),
            "join, write LEFT OUTER JOIN,",
        ),
        (
            format!("LEFT JOIN shop.customers AS customers {on}"),
            "LEFT JOIN, write LEFT OUTER JOIN,",
        ),
        (
            format!("RIGHT JOIN shop.customers AS customers {on}"),
            "join, write LEFT OUTER JOIN,",
        ),
        (
            format!("RIGHT OUTER JOIN shop.customers AS customers {on}"),
            "join, write LEFT OUTER JOIN,",
        ),
        (
            format!("FULL OUTER JOIN shop.customers AS customers {on}"),
            "join, write LEFT OUTER JOIN,",
        ),
        (
            "CROSS JOIN shop.customers AS customers".to_owned(),
            "join, write LEFT OUTER JOIN,",
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
        let errors = errors(&canon(join, &to));
        assert!(
            errors.iter().any(|e| e.contains(fragment)),
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
            "date_bin('1 hour', orders.placed_at, timestamptz 'epoch')",
            "date_bin('1 hour', placed_at, timestamptz 'epoch')",
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

/// Every SELECT item is named, the bucket `bucket`, and nothing twice.
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
        (
            "date_bin('1 hour', orders.placed_at, timestamptz 'epoch') AS bucket",
            "date_bin('1 hour', orders.placed_at, timestamptz 'epoch') AS hour",
            "the date_bin bucket is named AS bucket, not AS hour",
        ),
        (
            "date_bin('1 hour', orders.placed_at, timestamptz 'epoch') AS bucket",
            "date_bin('1 hour', orders.placed_at, timestamptz 'epoch')",
            "name every SELECT item",
        ),
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
            "GROUP BY date_bin('1 hour', orders.placed_at, timestamptz 'epoch'), 2, 3, 4, 5",
            "GROUP BY item, write the key's position in SELECT, `date_bin(",
        ),
        (
            keys,
            "GROUP BY 1, orders.status, 3, 4, 5",
            "GROUP BY item, write the key's position in SELECT, `orders.status`",
        ),
        (
            keys,
            "GROUP BY 2, 1, 3, 4, 5",
            "must list the keys' positions in SELECT: GROUP BY 1, 2, 3, 4, 5",
        ),
        (
            keys,
            "GROUP BY 1, 2, 3, 4",
            "must list the keys' positions in SELECT",
        ),
        (
            keys,
            "GROUP BY 1, 2, 3, 4, 5, 6",
            "must list the keys' positions in SELECT",
        ),
        (keys, "", "must list the keys' positions in SELECT"),
    ]);
}

/// Measures and conditions have one form each.
#[test]
fn one_measure_and_condition_spelling() {
    check(&[
        (
            "count(*) AS orders",
            "count(1) AS orders",
            "count, use count(*) or count(DISTINCT column),",
        ),
        (
            "count(*) AS orders",
            "count(orders.id) AS orders",
            "count, use count(*) or count(DISTINCT column),",
        ),
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

/// Asks name keys and measures as the view named them, and write both
/// bounds.
#[test]
fn asks_follow_the_same_rule() {
    let range = [Literal::Int(1_704_067_200), Literal::Int(1_706_745_600)];
    let ok = parse_ask(
        "SELECT bucket, country, orders FROM orders WHERE bucket >= $1 AND bucket < $2 \
         GROUP BY bucket, country",
        &range,
    );
    assert!(ok.is_ok(), "{ok:?}");
    for (query, fragment) in [
        (
            "SELECT customers.country, orders FROM orders WHERE bucket >= $1 AND bucket < $2 \
             GROUP BY customers.country",
            "expression, write the name the view gives the key or measure, `customers.country`",
        ),
        (
            "SELECT country, orders FROM orders WHERE bucket >= $1 AND bucket < $2 GROUP BY 1",
            "expression, write the name the view gives the key or measure, `1`",
        ),
        (
            "SELECT orders FROM orders WHERE bucket >= $1",
            "an ask writes both bounds; bucket < $n is missing",
        ),
        (
            "SELECT orders FROM orders WHERE bucket < $2",
            "an ask writes both bounds; bucket >= $n is missing",
        ),
        (
            "SELECT orders FROM shop.orders WHERE bucket >= $1 AND bucket < $2",
            "view shop.orders must be a bare name",
        ),
        (
            "SELECT orders AS n FROM orders WHERE bucket >= $1 AND bucket < $2",
            "SELECT item `orders AS n` is not in straum SQL",
        ),
    ] {
        let errors: Vec<String> = (parse_ask(query, &range).unwrap_err().iter())
            .map(ToString::to_string)
            .collect();
        assert!(
            errors.iter().any(|e| e.contains(fragment)),
            "{query}: {errors:?} has no {fragment:?}"
        );
    }
}
