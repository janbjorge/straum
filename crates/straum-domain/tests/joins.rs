//! Every join kind, any cardinality, a join through another dim and the
//! view's `WHERE`, each against a reference that joins the live rows
//! to the dims as they are: hand-made cases first, then random rows, dims
//! and changes to both, with buckets sealed under them, over each of the
//! cells an ask may read.

use std::collections::{BTreeMap, HashSet};

mod common;

use common::{
    Rng, Spilled, cond, dim_row, holds, id, lit, percentile, percentile_disc, table, text,
};
use straum_domain::{
    Aggregate, Answer, AnswerColumn, AnswerRow, Ask, AskKey, Attribute, ByKey, ColumnType,
    Condition, Datum, Dim, Fact, Identifier, Literal, Measure, Origin, Ref, RefKind, Spec, Store,
    Value, Via, Width,
};

const HOUR: i64 = 3_600;
/// 2024-01-01 00:00 UTC.
const START: i64 = 1_704_067_200;
const END: i64 = START + 48 * HOUR;

// Row positions of `orders`.
const AT: usize = 0;
const CUSTOMER: usize = 1;
const PRODUCT: usize = 2;
const QTY: usize = 3;
const EVEN: usize = 4;

/// The refs, in join order, and what each is for.
const CUSTOMERS: usize = 0;
const TIERS: usize = 1;
const TAGS: usize = 2;
const PROMOS: usize = 3;
const BLOCKED: usize = 4;

const KEYS: [&str; 6] = ["customer", "product", "country", "label", "tag", "bucket"];
const MEASURES: [&str; 9] = [
    "n", "q", "q_max", "q_min", "q_median", "products", "q_n", "all_even", "any_even",
];
/// A measure with a filter of its own, over a column a join reads.
const FILTERED: &str = "q_no";

fn dim(name: &str, key: &[&str], attrs: &[&str]) -> Dim {
    Dim {
        as_text: Vec::new(),
        name: id(name),
        table: table(name),
        key: key.iter().map(|column| id(column)).collect(),
        attributes: attrs.iter().map(|column| id(column)).collect(),
    }
}

fn attr(name: &str) -> Attribute {
    Attribute {
        name: id(name),
        column: id(name),
    }
}

/// How the orders are keyed and joined, which sets the cells an ask
/// reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Tiers are joined through the customers, so the fact keeps no bound
    /// cells and an ask walks each keyed cell once per row its joins find.
    Chained,
    /// Every ref joins on a `by` key, so bound cells would be as many as
    /// the keyed cells: the fact keeps none, and an ask reads the keyed
    /// cells through the projections.
    ByKeys,
    /// The product is no `by` key, so the joins on it make the keyed cells
    /// finer than the `by` keys, and the fact keeps bound cells.
    Bound,
}

/// Orders by customer and product, joined to customers (and through them
/// to tiers), to tags of any cardinality, and to promos and blocked
/// customers which keep no column, each join of the kind given; `filter`
/// is the view's `WHERE`.
fn spec(kinds: [RefKind; 5], filter: Vec<Condition>) -> Spec {
    spec_with(kinds, filter, Shape::Chained)
}

/// As [`spec`], in the `shape` given; unless chained, tiers are joined on
/// the customer id itself, so every ref joins on a fact column.
fn spec_with(kinds: [RefKind; 5], filter: Vec<Condition>, shape: Shape) -> Spec {
    let orders = Fact {
        as_text: Vec::new(),
        name: id("orders"),
        table: table("orders"),
        time: straum_domain::Time {
            column: id("at"),
            width: Width {
                seconds: u32::try_from(HOUR).unwrap(),
            },
            wider: vec![Width {
                seconds: 24 * u32::try_from(HOUR).unwrap(),
            }],
            origin: Origin::Timestamptz,
        },
        refs: refs(kinds, shape),
        by_keys: (["customer", "product"].into_iter())
            .filter(|key| shape != Shape::Bound || *key != "product")
            .map(|key| ByKey::Column {
                column: id(key),
                name: id(key),
            })
            .collect(),
        measures: measures(),
        filter,
    };
    let dims = vec![
        dim("customers", &["id"], &["country", "tier_id"]),
        dim("tiers", &["id"], &["label"]),
        dim("tags", &["product"], &["tag"]),
        dim("promos", &["product"], &["note"]),
        dim("blocked", &["id"], &["reason"]),
    ];
    Spec::new(dims, vec![orders]).unwrap()
}

/// The refs in join order, each of its kind in `kinds`.
fn refs(kinds: [RefKind; 5], shape: Shape) -> Vec<Ref> {
    let tiers_via = if shape == Shape::Chained {
        Via::Attribute {
            through: CUSTOMERS,
            column: id("tier_id"),
        }
    } else {
        Via::Fact(id("customer"))
    };
    vec![
        join(
            kinds[CUSTOMERS],
            "customers",
            Via::Fact(id("customer")),
            vec![attr("country")],
        ),
        join(kinds[TIERS], "tiers", tiers_via, vec![attr("label")]),
        join(
            kinds[TAGS],
            "tags",
            Via::Fact(id("product")),
            vec![attr("tag")],
        ),
        join(
            kinds[PROMOS],
            "promos",
            Via::Fact(id("product")),
            vec![attr("note")],
        ),
        join(
            kinds[BLOCKED],
            "blocked",
            Via::Fact(id("customer")),
            vec![attr("reason")],
        ),
    ]
}

/// A semi or anti join reads nothing; the same ref as a left or inner
/// join reads the dim's one column.
fn join(kind: RefKind, name: &str, via: Via, attrs: Vec<Attribute>) -> Ref {
    Ref {
        alias: id(name),
        dim: id(name),
        kind,
        via: vec![via],
        reads: match kind {
            RefKind::Left | RefKind::Inner => attrs,
            RefKind::Semi | RefKind::Anti => vec![],
        },
    }
}

fn measure(name: &str, aggregate: Aggregate) -> Measure {
    Measure {
        name: id(name),
        aggregate,
        filter: vec![],
    }
}

