//! Hand-made join cases over two customers, two products and a few dim
//! rows: each test builds a store, asks it, checks the answer by hand and
//! against the reference, then changes a dim and asks again.

use crate::common::{condition, lit, text};
use crate::reference::check;
use crate::view::{
    BLOCKED, Dims, HOUR, KINDS, Kinds, Loaded, MEASURES, PROMOS, Replaced, START, Shape, TAGS, ask,
    load, order, spec, spec_with,
};
use straum_domain::{AnswerRow, Ask, Condition, Datum, Literal, RefKind, Value};

/// Two customers, one in a tier and one not; product 1 with two tags, 2
/// with none; product 1 on promo; customer 2 blocked.
fn small_dims() -> Dims {
    let mut dims = Dims::new();
    for (dim, key, attributes) in [
        ("customers", 1, vec![text("NO"), Datum::Int(10)]),
        ("customers", 2, vec![text("SE"), Datum::Null]),
        ("tiers", 10, vec![text("gold")]),
        ("tags", 1, vec![text("red")]),
        ("tags", 1, vec![text("big")]),
        ("promos", 1, vec![text("x")]),
        ("blocked", 2, vec![text("y")]),
    ] {
        let rows = dims.entry(dim).or_default();
        rows.entry(vec![Datum::Int(key)])
            .or_default()
            .push(attributes);
    }
    dims
}

/// A store over [`small_dims`] and the orders given.
fn small(kinds: Kinds, filter: Vec<Condition>, orders: &[Vec<Datum>]) -> Loaded {
    load(spec(kinds, filter), small_dims(), orders)
}

/// [`small_dims`] with one row a key: product 1 has the one tag.
fn one_row_dims() -> Dims {
    let mut dims = small_dims();
    let tags = dims.get_mut("tags").unwrap();
    tags.get_mut(&vec![Datum::Int(1)]).unwrap().truncate(1);
    dims
}

/// An order at [`START`] of customer, product and quantity, all present.
fn order_of(customer: i64, product: i64, quantity: i64) -> Vec<Datum> {
    order(
        START,
        Datum::Int(customer),
        Datum::Int(product),
        Datum::Int(quantity),
    )
}

/// A row of an answer by one text key, of two integer measures.
fn text_row(key: &str, first: i128, second: i128) -> AnswerRow {
    AnswerRow {
        keys: vec![text(key)],
        values: vec![Value::Int(first), Value::Int(second)],
    }
}

/// A row of an answer by one key, of one integer measure.
fn key_row(key: Datum, value: i128) -> AnswerRow {
    AnswerRow {
        keys: vec![key],
        values: vec![Value::Int(value)],
    }
}

/// A row of an answer without keys: the measures asked.
fn values(loaded: &Loaded, ask: &Ask) -> Vec<Value> {
    loaded.store.ask(ask).unwrap().rows[0].values.clone()
}

/// One key's value in an answer, and its one measure.
#[derive(Debug, PartialEq)]
struct Labelled {
    key: Datum,
    value: Value,
}

fn labelled(key: Datum, value: i128) -> Labelled {
    Labelled {
        key,
        value: Value::Int(value),
    }
}

/// The answer to `ask`, an ask of one measure by one key.
fn labels(loaded: &Loaded, ask: &Ask) -> Vec<Labelled> {
    let mut labels = Vec::new();
    for row in loaded.store.ask(ask).unwrap().rows {
        labels.push(Labelled {
            key: row.keys[0].clone(),
            value: row.values[0].clone(),
        });
    }
    labels
}

/// The kinds of [`KINDS`] with promos and blocked as left joins, so they
/// drop no order.
fn with_left_promos_and_blocked() -> Kinds {
    let mut kinds = KINDS;
    kinds[PROMOS] = RefKind::Left;
    kinds[BLOCKED] = RefKind::Left;
    kinds
}

/// An inner join keeps the orders with a tag, once per tag; a left join
/// keeps every order, those without a tag once under null.
#[test]
fn inner_keeps_matched_rows_and_left_keeps_all() {
    let orders = [order_of(1, 1, 5), order_of(1, 2, 7)];
    let mut kinds = with_left_promos_and_blocked();
    let loaded = small(kinds, vec![], &orders);
    // Product 1 has two tags: its order counts twice; product 2 has none.
    let by_tag = ask(&["n", "q"], &["tag"], vec![]);
    assert_eq!(
        loaded.store.ask(&by_tag).unwrap().rows,
        [text_row("big", 1, 5), text_row("red", 1, 5)]
    );
    assert_eq!(
        values(&loaded, &ask(&["n", "q"], &[], vec![])),
        [Value::Int(2), Value::Int(10)]
    );
    check(&loaded, &orders, &by_tag);

    kinds[TAGS] = RefKind::Left;
    let loaded = small(kinds, vec![], &orders);
    assert_eq!(
        loaded.store.ask(&by_tag).unwrap().rows,
        [
            AnswerRow {
                keys: vec![Datum::Null],
                values: vec![Value::Int(1), Value::Int(7)]
            },
            text_row("big", 1, 5),
            text_row("red", 1, 5),
        ]
    );
    assert_eq!(
        values(&loaded, &ask(&["n", "q"], &[], vec![])),
        [Value::Int(3), Value::Int(17)]
    );
    check(&loaded, &orders, &by_tag);
}

