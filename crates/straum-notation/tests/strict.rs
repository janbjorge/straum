//! One spelling per meaning, nothing inferred (ADR-0029). Each case here
//! is one other spelling PostgreSQL would take, and the error must name
//! the construct and say what to write instead.

use straum_domain::{Literal, RefKind};
use straum_notation::{Error, parse};

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
        let errors = errors(&canon(join, &to));
        assert!(
            errors.iter().any(|e| e.contains(fragment)),
            "{to}: {errors:?} has no {fragment:?}"
        );
    }
}

/// `INNER JOIN` is the other spelling that stays, and the view's `WHERE`
/// with `EXISTS` and `NOT EXISTS` are the semi and anti joins.
#[test]
fn inner_join_and_exists_parse() {
    let src = canon(
        "LEFT OUTER JOIN shop.customers AS customers ON customers.id = orders.customer_id",
        "INNER JOIN shop.customers AS customers ON customers.id = orders.customer_id\n\
         WHERE EXISTS (SELECT 1 FROM shop.promos AS promos WHERE promos.sku = orders.sku)\n\
         AND NOT EXISTS (SELECT 1 FROM shop.blocked AS blocked WHERE blocked.id = orders.customer_id)",
    );
    let spec = parse(&src).unwrap();
    let kinds: Vec<_> = (spec.fact("orders").unwrap().refs.iter())
        .map(|r| (r.alias.to_string(), r.kind))
        .collect();
    assert_eq!(
        kinds,
        [
            ("customers".into(), RefKind::Inner),
            ("promos".into(), RefKind::Semi),
            ("blocked".into(), RefKind::Anti),
        ]
    );
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
        let errors = errors(&canon(
            "GROUP BY 1, 2, 3, 4, 5",
            &format!("{to} GROUP BY 1, 2, 3, 4, 5"),
        ));
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

/// Measures and conditions have one form each.
#[test]
fn one_measure_and_condition_spelling() {
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
            "count(orders.id + 1) AS orders",
            "column, write alias.column, `orders.id + 1`",
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

/// An enum column is read as its label by writing `alias.column::text`
/// (ADR-0037): on a key, its null-ness, or a FILTER column, spelled alike
/// everywhere the view reads the column, and nowhere else.
#[test]
fn enum_columns_are_cast_to_text() {
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
    let cast = cast.replacen(
        "WHERE orders.shipped_at IS NOT NULL AND customers.country IN",
        "WHERE orders.shipped_at::text IS NOT NULL AND customers.country::text IN",
        1,
    );
    let spec = parse(&cast).unwrap();
    let orders = spec.fact("orders").unwrap();
    let as_text: Vec<&str> = orders.as_text.iter().map(|c| c.as_str()).collect();
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

    // A second view reads the dim column without the cast.
    let two = format!(
        "{cast};\n{}",
        CANON.replace("VIEW orders", "VIEW orders_again")
    );
    assert!(errors(&two).iter().any(|e| e.contains(
        "shop.customers.country is read without ::text here and with it in another view; \
         write it alike"
    )));

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
            "date_bin('1 hour', orders.placed_at,",
            "date_bin('1 hour', orders.placed_at::text,",
            "the date_bin column orders.placed_at::text can't be ::text",
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