fn measures() -> Vec<Measure> {
    vec![
        measure("n", Aggregate::Count),
        measure("q", Aggregate::Sum(id("qty"))),
        measure("q_max", Aggregate::Max(id("qty"))),
        measure("q_min", Aggregate::Min(id("qty"))),
        measure("q_median", percentile(5, 1, "qty")),
        measure("products", Aggregate::CountDistinct(id("product"))),
        measure("q_n", Aggregate::CountNotNull(id("qty"))),
        measure("all_even", Aggregate::BoolAnd(id("even"))),
        measure("any_even", Aggregate::BoolOr(id("even"))),
        Measure {
            filter: vec![cond("country", vec![lit("NO")])],
            ..measure(FILTERED, Aggregate::Sum(id("qty")))
        },
    ]
}

/// Left, left, inner, semi, anti: one of each kind, and the chain.
const KINDS: [RefKind; 5] = [
    RefKind::Left,
    RefKind::Left,
    RefKind::Inner,
    RefKind::Semi,
    RefKind::Anti,
];

/// The dims as the projections key them: dim name to key to its rows'
/// attributes, several rows a key allowed.
type Dims = BTreeMap<&'static str, BTreeMap<Vec<Datum>, Vec<Vec<Datum>>>>;

fn typed(spec: &Spec) -> Store {
    let mut store = Store::new(spec.clone());
    store
        .set_types(
            "orders",
            &[
                ColumnType::Time { zoned: true },
                ColumnType::Int,
                ColumnType::Int,
                ColumnType::Int,
                ColumnType::Bool,
            ],
        )
        .unwrap();
    let text = ColumnType::Text;
    let int = ColumnType::Int;
    for (dim, types) in [
        ("customers", vec![int, text, int]),
        ("tiers", vec![int, text]),
        ("tags", vec![int, text]),
        ("promos", vec![int, text]),
        ("blocked", vec![int, text]),
    ] {
        store.set_types(dim, &types).unwrap();
    }
    store
}

fn load_dims(store: &mut Store, dims: &Dims) {
    for (dim, rows) in dims {
        for (key, each) in rows {
            for attrs in each {
                store.add_row(dim, &dim_row(key, attrs)).unwrap();
            }
        }
    }
}

fn add_dim(
    store: &mut Store,
    dims: &mut Dims,
    dim: &'static str,
    key: Vec<Datum>,
    attrs: Vec<Datum>,
) {
    store.add_row(dim, &dim_row(&key, &attrs)).unwrap();
    dims.entry(dim)
        .or_default()
        .entry(key)
        .or_default()
        .push(attrs);
}

fn remove_dim(
    store: &mut Store,
    dims: &mut Dims,
    dim: &'static str,
    key: &[Datum],
    attrs: &[Datum],
) {
    store.remove_row(dim, &dim_row(key, attrs)).unwrap();
    let rows = dims.get_mut(dim).unwrap().get_mut(key).unwrap();
    let index = rows.iter().position(|held| held == attrs).unwrap();
    rows.remove(index);
    if rows.is_empty() {
        dims.get_mut(dim).unwrap().remove(key);
    }
}

/// `even` is whether `qty` is, a boolean for the bool_and and bool_or.
fn order(at: i64, customer: Datum, product: Datum, qty: Datum) -> Vec<Datum> {
    let even = match qty {
        Datum::Int(qty) => Datum::Bool(qty % 2 == 0),
        _ => Datum::Null,
    };
    vec![Datum::Int(at), customer, product, qty, even]
}

/// One way an order's joins resolve: per ref, the dim row matched.
type JoinRow = Vec<Option<Vec<Datum>>>;

/// Every way an order's joins resolve under `kinds`, in ref order, as
/// PostgreSQL would repeat the row.
fn join_rows(row: &[Datum], dims: &Dims, kinds: &[RefKind; 5], chained: bool) -> Vec<JoinRow> {
    let matches = |dim: &str, key: &Datum| -> Vec<Vec<Datum>> {
        if *key == Datum::Null {
            return Vec::new();
        }
        dims.get(dim)
            .and_then(|rows| rows.get(&vec![key.clone()]))
            .cloned()
            .unwrap_or_default()
    };
    let mut rows: Vec<JoinRow> = vec![Vec::new()];
    for (index, (dim, kind)) in ["customers", "tiers", "tags", "promos", "blocked"]
        .into_iter()
        .zip(kinds)
        .enumerate()
    {
        let mut next = Vec::new();
        for joined in &rows {
            let key = match index {
                CUSTOMERS | BLOCKED => row[CUSTOMER].clone(),
                TIERS if chained => {
                    (joined[CUSTOMERS].as_ref()).map_or(Datum::Null, |customer| customer[1].clone())
                }
                TIERS => row[CUSTOMER].clone(),
                _ => row[PRODUCT].clone(),
            };
            let found = matches(dim, &key);
            let with = |matched: Option<Vec<Datum>>| {
                let mut joined = joined.clone();
                joined.push(matched);
                joined
            };
            match kind {
                RefKind::Left if found.is_empty() => next.push(with(None)),
                RefKind::Left | RefKind::Inner => {
                    next.extend(found.into_iter().map(|matched| with(Some(matched))))
                }
                RefKind::Semi if !found.is_empty() => next.push(with(None)),
                RefKind::Anti if found.is_empty() => next.push(with(None)),
                RefKind::Semi | RefKind::Anti => {}
            }
        }
        rows = next;
    }
    rows
}

/// A key's value for one order under one way its joins resolved.
fn value(row: &[Datum], joined: &JoinRow, key: &Identifier) -> Datum {
    let attr = |join: usize, column: usize| {
        (joined[join].as_ref()).map_or(Datum::Null, |matched| matched[column].clone())
    };
    match key.as_str() {
        "customer" => row[CUSTOMER].clone(),
        "product" => row[PRODUCT].clone(),
        "country" => attr(CUSTOMERS, 0),
        "label" => attr(TIERS, 0),
        "tag" => attr(TAGS, 0),
        other => panic!("no key {other}"),
    }
}

fn passes(row: &[Datum], joined: &JoinRow, conds: &[Condition]) -> bool {
    conds
        .iter()
        .all(|condition| holds(&value(row, joined, &condition.key), &condition.values))
}