/// Every measure of one order of quantity 5, counted `rows` times. The
/// copies are alike, so they spread by nothing, 5 is their mode and their
/// one distinct value; a sample of one has no variance.
fn every_measure_of_five(rows: i128) -> Vec<Value> {
    let variance = if rows > 1 { Value::Int(0) } else { Value::Null };
    vec![
        Value::Int(rows),
        Value::Int(5 * rows),
        Value::Int(5),
        Value::Int(5),
        Value::Int(5),
        Value::Int(1),
        Value::Int(rows),
        Value::Bool(false),
        Value::Bool(false),
        variance,
        Value::Int(0),
        Value::Int(5),
        Value::Int(5),
        Value::Avg { sum: 5, count: 1 },
    ]
}

/// Several dim rows under one key multiply a count and a sum, and leave a
/// min, a max, a median of one value and a count distinct as they were.
#[test]
fn many_matches_multiply_counts_and_sums_only() {
    let orders = [order_of(1, 1, 5)];
    let kinds = with_left_promos_and_blocked();
    let mut loaded = small(kinds, vec![], &orders);
    let all = ask(&MEASURES, &[], vec![]);
    assert_eq!(values(&loaded, &all), every_measure_of_five(2));
    check(&loaded, &orders, &all);
    // A third tag, then one gone: the multiplicity follows the dim.
    loaded.add_dim("tags", vec![Datum::Int(1)], vec![text("new")]);
    assert_eq!(values(&loaded, &all)[0], Value::Int(3));
    loaded.remove_dim("tags", &[Datum::Int(1)], &[text("red")]);
    loaded.remove_dim("tags", &[Datum::Int(1)], &[text("big")]);
    assert_eq!(values(&loaded, &all), every_measure_of_five(1));
    check(&loaded, &orders, &all);
}

/// A semi join counts an order once however many rows match, and not at
/// all when none does; an anti join is the opposite.
#[test]
fn semi_counts_once_and_anti_counts_the_unmatched() {
    let orders = [order_of(1, 1, 5), order_of(2, 2, 7)];
    // Only semi and anti join, so the counts are theirs alone.
    let mut kinds = KINDS;
    kinds[TAGS] = RefKind::Left;
    let mut loaded = small(kinds, vec![], &orders);
    let count = ask(&["n", "q"], &[], vec![]);
    // Order 1: on promo, customer not blocked. Order 2: no promo.
    assert_eq!(values(&loaded, &count), [Value::Int(2), Value::Int(10)]);
    check(&loaded, &orders, &count);
    // A second promo row for product 1 changes nothing; blocking customer
    // 1 drops the order; unblocking customer 2 still leaves it off promo.
    loaded.add_dim("promos", vec![Datum::Int(1)], vec![text("x2")]);
    assert_eq!(values(&loaded, &count), [Value::Int(2), Value::Int(10)]);
    loaded.add_dim("blocked", vec![Datum::Int(1)], vec![text("y")]);
    assert_eq!(values(&loaded, &count), [Value::Int(0), Value::Null]);
    loaded.remove_dim("blocked", &[Datum::Int(2)], &[text("y")]);
    assert_eq!(values(&loaded, &count), [Value::Int(0), Value::Null]);
    loaded.add_dim("promos", vec![Datum::Int(2)], vec![text("x")]);
    assert_eq!(values(&loaded, &count), [Value::Int(1), Value::Int(7)]);
    check(&loaded, &orders, &count);

    // Swapped: semi on blocked, anti on promos.
    kinds[PROMOS] = RefKind::Anti;
    kinds[BLOCKED] = RefKind::Semi;
    let loaded = small(kinds, vec![], &orders);
    assert_eq!(values(&loaded, &count), [Value::Int(1), Value::Int(7)]);
    check(&loaded, &orders, &count);
}

