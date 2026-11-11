//! Every join kind, any cardinality, a join through another dim and the
//! view's `WHERE`, each against a reference that joins the live rows
//! to the dims as they are: hand-made cases first, then random rows, dims
//! and changes to both.

use std::collections::{BTreeMap, HashSet};

mod common;

use common::{Rng, cond, dim_row, holds, id, lit, table, text};
use straum_domain::{
    Aggregate, Answer, AnswerColumn, AnswerRow, Ask, AskKey, Attribute, ByKey, ColumnType,
    Condition, Datum, Dim, Fact, Identifier, Literal, Measure, Origin, Ref, RefKind, Spec, Store,
    TimeKey, Value, Via, Width,
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

/// The refs, in join order, and what each is for.
const CUSTOMERS: usize = 0;
const TIERS: usize = 1;
const TAGS: usize = 2;
const PROMOS: usize = 3;
const BLOCKED: usize = 4;

const KEYS: [&str; 6] = ["customer", "product", "country", "label", "tag", "bucket"];
const MEASURES: [&str; 5] = ["n", "q", "q_max", "q_min", "products"];

fn dim(name: &str, key: &[&str], attrs: &[&str]) -> Dim {
    Dim {
        as_text: Vec::new(),
        name: id(name),
        table: table(name),
        key: key.iter().map(|k| id(k)).collect(),
        attributes: attrs.iter().map(|a| id(a)).collect(),
    }
}

fn attr(name: &str) -> Attribute {
    Attribute {
        name: id(name),
        column: id(name),
    }
}

/// Orders by customer and product, joined to customers (and through them
/// to tiers), to tags of any cardinality, and to promos and blocked
/// customers which keep no column, each join of the kind given; `filter`
/// is the view's `WHERE`.
fn spec(kinds: [RefKind; 5], filter: Vec<Condition>) -> Spec {
    spec_with(kinds, filter, true)
}

/// As [`spec`]; without `chained`, tiers are joined on the customer id
/// itself, so every ref joins on a fact column and the fact keeps bound
/// cells.
fn spec_with(kinds: [RefKind; 5], filter: Vec<Condition>, chained: bool) -> Spec {
    let m = |name, aggregate| Measure {
        name: id(name),
        aggregate,
        filter: vec![],
    };
    // A semi or anti join reads nothing; the same ref as a left or inner
    // join reads the dim's one column.
    let r = |i: usize, name: &str, via: Via, attrs: Vec<Attribute>| Ref {
        alias: id(name),
        dim: id(name),
        kind: kinds[i],
        via: vec![via],
        reads: match kinds[i] {
            RefKind::Left | RefKind::Inner => attrs,
            RefKind::Semi | RefKind::Anti => vec![],
        },
    };
    let orders = Fact {
        as_text: Vec::new(),
        name: id("orders"),
        table: table("orders"),
        time: straum_domain::Time {
            column: id("at"),
            width: Width {
                seconds: HOUR as u32,
            },
            wider: vec![Width {
                seconds: 24 * HOUR as u32,
            }],
            origin: Origin::Timestamptz,
        },
        refs: vec![
            r(
                CUSTOMERS,
                "customers",
                Via::Fact(id("customer")),
                vec![attr("country")],
            ),
            r(
                TIERS,
                "tiers",
                if chained {
                    Via::Attribute {
                        through: CUSTOMERS,
                        column: id("tier_id"),
                    }
                } else {
                    Via::Fact(id("customer"))
                },
                vec![attr("label")],
            ),
            r(TAGS, "tags", Via::Fact(id("product")), vec![attr("tag")]),
            r(
                PROMOS,
                "promos",
                Via::Fact(id("product")),
                vec![attr("note")],
            ),
            r(
                BLOCKED,
                "blocked",
                Via::Fact(id("customer")),
                vec![attr("reason")],
            ),
        ],
        by_keys: vec![
            ByKey::Column {
                column: id("customer"),
                name: id("customer"),
            },
            ByKey::Column {
                column: id("product"),
                name: id("product"),
            },
        ],
        measures: vec![
            m("n", Aggregate::Count),
            m("q", Aggregate::Sum(id("qty"))),
            m("q_max", Aggregate::Max(id("qty"))),
            m("q_min", Aggregate::Min(id("qty"))),
            m("products", Aggregate::CountDistinct(id("product"))),
        ],
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
    let i = rows.iter().position(|a| a == attrs).unwrap();
    rows.remove(i);
    if rows.is_empty() {
        dims.get_mut(dim).unwrap().remove(key);
    }
}

fn order(at: i64, customer: Datum, product: Datum, qty: Datum) -> Vec<Datum> {
    vec![Datum::Int(at), customer, product, qty]
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
    for (i, (dim, kind)) in ["customers", "tiers", "tags", "promos", "blocked"]
        .into_iter()
        .zip(kinds)
        .enumerate()
    {
        let mut next = Vec::new();
        for joined in &rows {
            let key = match i {
                CUSTOMERS | BLOCKED => row[CUSTOMER].clone(),
                TIERS if chained => {
                    (joined[CUSTOMERS].as_ref()).map_or(Datum::Null, |c| c[1].clone())
                }
                TIERS => row[CUSTOMER].clone(),
                _ => row[PRODUCT].clone(),
            };
            let found = matches(dim, &key);
            let with = |m: Option<Vec<Datum>>| {
                let mut joined = joined.clone();
                joined.push(m);
                joined
            };
            match kind {
                RefKind::Left if found.is_empty() => next.push(with(None)),
                RefKind::Left | RefKind::Inner => {
                    next.extend(found.into_iter().map(|m| with(Some(m))))
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
    let attr = |r: usize, i: usize| (joined[r].as_ref()).map_or(Datum::Null, |m| m[i].clone());
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
        .all(|c| holds(&value(row, joined, &c.key), &c.values))
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
        let Datum::Int(t) = row[AT] else { continue };
        if t < ask.from || t >= ask.to {
            continue;
        }
        for joined in join_rows(row, dims, kinds, chained) {
            if !passes(row, &joined, &fact.filter) || !passes(row, &joined, &ask.filter) {
                continue;
            }
            let key = ask
                .keys
                .iter()
                .map(|k| match k {
                    AskKey::Key(name) => value(row, &joined, name),
                    AskKey::Time(tk) => Datum::Int(tk.bucket_start(
                        t,
                        Width {
                            seconds: HOUR as u32,
                        },
                    )),
                })
                .collect();
            groups.entry(key).or_default().push((row, joined));
        }
    }
    let rows = groups
        .into_iter()
        .map(|(keys, rows)| {
            let values = (ask.measures.iter())
                .map(|name| measure(fact.measure(name.as_str()).unwrap(), &rows))
                .collect();
            AnswerRow { keys, values }
        })
        .collect();
    let columns = (ask.keys.iter().map(|k| match k {
        AskKey::Key(p) => AnswerColumn {
            name: p.to_string(),
            scale: 0,
            is_time: false,
        },
        AskKey::Time(t) => AnswerColumn {
            name: t.to_string(),
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

fn measure(m: &Measure, rows: &[(&[Datum], JoinRow)]) -> Value {
    let rows: Vec<_> = rows
        .iter()
        .filter(|(r, j)| passes(r, j, &m.filter))
        .collect();
    let ints = |pos: usize| -> Vec<i128> {
        rows.iter()
            .filter_map(|(r, _)| match r[pos] {
                Datum::Int(v) => Some(i128::from(v)),
                _ => None,
            })
            .collect()
    };
    let or_null = |v: Option<i128>| v.map_or(Value::Null, Value::Int);
    match &m.aggregate {
        Aggregate::Count => Value::Int(rows.len() as i128),
        Aggregate::Sum(_) => {
            let v = ints(QTY);
            or_null((!v.is_empty()).then(|| v.iter().sum()))
        }
        Aggregate::Max(_) => or_null(ints(QTY).into_iter().max()),
        Aggregate::Min(_) => or_null(ints(QTY).into_iter().min()),
        Aggregate::CountDistinct(_) => {
            let seen: HashSet<_> = rows
                .iter()
                .map(|(r, _)| &r[PRODUCT])
                .filter(|d| **d != Datum::Null)
                .collect();
            Value::Int(seen.len() as i128)
        }
        Aggregate::Avg(_) => unreachable!("no average here"),
    }
}

fn ask(measures: &[&str], by: &[&str], filter: Vec<Condition>) -> Ask {
    Ask {
        view: id("orders"),
        measures: measures.iter().map(|m| id(m)).collect(),
        keys: by
            .iter()
            .map(|k| match *k {
                "bucket" => AskKey::Time(TimeKey::Bucket),
                k => AskKey::Key(id(k)),
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
    let spec = spec(kinds, filter);
    let mut store = typed(&spec);
    let dims = small_dims();
    load_dims(&mut store, &dims);
    for row in orders {
        store.add_row("orders", row).unwrap();
    }
    (store, spec, dims)
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
/// min, a max and a count distinct as they were.
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
        Value::Int(1),
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
            Value::Int(1)
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
    let labels = |store: &Store| -> Vec<(Datum, Value)> {
        (store.ask(&by_label).unwrap().rows.into_iter())
            .map(|r| (r.keys[0].clone(), r.values[0].clone()))
            .collect()
    };
    // Customer 1 is gold; 2 has no tier; 3 has no row. Product 1 has two
    // tags, so every order counts twice.
    assert_eq!(
        labels(&store),
        [
            (Datum::Null, Value::Int(32)),
            (text("gold"), Value::Int(10))
        ]
    );
    check(&store, &spec, &dims, &kinds, &orders, &by_label);
    // The tier relabelled, then customer 2 moved into it, then the tier
    // row gone: each moves the answer at once.
    remove_dim(
        &mut store,
        &mut dims,
        "tiers",
        &[Datum::Int(10)],
        &[text("gold")],
    );
    add_dim(
        &mut store,
        &mut dims,
        "tiers",
        vec![Datum::Int(10)],
        vec![text("vip")],
    );
    assert_eq!(
        labels(&store),
        [(Datum::Null, Value::Int(32)), (text("vip"), Value::Int(10))]
    );
    remove_dim(
        &mut store,
        &mut dims,
        "customers",
        &[Datum::Int(2)],
        &[text("SE"), Datum::Null],
    );
    add_dim(
        &mut store,
        &mut dims,
        "customers",
        vec![Datum::Int(2)],
        vec![text("SE"), Datum::Int(10)],
    );
    assert_eq!(
        labels(&store),
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
    assert_eq!(labels(&store), [(Datum::Null, Value::Int(42))]);
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

impl Rng {
    fn kinds(&mut self) -> [RefKind; 5] {
        let any = [RefKind::Left, RefKind::Inner, RefKind::Semi, RefKind::Anti];
        let reads = [RefKind::Left, RefKind::Inner];
        [
            *self.pick(&reads),
            *self.pick(&reads),
            *self.pick(&reads),
            *self.pick(&any),
            *self.pick(&any),
        ]
    }

    fn random_order(&mut self) -> Vec<Datum> {
        order(
            START + HOUR * self.int(0, 48),
            self.maybe(1, 6, 5),
            self.maybe(1, 5, 5),
            self.maybe(0, 10, 10),
        )
    }

    /// A random dim row: which dim, its key and its attributes; tiers
    /// are keyed as the spec joins them.
    fn dim_row(&mut self, chained: bool) -> (&'static str, Vec<Datum>, Vec<Datum>) {
        match self.below(5) {
            0 => (
                "customers",
                vec![Datum::Int(self.int(1, 6))],
                vec![text(self.pick(&["NO", "SE", "DK"])), self.maybe(10, 13, 30)],
            ),
            1 => (
                "tiers",
                vec![Datum::Int(if chained {
                    self.int(10, 13)
                } else {
                    self.int(1, 6)
                })],
                vec![text(self.pick(&["gold", "silver"]))],
            ),
            2 => (
                "tags",
                vec![Datum::Int(self.int(1, 5))],
                vec![text(self.pick(&["red", "big", "new"]))],
            ),
            3 => (
                "promos",
                vec![Datum::Int(self.int(1, 5))],
                vec![text(self.pick(&["x", "x2"]))],
            ),
            _ => (
                "blocked",
                vec![Datum::Int(self.int(1, 6))],
                vec![text(self.pick(&["y", "y2"]))],
            ),
        }
    }

    fn random_ask(&mut self) -> Ask {
        let by: Vec<&str> = KEYS.iter().copied().filter(|_| self.chance(30)).collect();
        let mut measures: Vec<&str> = MEASURES
            .iter()
            .copied()
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
        let filter = pool.into_iter().filter(|_| self.chance(20)).collect();
        let mut ask = ask(&measures, &by, filter);
        if self.chance(30) {
            ask.from = START + HOUR * self.int(0, 24);
            ask.to = ask.from + HOUR * self.int(1, 24);
        }
        ask
    }
}

/// Random kinds, rows, dims and changes to both, checked against the
/// reference every few steps. Without `chained` the fact keeps bound
/// cells, which answer until a dim changes under its rows; without
/// `changing` none does, so they answer throughout.
fn run(seed: u64, chained: bool, changing: bool) {
    let mut rng = Rng(seed);
    let kinds = rng.kinds();
    let filter = if rng.chance(50) {
        vec![cond("country", vec![lit("NO"), lit("SE")])]
    } else {
        Vec::new()
    };
    let spec = spec_with(kinds, filter, chained);
    let mut store = typed(&spec);
    let mut dims = Dims::new();
    let mut rows: Vec<Vec<Datum>> = Vec::new();
    for _ in 0..rng.below(30) {
        let (dim, key, attrs) = rng.dim_row(chained);
        add_dim(&mut store, &mut dims, dim, key, attrs);
    }
    for step in 0..400 {
        match rng.below(10) {
            0 | 1 if changing => {
                let (dim, key, attrs) = rng.dim_row(chained);
                add_dim(&mut store, &mut dims, dim, key, attrs);
            }
            2 if changing => {
                let (dim, key, attrs) = rng.dim_row(chained);
                let held = (dims.get(dim))
                    .and_then(|d| d.get(&key))
                    .is_some_and(|rows| rows.contains(&attrs));
                if held {
                    remove_dim(&mut store, &mut dims, dim, &key, &attrs);
                }
            }
            3 if !rows.is_empty() => {
                let row = rows.swap_remove(rng.below(rows.len()));
                store.remove_row("orders", &row).unwrap();
            }
            _ => {
                let row = rng.random_order();
                store.add_row("orders", &row).unwrap();
                rows.push(row);
            }
        }
        if step % 100 == 99 {
            // A ref through another dim keeps the fact unbound; without
            // one, the bound cells hold until a dim changes.
            let bound = store.bound_cells("orders").is_some();
            assert_eq!(bound, !chained && (!changing || step < 99), "seed {seed}");
            for _ in 0..4 {
                let ask = rng.random_ask();
                assert_eq!(
                    store.ask(&ask).unwrap(),
                    reference(&spec, &ask, &rows, &dims, &kinds),
                    "seed {seed}, step {step}, kinds {kinds:?}, {ask:?}"
                );
            }
        }
    }
}

#[test]
fn joins_match_the_naive_reference() {
    for seed in 0..8 {
        run(seed, true, true);
    }
}

#[test]
fn direct_joins_match_the_naive_reference() {
    for seed in 0..8 {
        run(seed, false, true);
    }
}

#[test]
fn bound_joins_match_the_naive_reference() {
    for seed in 0..8 {
        run(seed, false, false);
    }
}