/// The reference answer: the ask over the raw rows joined as the dims are: every way each row's
/// joins resolve is one row of the view, as it would be in PostgreSQL.
fn reference(
    spec: &Spec,
    ask: &Ask,
    rows: &[Vec<Datum>],
    dims: &Dims,
    kinds: &[RefKind; 5],
) -> Answer {
    let fact = spec.fact("orders").unwrap();
    let chained = matches!(fact.refs[TIERS].via[0], Via::Attribute { .. });
    let mut groups: BTreeMap<Vec<Datum>, Vec<(&[Datum], JoinRow)>> = BTreeMap::new();
    if ask.keys.is_empty() {
        groups.insert(Vec::new(), Vec::new());
    }
    for row in rows {
        let Datum::Int(at) = row[AT] else { continue };
        if at < ask.from || at >= ask.to {
            continue;
        }
        for joined in join_rows(row, dims, kinds, chained) {
            if !passes(row, &joined, &fact.filter) || !passes(row, &joined, &ask.filter) {
                continue;
            }
            let key = ask
                .keys
                .iter()
                .map(|key| match key {
                    AskKey::Key(name) => value(row, &joined, name),
                    AskKey::Bucket => Datum::Int(
                        Width {
                            seconds: u32::try_from(HOUR).unwrap(),
                        }
                        .bucket_start(at),
                    ),
                })
                .collect();
            groups.entry(key).or_default().push((row, joined));
        }
    }
    let rows = groups
        .into_iter()
        .map(|(keys, rows)| {
            let values = (ask.measures.iter())
                .map(|name| reference_value(fact.measure(name.as_str()).unwrap(), &rows))
                .collect();
            AnswerRow { keys, values }
        })
        .collect();
    let columns = (ask.keys.iter().map(|key| match key {
        AskKey::Key(name) => AnswerColumn {
            name: name.to_string(),
            scale: 0,
            is_time: false,
        },
        AskKey::Bucket => AnswerColumn {
            name: key.to_string(),
            scale: 0,
            is_time: true,
        },
    }))
    .chain(ask.measures.iter().map(|name| AnswerColumn {
        name: name.to_string(),
        scale: 0,
        is_time: false,
    }))
    .collect();
    Answer { columns, rows }
}

fn reference_value(measure: &Measure, rows: &[(&[Datum], JoinRow)]) -> Value {
    let rows: Vec<_> = rows
        .iter()
        .filter(|(row, joined)| passes(row, joined, &measure.filter))
        .collect();
    let ints = |pos: usize| -> Vec<i128> {
        rows.iter()
            .filter_map(|(row, _)| match row[pos] {
                Datum::Int(value) => Some(i128::from(value)),
                _ => None,
            })
            .collect()
    };
    let or_null = |value: Option<i128>| value.map_or(Value::Null, Value::Int);
    let bools = || -> Vec<bool> {
        rows.iter()
            .filter_map(|(row, _)| match row[EVEN] {
                Datum::Bool(value) => Some(value),
                _ => None,
            })
            .collect()
    };
    let or_null_bool = |value: Option<bool>| value.map_or(Value::Null, Value::Bool);
    match &measure.aggregate {
        Aggregate::Count => Value::Int(rows.len() as i128),
        Aggregate::CountNotNull(_) => Value::Int(ints(QTY).len() as i128),
        Aggregate::Sum(_) => {
            let values = ints(QTY);
            or_null((!values.is_empty()).then(|| values.iter().sum()))
        }
        Aggregate::Max(_) => or_null(ints(QTY).into_iter().max()),
        Aggregate::Min(_) => or_null(ints(QTY).into_iter().min()),
        Aggregate::PercentileDisc { fraction, .. } => percentile_disc(ints(QTY), *fraction),
        Aggregate::CountDistinct(_) => {
            let seen: HashSet<_> = rows
                .iter()
                .map(|(row, _)| &row[PRODUCT])
                .filter(|product| **product != Datum::Null)
                .collect();
            Value::Int(seen.len() as i128)
        }
        Aggregate::BoolAnd(_) => or_null_bool(bools().into_iter().min()),
        Aggregate::BoolOr(_) => or_null_bool(bools().into_iter().max()),
        Aggregate::Avg(_) => unreachable!("no average here"),
    }
}

fn ask(measures: &[&str], by: &[&str], filter: Vec<Condition>) -> Ask {
    Ask {
        view: id("orders"),
        measures: measures.iter().map(|name| id(name)).collect(),
        keys: by
            .iter()
            .map(|key| match *key {
                "bucket" => AskKey::Bucket,
                name => AskKey::Key(id(name)),
            })
            .collect(),
        filter,
        from: START,
        to: END,
    }
}

/// Two customers, one in a tier and one not; product 1 with two tags, 2
/// with none; product 1 on promo; customer 2 blocked.
fn small_dims() -> Dims {
    let mut dims = Dims::new();
    let mut put = |dim: &'static str, key: Vec<Datum>, attrs: Vec<Datum>| {
        dims.entry(dim)
            .or_default()
            .entry(key)
            .or_default()
            .push(attrs);
    };
    put(
        "customers",
        vec![Datum::Int(1)],
        vec![text("NO"), Datum::Int(10)],
    );
    put(
        "customers",
        vec![Datum::Int(2)],
        vec![text("SE"), Datum::Null],
    );
    put("tiers", vec![Datum::Int(10)], vec![text("gold")]);
    put("tags", vec![Datum::Int(1)], vec![text("red")]);
    put("tags", vec![Datum::Int(1)], vec![text("big")]);
    put("promos", vec![Datum::Int(1)], vec![text("x")]);
    put("blocked", vec![Datum::Int(2)], vec![text("y")]);
    dims
}

/// A store over `small_dims` and the orders given, with the naive answer
/// to compare against.
fn small(
    kinds: [RefKind; 5],
    filter: Vec<Condition>,
    orders: &[Vec<Datum>],
) -> (Store, Spec, Dims) {
    small_in(spec(kinds, filter), small_dims(), orders)
}

/// A store of `spec` over `dims` and the orders given.
fn small_in(spec: Spec, dims: Dims, orders: &[Vec<Datum>]) -> (Store, Spec, Dims) {
    let mut store = typed(&spec);
    load_dims(&mut store, &dims);
    for row in orders {
        store.add_row("orders", row).unwrap();
    }
    (store, spec, dims)
}

/// [`small_dims`] with one row a key: product 1 has the one tag.
fn one_row_dims() -> Dims {
    let mut dims = small_dims();
    let tags = dims.get_mut("tags").unwrap();
    tags.get_mut(&vec![Datum::Int(1)]).unwrap().truncate(1);
    dims
}

fn check(
    store: &Store,
    spec: &Spec,
    dims: &Dims,
    kinds: &[RefKind; 5],
    rows: &[Vec<Datum>],
    ask: &Ask,
) {
    assert_eq!(
        store.ask(ask).unwrap(),
        reference(spec, ask, rows, dims, kinds),
        "{ask:?}"
    );
}