/// A join through another dim reads the second dim by the first's column:
/// a customer's tier label, null when the customer has no tier or no row.
#[test]
fn a_chain_reads_through_the_first_dim() {
    let orders = [order_of(1, 1, 5), order_of(2, 1, 7), order_of(3, 1, 9)];
    let mut kinds = with_left_promos_and_blocked();
    kinds[TAGS] = RefKind::Left;
    let mut loaded = small(kinds, vec![], &orders);
    let by_label = ask(&["q"], &["label"], vec![]);
    // Customer 1 is gold; 2 has no tier; 3 has no row. Product 1 has two
    // tags, so every order counts twice.
    assert_eq!(
        labels(&loaded, &by_label),
        [labelled(Datum::Null, 32), labelled(text("gold"), 10)]
    );
    check(&loaded, &orders, &by_label);
    // The tier relabelled, then customer 2 moved into it, then the tier
    // row gone: each moves the answer at once.
    loaded.replace_dim(Replaced {
        dim: "tiers",
        key: Datum::Int(10),
        old: vec![text("gold")],
        new: vec![text("vip")],
    });
    assert_eq!(
        labels(&loaded, &by_label),
        [labelled(Datum::Null, 32), labelled(text("vip"), 10)]
    );
    loaded.replace_dim(Replaced {
        dim: "customers",
        key: Datum::Int(2),
        old: vec![text("SE"), Datum::Null],
        new: vec![text("SE"), Datum::Int(10)],
    });
    assert_eq!(
        labels(&loaded, &by_label),
        [labelled(Datum::Null, 18), labelled(text("vip"), 24)]
    );
    check(&loaded, &orders, &by_label);
    loaded.remove_dim("tiers", &[Datum::Int(10)], &[text("vip")]);
    assert_eq!(labels(&loaded, &by_label), [labelled(Datum::Null, 42)]);
    // Filtered on the chained value too.
    loaded.add_dim("tiers", vec![Datum::Int(10)], vec![text("vip")]);
    let vip = ask(&["n"], &[], vec![condition("label", vec![lit("vip")])]);
    assert_eq!(values(&loaded, &vip), [Value::Int(4)]);
    check(&loaded, &orders, &vip);
}

/// The view's `WHERE` drops rows from every measure of every ask, and
/// follows the dim when it names a dim column.
#[test]
fn where_drops_rows_from_every_ask() {
    let orders = [
        order_of(1, 1, 5),
        order_of(2, 2, 7),
        order(START + HOUR, Datum::Int(3), Datum::Int(2), Datum::Int(9)),
    ];
    let mut kinds = with_left_promos_and_blocked();
    kinds[TAGS] = RefKind::Left;
    let filter = vec![
        condition("country", vec![lit("NO"), lit("SE")]),
        condition("product", vec![Literal::Int(2)]),
    ];
    let mut loaded = small(kinds, filter, &orders);
    let all = ask(&["n", "q"], &[], vec![]);
    assert_eq!(values(&loaded, &all), [Value::Int(1), Value::Int(7)]);
    let by_customer = ask(&["n"], &["customer"], vec![]);
    assert_eq!(
        loaded.store.ask(&by_customer).unwrap().rows,
        [key_row(Datum::Int(2), 1)]
    );
    check(&loaded, &orders, &all);
    check(&loaded, &orders, &by_customer);
    // Customer 3 gets a row in NO: its order now passes.
    loaded.add_dim(
        "customers",
        vec![Datum::Int(3)],
        vec![text("NO"), Datum::Null],
    );
    assert_eq!(values(&loaded, &all), [Value::Int(2), Value::Int(16)]);
    check(&loaded, &orders, &all);
    // Without the WHERE every order counts, the one with two tags twice.
    let loaded = small(kinds, vec![], &orders);
    assert_eq!(values(&loaded, &all), [Value::Int(4), Value::Int(26)]);
    check(&loaded, &orders, &all);
}

/// A chain's key or a semi join's key that is null joins nothing, and an
/// inner join on a null key keeps nothing.
#[test]
fn null_keys_join_nothing() {
    let orders = [
        order(START, Datum::Null, Datum::Int(1), Datum::Int(5)),
        order(START, Datum::Int(1), Datum::Null, Datum::Int(7)),
    ];
    let mut kinds = with_left_promos_and_blocked();
    let loaded = small(kinds, vec![], &orders);
    // Inner on tags: the null product keeps nothing; the null customer's
    // order has two tags.
    let all = ask(&["n"], &[], vec![]);
    assert_eq!(values(&loaded, &all), [Value::Int(2)]);
    check(&loaded, &orders, &all);
    let by = ask(&["n"], &["country", "label", "tag"], vec![]);
    check(&loaded, &orders, &by);

    kinds[TAGS] = RefKind::Left;
    kinds[PROMOS] = RefKind::Semi;
    let loaded = small(kinds, vec![], &orders);
    assert_eq!(values(&loaded, &all), [Value::Int(2)]);
    check(&loaded, &orders, &all);
}