/// A row of an answer without keys: the measures asked.
fn values(store: &Store, ask: &Ask) -> Vec<Value> {
    store.ask(ask).unwrap().rows[0].values.clone()
}

/// An inner join keeps the orders with a tag, once per tag; a left join
/// keeps every order, those without a tag once under null.
#[test]
fn inner_keeps_matched_rows_and_left_keeps_all() {
    let orders = [
        order(START, Datum::Int(1), Datum::Int(1), Datum::Int(5)),
        order(START, Datum::Int(1), Datum::Int(2), Datum::Int(7)),
    ];
    let mut kinds = KINDS;
    kinds[PROMOS] = RefKind::Left;
    kinds[BLOCKED] = RefKind::Left;
    let (store, spec, dims) = small(kinds, vec![], &orders);
    // Product 1 has two tags: its order counts twice; product 2 has none.
    let by_tag = ask(&["n", "q"], &["tag"], vec![]);
    assert_eq!(
        store.ask(&by_tag).unwrap().rows,
        [
            AnswerRow {
                keys: vec![text("big")],
                values: vec![Value::Int(1), Value::Int(5)]
            },
            AnswerRow {
                keys: vec![text("red")],
                values: vec![Value::Int(1), Value::Int(5)]
            },
        ]
    );
    assert_eq!(
        values(&store, &ask(&["n", "q"], &[], vec![])),
        [Value::Int(2), Value::Int(10)]
    );
    check(&store, &spec, &dims, &kinds, &orders, &by_tag);

    kinds[TAGS] = RefKind::Left;
    let (store, spec, dims) = small(kinds, vec![], &orders);
    assert_eq!(
        store.ask(&by_tag).unwrap().rows,
        [
            AnswerRow {
                keys: vec![Datum::Null],
                values: vec![Value::Int(1), Value::Int(7)]
            },
            AnswerRow {
                keys: vec![text("big")],
                values: vec![Value::Int(1), Value::Int(5)]
            },
            AnswerRow {
                keys: vec![text("red")],
                values: vec![Value::Int(1), Value::Int(5)]
            },
        ]
    );
    assert_eq!(
        values(&store, &ask(&["n", "q"], &[], vec![])),
        [Value::Int(3), Value::Int(17)]
    );
    check(&store, &spec, &dims, &kinds, &orders, &by_tag);
}

/// Several dim rows under one key multiply a count and a sum, and leave a
/// min, a max, a median of one value and a count distinct as they were.
#[test]
fn many_matches_multiply_counts_and_sums_only() {
    let orders = [order(START, Datum::Int(1), Datum::Int(1), Datum::Int(5))];
    let mut kinds = KINDS;
    kinds[PROMOS] = RefKind::Left;
    kinds[BLOCKED] = RefKind::Left;
    let (mut store, spec, mut dims) = small(kinds, vec![], &orders);
    let all = ask(&MEASURES, &[], vec![]);
    let two = vec![
        Value::Int(2),
        Value::Int(10),
        Value::Int(5),
        Value::Int(5),
        Value::Int(5),
        Value::Int(1),
        Value::Int(2),
        Value::Bool(false),
        Value::Bool(false),
    ];
    assert_eq!(values(&store, &all), two);
    check(&store, &spec, &dims, &kinds, &orders, &all);
    // A third tag, then one gone: the multiplicity follows the dim.
    add_dim(
        &mut store,
        &mut dims,
        "tags",
        vec![Datum::Int(1)],
        vec![text("new")],
    );
    assert_eq!(values(&store, &all)[0], Value::Int(3));
    remove_dim(
        &mut store,
        &mut dims,
        "tags",
        &[Datum::Int(1)],
        &[text("red")],
    );
    remove_dim(
        &mut store,
        &mut dims,
        "tags",
        &[Datum::Int(1)],
        &[text("big")],
    );
    assert_eq!(
        values(&store, &all),
        [
            Value::Int(1),
            Value::Int(5),
            Value::Int(5),
            Value::Int(5),
            Value::Int(5),
            Value::Int(1),
            Value::Int(1),
            Value::Bool(false),
            Value::Bool(false),
        ]
    );
    check(&store, &spec, &dims, &kinds, &orders, &all);
}

/// A semi join counts an order once however many rows match, and not at
/// all when none does; an anti join is the opposite.
#[test]
fn semi_counts_once_and_anti_counts_the_unmatched() {
    let orders = [
        order(START, Datum::Int(1), Datum::Int(1), Datum::Int(5)),
        order(START, Datum::Int(2), Datum::Int(2), Datum::Int(7)),
    ];
    // Only semi and anti join, so the counts are theirs alone.
    let mut kinds = KINDS;
    kinds[TAGS] = RefKind::Left;
    let (mut store, spec, mut dims) = small(kinds, vec![], &orders);
    let n = ask(&["n", "q"], &[], vec![]);
    // Order 1: on promo, customer not blocked. Order 2: no promo.
    assert_eq!(values(&store, &n), [Value::Int(2), Value::Int(10)]);
    check(&store, &spec, &dims, &kinds, &orders, &n);
    // A second promo row for product 1 changes nothing; blocking customer
    // 1 drops the order; unblocking customer 2 still leaves it off promo.
    add_dim(
        &mut store,
        &mut dims,
        "promos",
        vec![Datum::Int(1)],
        vec![text("x2")],
    );
    assert_eq!(values(&store, &n), [Value::Int(2), Value::Int(10)]);
    add_dim(
        &mut store,
        &mut dims,
        "blocked",
        vec![Datum::Int(1)],
        vec![text("y")],
    );
    assert_eq!(values(&store, &n), [Value::Int(0), Value::Null]);
    remove_dim(
        &mut store,
        &mut dims,
        "blocked",
        &[Datum::Int(2)],
        &[text("y")],
    );
    assert_eq!(values(&store, &n), [Value::Int(0), Value::Null]);
    add_dim(
        &mut store,
        &mut dims,
        "promos",
        vec![Datum::Int(2)],
        vec![text("x")],
    );
    assert_eq!(values(&store, &n), [Value::Int(1), Value::Int(7)]);
    check(&store, &spec, &dims, &kinds, &orders, &n);

    // Swapped: semi on blocked, anti on promos.
    kinds[PROMOS] = RefKind::Anti;
    kinds[BLOCKED] = RefKind::Semi;
    let (store, spec, dims) = small(kinds, vec![], &orders);
    assert_eq!(values(&store, &n), [Value::Int(1), Value::Int(7)]);
    check(&store, &spec, &dims, &kinds, &orders, &n);
}