/// Joined on its `by` keys alone, a fact keeps no bound cells: they
/// would be as many as its keyed cells. An ask reads the keyed cells
/// through the dims as they are, once each while every key holds one row,
/// passing over the cells an inner, semi or anti join drops.
#[test]
fn joins_on_by_keys_keep_no_bound_cells() {
    let orders = [order_of(1, 1, 5), order_of(1, 2, 7), order_of(2, 1, 9)];
    let spec = spec_with(KINDS, vec![], Shape::ByKeys);
    let mut loaded = load(spec, one_row_dims(), &orders);
    assert_eq!(loaded.store.cells("orders"), Some(3));
    assert_eq!(loaded.store.bound_cells("orders"), None);
    // Product 2 has no tag and is on no promo; customer 2 is blocked.
    let by_tag = ask(&["n", "q"], &["tag"], vec![]);
    assert_eq!(
        loaded.store.ask(&by_tag).unwrap().rows,
        [text_row("red", 1, 5)]
    );
    check(&loaded, &orders, &by_tag);
    // The next ask reads the dims as they are then.
    for (dim, attribute) in [("tags", "big"), ("promos", "x")] {
        loaded.add_dim(dim, vec![Datum::Int(2)], vec![text(attribute)]);
    }
    assert_eq!(
        loaded.store.ask(&by_tag).unwrap().rows,
        [text_row("big", 1, 7), text_row("red", 1, 5)]
    );
    loaded.remove_dim("blocked", &[Datum::Int(2)], &[text("y")]);
    assert_eq!(
        loaded.store.ask(&by_tag).unwrap().rows,
        [text_row("big", 1, 7), text_row("red", 2, 14)]
    );
    check(&loaded, &orders, &by_tag);
    assert_eq!(loaded.store.bound_cells("orders"), None);
}

/// A max and a min in a view joined on its `by` keys, which answers from
/// its keyed cells: the largest order leaving reveals the next, and a
/// customer who moves takes their orders' ends to the new country.
#[test]
fn a_max_over_joined_keys_follows_deletes_and_dims() {
    let mut orders = vec![
        order_of(1, 1, 5),
        order_of(1, 1, 9),
        order_of(1, 2, 7),
        order_of(2, 1, 3),
    ];
    let kinds = [RefKind::Left; 5];
    let spec = spec_with(kinds, vec![], Shape::ByKeys);
    let mut loaded = load(spec, one_row_dims(), &orders);
    assert_eq!(loaded.store.bound_cells("orders"), None);
    let ends = ask(&["q_max", "q_min"], &["country"], vec![]);
    assert_eq!(
        loaded.store.ask(&ends).unwrap().rows,
        [text_row("NO", 9, 5), text_row("SE", 3, 3)]
    );
    check(&loaded, &orders, &ends);

    let largest = orders.remove(1);
    loaded.store.remove_row("orders", &largest).unwrap();
    assert_eq!(
        loaded.store.ask(&ends).unwrap().rows,
        [text_row("NO", 7, 5), text_row("SE", 3, 3)]
    );
    check(&loaded, &orders, &ends);

    loaded.replace_dim(Replaced {
        dim: "customers",
        key: Datum::Int(1),
        old: vec![text("NO"), Datum::Int(10)],
        new: vec![text("SE"), Datum::Int(10)],
    });
    assert_eq!(
        loaded.store.ask(&ends).unwrap().rows,
        [text_row("SE", 7, 3)]
    );
    check(&loaded, &orders, &ends);
}

/// Joined on a column no `by` key reads, a fact keeps bound cells, fewer
/// than its keyed cells where dim rows read alike.
#[test]
fn a_join_off_the_by_keys_keeps_fewer_bound_cells() {
    let orders = [order_of(1, 1, 5), order_of(1, 2, 5), order_of(1, 3, 5)];
    let kinds = [RefKind::Left; 5];
    let mut dims = one_row_dims();
    for (product, tag) in [(2, "red"), (3, "big")] {
        let tags = dims.get_mut("tags").unwrap();
        tags.insert(vec![Datum::Int(product)], vec![vec![text(tag)]]);
    }
    dims.remove("promos");
    let spec = spec_with(kinds, vec![], Shape::Bound);
    let loaded = load(spec, dims, &orders);
    // A cell a product; products 1 and 2 are both red.
    assert_eq!(loaded.store.cells("orders"), Some(3));
    assert_eq!(loaded.store.bound_cells("orders"), Some(2));
    let by_tag = ask(&["n"], &["tag"], vec![]);
    assert_eq!(
        loaded.store.ask(&by_tag).unwrap().rows,
        [key_row(text("big"), 1), key_row(text("red"), 2)]
    );
    check(&loaded, &orders, &by_tag);
}