/// The answer to `ask`, an ask of one measure by one key, as key and value.
fn labels(store: &Store, ask: &Ask) -> Vec<(Datum, Value)> {
    (store.ask(ask).unwrap().rows.into_iter())
        .map(|row| (row.keys[0].clone(), row.values[0].clone()))
        .collect()
}

/// One dim row under a key of one column, `old` before and `new` after.
struct Replaced {
    dim: &'static str,
    key: Datum,
    old: Vec<Datum>,
    new: Vec<Datum>,
}

fn replace_dim(store: &mut Store, dims: &mut Dims, replaced: Replaced) {
    let key = vec![replaced.key];
    remove_dim(store, dims, replaced.dim, &key, &replaced.old);
    add_dim(store, dims, replaced.dim, key, replaced.new);
}

/// A join through another dim reads the second dim by the first's column:
/// a customer's tier label, null when the customer has no tier or no row.
#[test]
fn a_chain_reads_through_the_first_dim() {
    let orders = [
        order(START, Datum::Int(1), Datum::Int(1), Datum::Int(5)),
        order(START, Datum::Int(2), Datum::Int(1), Datum::Int(7)),
        order(START, Datum::Int(3), Datum::Int(1), Datum::Int(9)),
    ];
    let mut kinds = KINDS;
    kinds[TAGS] = RefKind::Left;
    kinds[PROMOS] = RefKind::Left;
    kinds[BLOCKED] = RefKind::Left;
    let (mut store, spec, mut dims) = small(kinds, vec![], &orders);
    let by_label = ask(&["q"], &["label"], vec![]);
    // Customer 1 is gold; 2 has no tier; 3 has no row. Product 1 has two
    // tags, so every order counts twice.
    assert_eq!(
        labels(&store, &by_label),
        [
            (Datum::Null, Value::Int(32)),
            (text("gold"), Value::Int(10))
        ]
    );
    check(&store, &spec, &dims, &kinds, &orders, &by_label);
    // The tier relabelled, then customer 2 moved into it, then the tier
    // row gone: each moves the answer at once.
    let gold = Replaced {
        dim: "tiers",
        key: Datum::Int(10),
        old: vec![text("gold")],
        new: vec![text("vip")],
    };
    replace_dim(&mut store, &mut dims, gold);
    assert_eq!(
        labels(&store, &by_label),
        [(Datum::Null, Value::Int(32)), (text("vip"), Value::Int(10))]
    );
    let untiered = Replaced {
        dim: "customers",
        key: Datum::Int(2),
        old: vec![text("SE"), Datum::Null],
        new: vec![text("SE"), Datum::Int(10)],
    };
    replace_dim(&mut store, &mut dims, untiered);
    assert_eq!(
        labels(&store, &by_label),
        [(Datum::Null, Value::Int(18)), (text("vip"), Value::Int(24))]
    );
    check(&store, &spec, &dims, &kinds, &orders, &by_label);
    remove_dim(
        &mut store,
        &mut dims,
        "tiers",
        &[Datum::Int(10)],
        &[text("vip")],
    );
    assert_eq!(labels(&store, &by_label), [(Datum::Null, Value::Int(42))]);
    // Filtered on the chained value too.
    add_dim(
        &mut store,
        &mut dims,
        "tiers",
        vec![Datum::Int(10)],
        vec![text("vip")],
    );
    let vip = ask(&["n"], &[], vec![cond("label", vec![lit("vip")])]);
    assert_eq!(values(&store, &vip), [Value::Int(4)]);
    check(&store, &spec, &dims, &kinds, &orders, &vip);
}

/// The view's `WHERE` drops rows from every measure of every ask, and
/// follows the dim when it names a dim column.
#[test]
fn where_drops_rows_from_every_ask() {
    let orders = [
        order(START, Datum::Int(1), Datum::Int(1), Datum::Int(5)),
        order(START, Datum::Int(2), Datum::Int(2), Datum::Int(7)),
        order(START + HOUR, Datum::Int(3), Datum::Int(2), Datum::Int(9)),
    ];
    let mut kinds = KINDS;
    kinds[TAGS] = RefKind::Left;
    kinds[PROMOS] = RefKind::Left;
    kinds[BLOCKED] = RefKind::Left;
    let filter = vec![
        cond("country", vec![lit("NO"), lit("SE")]),
        cond("product", vec![Literal::Int(2)]),
    ];
    let (mut store, spec, mut dims) = small(kinds, filter, &orders);
    let all = ask(&["n", "q"], &[], vec![]);
    assert_eq!(values(&store, &all), [Value::Int(1), Value::Int(7)]);
    let by_customer = ask(&["n"], &["customer"], vec![]);
    assert_eq!(
        store.ask(&by_customer).unwrap().rows,
        [AnswerRow {
            keys: vec![Datum::Int(2)],
            values: vec![Value::Int(1)]
        }]
    );
    check(&store, &spec, &dims, &kinds, &orders, &all);
    check(&store, &spec, &dims, &kinds, &orders, &by_customer);
    // Customer 3 gets a row in NO: its order now passes.
    add_dim(
        &mut store,
        &mut dims,
        "customers",
        vec![Datum::Int(3)],
        vec![text("NO"), Datum::Null],
    );
    assert_eq!(values(&store, &all), [Value::Int(2), Value::Int(16)]);
    check(&store, &spec, &dims, &kinds, &orders, &all);
    // Without the WHERE every order counts, the one with two tags twice.
    let (store, spec, dims) = small(kinds, vec![], &orders);
    assert_eq!(values(&store, &all), [Value::Int(4), Value::Int(26)]);
    check(&store, &spec, &dims, &kinds, &orders, &all);
}

/// A chain's key or a semi join's key that is null joins nothing, and an
/// inner join on a null key keeps nothing.
#[test]
fn null_keys_join_nothing() {
    let orders = [
        order(START, Datum::Null, Datum::Int(1), Datum::Int(5)),
        order(START, Datum::Int(1), Datum::Null, Datum::Int(7)),
    ];
    let mut kinds = KINDS;
    kinds[PROMOS] = RefKind::Left;
    kinds[BLOCKED] = RefKind::Left;
    let (store, spec, dims) = small(kinds, vec![], &orders);
    // Inner on tags: the null product keeps nothing; the null customer's
    // order has two tags.
    let all = ask(&["n"], &[], vec![]);
    assert_eq!(values(&store, &all), [Value::Int(2)]);
    check(&store, &spec, &dims, &kinds, &orders, &all);
    let by = ask(&["n"], &["country", "label", "tag"], vec![]);
    check(&store, &spec, &dims, &kinds, &orders, &by);

    kinds[TAGS] = RefKind::Left;
    kinds[PROMOS] = RefKind::Semi;
    let (store, spec, dims) = small(kinds, vec![], &orders);
    assert_eq!(values(&store, &all), [Value::Int(2)]);
    check(&store, &spec, &dims, &kinds, &orders, &all);
}

/// Joined on its `by` keys alone, a fact keeps no bound cells: they
/// would be as many as its keyed cells. An ask reads the keyed cells
/// through the dims as they are, once each while every key holds one row,
/// passing over the cells an inner, semi or anti join drops.
#[test]
fn joins_on_by_keys_keep_no_bound_cells() {
    let orders = [
        order(START, Datum::Int(1), Datum::Int(1), Datum::Int(5)),
        order(START, Datum::Int(1), Datum::Int(2), Datum::Int(7)),
        order(START, Datum::Int(2), Datum::Int(1), Datum::Int(9)),
    ];
    let spec = spec_with(KINDS, vec![], Shape::ByKeys);
    let (mut store, spec, mut dims) = small_in(spec, one_row_dims(), &orders);
    assert_eq!(store.cells("orders"), Some(3));
    assert_eq!(store.bound_cells("orders"), None);
    // Product 2 has no tag and is on no promo; customer 2 is blocked.
    let by_tag = ask(&["n", "q"], &["tag"], vec![]);
    let row = |tag: &str, count: i128, quantity: i128| AnswerRow {
        keys: vec![text(tag)],
        values: vec![Value::Int(count), Value::Int(quantity)],
    };
    assert_eq!(store.ask(&by_tag).unwrap().rows, [row("red", 1, 5)]);
    check(&store, &spec, &dims, &KINDS, &orders, &by_tag);
    // The next ask reads the dims as they are then.
    for (dim, attr) in [("tags", "big"), ("promos", "x")] {
        add_dim(
            &mut store,
            &mut dims,
            dim,
            vec![Datum::Int(2)],
            vec![text(attr)],
        );
    }
    assert_eq!(
        store.ask(&by_tag).unwrap().rows,
        [row("big", 1, 7), row("red", 1, 5)]
    );
    remove_dim(
        &mut store,
        &mut dims,
        "blocked",
        &[Datum::Int(2)],
        &[text("y")],
    );
    assert_eq!(
        store.ask(&by_tag).unwrap().rows,
        [row("big", 1, 7), row("red", 2, 14)]
    );
    check(&store, &spec, &dims, &KINDS, &orders, &by_tag);
    assert_eq!(store.bound_cells("orders"), None);
}

/// A max and a min in a view joined on its `by` keys, which answers from
/// its keyed cells: the largest order leaving reveals the next, and a
/// customer who moves takes their orders' ends to the new country.
#[test]
fn a_max_over_joined_keys_follows_deletes_and_dims() {
    let mut orders = vec![
        order(START, Datum::Int(1), Datum::Int(1), Datum::Int(5)),
        order(START, Datum::Int(1), Datum::Int(1), Datum::Int(9)),
        order(START, Datum::Int(1), Datum::Int(2), Datum::Int(7)),
        order(START, Datum::Int(2), Datum::Int(1), Datum::Int(3)),
    ];
    let kinds = [RefKind::Left; 5];
    let spec = spec_with(kinds, vec![], Shape::ByKeys);
    let (mut store, spec, mut dims) = small_in(spec, one_row_dims(), &orders);
    assert_eq!(store.bound_cells("orders"), None);
    let ends = ask(&["q_max", "q_min"], &["country"], vec![]);
    let row = |country: &str, max: i128, min: i128| AnswerRow {
        keys: vec![text(country)],
        values: vec![Value::Int(max), Value::Int(min)],
    };
    assert_eq!(
        store.ask(&ends).unwrap().rows,
        [row("NO", 9, 5), row("SE", 3, 3)]
    );
    check(&store, &spec, &dims, &kinds, &orders, &ends);

    let largest = orders.remove(1);
    store.remove_row("orders", &largest).unwrap();
    assert_eq!(
        store.ask(&ends).unwrap().rows,
        [row("NO", 7, 5), row("SE", 3, 3)]
    );
    check(&store, &spec, &dims, &kinds, &orders, &ends);

    let home = [text("NO"), Datum::Int(10)];
    remove_dim(&mut store, &mut dims, "customers", &[Datum::Int(1)], &home);
    add_dim(
        &mut store,
        &mut dims,
        "customers",
        vec![Datum::Int(1)],
        vec![text("SE"), Datum::Int(10)],
    );
    assert_eq!(store.ask(&ends).unwrap().rows, [row("SE", 7, 3)]);
    check(&store, &spec, &dims, &kinds, &orders, &ends);
}

/// Joined on a column no `by` key reads, a fact keeps bound cells, fewer
/// than its keyed cells where dim rows read alike.
#[test]
fn a_join_off_the_by_keys_keeps_fewer_bound_cells() {
    let orders: Vec<Vec<Datum>> = (1..=3)
        .map(|product| order(START, Datum::Int(1), Datum::Int(product), Datum::Int(5)))
        .collect();
    let kinds = [RefKind::Left; 5];
    let mut dims = one_row_dims();
    for (product, tag) in [(2, "red"), (3, "big")] {
        (dims.get_mut("tags").unwrap()).insert(vec![Datum::Int(product)], vec![vec![text(tag)]]);
    }
    dims.remove("promos");
    let spec = spec_with(kinds, vec![], Shape::Bound);
    let (store, spec, dims) = small_in(spec, dims, &orders);
    // A cell a product; products 1 and 2 are both red.
    assert_eq!(store.cells("orders"), Some(3));
    assert_eq!(store.bound_cells("orders"), Some(2));
    let by_tag = ask(&["n"], &["tag"], vec![]);
    assert_eq!(
        store.ask(&by_tag).unwrap().rows,
        [
            AnswerRow {
                keys: vec![text("big")],
                values: vec![Value::Int(1)]
            },
            AnswerRow {
                keys: vec![text("red")],
                values: vec![Value::Int(2)]
            },
        ]
    );
    check(&store, &spec, &dims, &kinds, &orders, &by_tag);
}

/// The kinds a seed runs with. Over eight seeds: every way the three
/// joins that read a column are left or inner, and each kind twice for
/// each of the two that may read none.
fn kinds_of(seed: u64) -> [RefKind; 5] {
    let reads = |bit: u64| match seed >> bit & 1 {
        0 => RefKind::Left,
        _ => RefKind::Inner,
    };
    let any = [RefKind::Left, RefKind::Inner, RefKind::Semi, RefKind::Anti];
    [
        reads(0),
        reads(1),
        reads(2),
        any[usize::try_from(seed % 4).unwrap()],
        any[usize::try_from((seed / 2 + 1) % 4).unwrap()],
    ]
}

impl Rng {
    /// An order in the two days, or now and then one without a time.
    fn random_order(&mut self) -> Vec<Datum> {
        let mut order = order(
            START + HOUR * self.int(0, 48),
            self.maybe(1, 6, 5),
            self.maybe(1, 5, 5),
            self.maybe(0, 10, 10),
        );
        if self.chance(3) {
            order[AT] = Datum::Null;
        }
        order
    }

    /// A random dim row: which dim, its key and its attributes; tiers
    /// are keyed as the spec joins them. Now and then the key is null,
    /// which nothing joins.
    fn dim_row(&mut self, chained: bool) -> (&'static str, Vec<Datum>, Vec<Datum>) {
        match self.below(5) {
            0 => (
                "customers",
                vec![self.maybe(1, 6, 5)],
                vec![text(self.pick(&["NO", "SE", "DK"])), self.maybe(10, 13, 30)],
            ),
            1 => (
                "tiers",
                vec![if chained {
                    self.maybe(10, 13, 5)
                } else {
                    self.maybe(1, 6, 5)
                }],
                vec![text(self.pick(&["gold", "silver"]))],
            ),
            2 => (
                "tags",
                vec![self.maybe(1, 5, 5)],
                vec![text(self.pick(&["red", "big", "new"]))],
            ),
            3 => (
                "promos",
                vec![self.maybe(1, 5, 5)],
                vec![text(self.pick(&["x", "x2"]))],
            ),
            _ => (
                "blocked",
                vec![self.maybe(1, 6, 5)],
                vec![text(self.pick(&["y", "y2"]))],
            ),
        }
    }

    /// An ask of the keys the view has: the product is one unless the
    /// view is [`Shape::Bound`].
    fn random_ask(&mut self, shape: Shape) -> Ask {
        let keyed = |key: &str| shape != Shape::Bound || key != "product";
        let by: Vec<&str> = (KEYS.iter().copied())
            .filter(|key| self.chance(30) && keyed(key))
            .collect();
        let mut measures: Vec<&str> = (MEASURES.iter().copied())
            .chain([FILTERED])
            .filter(|_| self.chance(50))
            .collect();
        if measures.is_empty() {
            measures.push("n");
        }
        let pool = [
            cond("country", vec![lit("NO")]),
            cond("label", vec![lit("gold"), lit("silver")]),
            cond("tag", vec![lit("red")]),
            cond("customer", vec![Literal::Int(1), Literal::Int(2)]),
            cond("product", vec![Literal::Int(3)]),
        ];
        let filter = (pool.into_iter())
            .filter(|cond| self.chance(20) && keyed(cond.key.as_str()))
            .collect();
        let mut ask = ask(&measures, &by, filter);
        if self.chance(30) {
            ask.from = START + HOUR * self.int(0, 24);
            ask.to = ask.from + HOUR * self.int(1, 24);
        }
        ask
    }
}

/// The rows of a dim the store holds: those under a key that is not
/// null.
fn held(dims: &Dims, dim: &str) -> usize {
    let rows = dims.get(dim).into_iter().flatten();
    rows.filter(|(key, _)| !key.contains(&Datum::Null))
        .map(|(_, rows)| rows.len())
        .sum()
}

/// What a random run goes through.
#[derive(Clone, Copy)]
struct Run {
    kinds: [RefKind; 5],
    shape: Shape,
    /// Buckets are sealed and unsealed as the run goes, and an ask over
    /// one reads the keyed cells, thawed.
    sealing: bool,
    /// No dim key holds a second row, so no join repeats a cell.
    one_row_a_key: bool,
}

/// Random rows, dims and changes to both, checked against the reference
/// every few steps; returns the sealed buckets asks read. Only a
/// [`Shape::Bound`] fact answers from bound cells; the others read the
/// keyed cells through the projections.
fn run(seed: u64, run: Run) -> usize {
    let mut walk = Walk::new(seed, run);
    for _ in 0..walk.rng.below(30) {
        walk.add_random_dim_row();
    }
    for step in 0..400 {
        walk.take_a_step();
        if step % 50 == 49 {
            walk.check(step);
        }
    }
    walk.drain()
}

/// A random run under way: the store, and beside it the rows and dims
/// the reference joins.
struct Walk {
    seed: u64,
    run: Run,
    rng: Rng,
    spec: Spec,
    store: Store,
    dims: Dims,
    rows: Vec<Vec<Datum>>,
    spilled: Spilled,
}

impl Walk {
    fn new(seed: u64, run: Run) -> Walk {
        let mut rng = Rng(seed);
        let filter = if rng.chance(50) {
            vec![cond("country", vec![lit("NO"), lit("SE")])]
        } else {
            Vec::new()
        };
        let spec = spec_with(run.kinds, filter, run.shape);
        let store = typed(&spec);
        Walk {
            seed,
            run,
            rng,
            spec,
            store,
            dims: Dims::new(),
            rows: Vec::new(),
            spilled: Spilled::new("orders"),
        }
    }

    fn chained(&self) -> bool {
        self.run.shape == Shape::Chained
    }

    /// One change to the rows or the dims, after the buckets that end by
    /// a random time are sealed, if the run seals.
    fn take_a_step(&mut self) {
        if self.run.sealing && self.rng.chance(10) {
            let before = START + HOUR * self.rng.int(0, 60);
            self.spilled.seal(&mut self.rng, &mut self.store, before);
        }
        match self.rng.below(40) {
            0..=7 => self.add_random_dim_row(),
            8..=11 => self.remove_random_dim_row(),
            // A dim or the orders truncated.
            12 if self.rng.chance(20) => {
                let (dim, _, _) = self.rng.dim_row(self.chained());
                self.store.clear(dim);
                self.dims.remove(dim);
            }
            13 if self.rng.chance(10) => {
                self.store.clear("orders");
                self.spilled.forget();
                self.rows.clear();
            }
            14..=17 if !self.rows.is_empty() => self.remove_random_order(),
            18..=21 if !self.rows.is_empty() => self.update_random_order(),
            _ => self.add_random_order(),
        }
    }

    /// A random dim row, unless every key holds one row at most and its
    /// key holds one already.
    fn add_random_dim_row(&mut self) {
        let (dim, key, attrs) = self.rng.dim_row(self.chained());
        let taken = self.run.one_row_a_key
            && (self.dims.get(dim)).is_some_and(|rows| rows.contains_key(&key));
        if !taken {
            add_dim(&mut self.store, &mut self.dims, dim, key, attrs);
        }
    }

    /// A random dim row removed, if it is held.
    fn remove_random_dim_row(&mut self) {
        let (dim, key, attrs) = self.rng.dim_row(self.chained());
        let held = (self.dims.get(dim))
            .and_then(|by_key| by_key.get(&key))
            .is_some_and(|rows| rows.contains(&attrs));
        if held {
            remove_dim(&mut self.store, &mut self.dims, dim, &key, &attrs);
        }
    }

    fn add_random_order(&mut self) {
        let row = self.rng.random_order();
        self.spilled.unseal(&mut self.store, &row);
        self.store.add_row("orders", &row).unwrap();
        self.rows.push(row);
    }

    fn remove_random_order(&mut self) {
        let row = self.rows.swap_remove(self.rng.below(self.rows.len()));
        self.spilled.unseal(&mut self.store, &row);
        self.store.remove_row("orders", &row).unwrap();
    }

    fn update_random_order(&mut self) {
        let i = self.rng.below(self.rows.len());
        let new = self.rng.random_order();
        self.spilled.unseal(&mut self.store, &self.rows[i]);
        self.spilled.unseal(&mut self.store, &new);
        self.store
            .update_row("orders", &self.rows[i], &new)
            .unwrap();
        self.rows[i] = new;
    }

    /// The cells the store keeps, the dim rows it holds, and random asks
    /// against the reference.
    fn check(&mut self, step: u32) {
        let Run { kinds, shape, .. } = self.run;
        let seed = self.seed;
        let bound = self.store.bound_cells("orders").is_some();
        assert_eq!(bound, shape == Shape::Bound, "seed {seed}, step {step}");
        for dim in ["customers", "tiers", "tags", "promos", "blocked"] {
            assert_eq!(
                self.store.dim_rows(dim),
                Some(held(&self.dims, dim)),
                "seed {seed}, step {step}: {dim} holds a row with a null key"
            );
        }
        for _ in 0..6 {
            let ask = self.rng.random_ask(shape);
            assert_eq!(
                self.spilled.ask(&self.store, &ask),
                reference(&self.spec, &ask, &self.rows, &self.dims, &kinds),
                "seed {seed}, step {step}, kinds {kinds:?}, {ask:?}"
            );
        }
    }

    /// Every row removed, and no cell left behind; returns the sealed
    /// buckets asks read.
    fn drain(self) -> usize {
        let Walk {
            seed,
            run,
            mut store,
            rows,
            mut spilled,
            ..
        } = self;
        for row in rows {
            spilled.unseal(&mut store, &row);
            store.remove_row("orders", &row).unwrap();
        }
        assert_eq!(
            store.cells("orders"),
            Some(0),
            "seed {seed}: a cell outlived its rows"
        );
        if run.shape == Shape::Bound {
            assert_eq!(
                store.bound_cells("orders"),
                Some(0),
                "seed {seed}: a bound cell outlived its rows"
            );
        }
        spilled.read
    }
}

/// Eight seeds of [`run`], each with the kinds [`kinds_of`] gives it;
/// returns the sealed buckets asks read.
fn runs(shape: Shape, sealing: bool, one_row_a_key: bool) -> usize {
    (0..8)
        .map(|seed| {
            let kinds = kinds_of(seed);
            run(
                seed,
                Run {
                    kinds,
                    shape,
                    sealing,
                    one_row_a_key,
                },
            )
        })
        .sum()
}

#[test]
fn joins_match_the_naive_reference() {
    assert_eq!(runs(Shape::Chained, false, false), 0);
}

#[test]
fn by_key_joins_match_the_naive_reference() {
    assert_eq!(runs(Shape::ByKeys, false, false), 0);
}

/// With one row a key no join repeats a cell, so an ask over the keyed
/// cells walks them once each and passes over those an inner, semi or
/// anti join drops.
#[test]
fn one_row_a_key_joins_match_the_naive_reference() {
    assert_eq!(runs(Shape::ByKeys, false, true), 0);
}

#[test]
fn bound_joins_match_the_naive_reference() {
    assert_eq!(runs(Shape::Bound, false, false), 0);
}

#[test]
fn sealed_joins_match_the_naive_reference() {
    let read = runs(Shape::Chained, true, false);
    assert!(read > 500, "asks read {read} sealed buckets");
}

#[test]
fn sealed_by_key_joins_match_the_naive_reference() {
    let read = runs(Shape::ByKeys, true, false);
    assert!(read > 500, "asks read {read} sealed buckets");
}

#[test]
fn sealed_one_row_a_key_joins_match_the_naive_reference() {
    let read = runs(Shape::ByKeys, true, true);
    assert!(read > 500, "asks read {read} sealed buckets");
}

#[test]
fn sealed_bound_joins_match_the_naive_reference() {
    let read = runs(Shape::Bound, true, false);
    assert!(read > 500, "asks read {read} sealed buckets");
}

/// Left joins alone count a cell once while every dim key holds one
/// row. A key with several rows repeats the cell, in a sealed bucket's
/// keyed cells as in the bound cells.
#[test]
fn sealed_left_joins_match_the_naive_reference() {
    let read: usize = (0..8)
        .map(|seed| {
            run(
                seed,
                Run {
                    kinds: [RefKind::Left; 5],
                    shape: Shape::Bound,
                    sealing: true,
                    one_row_a_key: false,
                },
            )
        })
        .sum();
    assert!(read > 500, "asks read {read} sealed buckets");
}
