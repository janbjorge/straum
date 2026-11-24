//! The orders view the join tests share: orders by customer and product,
//! joined to five dims, one join of each kind. Here too the store over
//! it, the reference's copy of the dims, and the helpers that change both
//! in step, so the reference always joins what the store holds.

use std::collections::BTreeMap;

use crate::common::{
    attribute, by_key, condition, dim, dim_row, id, lit, measure, names, percentile, table,
};
use straum_domain::{
    Aggregate, Ask, AskKey, Attribute, ColumnType, Condition, Datum, Fact, Measure, Ref, RefIndex,
    RefKind, Spec, Store, Via, Width,
};

pub const HOUR: i64 = 3_600;
/// The view's bucket width, [`HOUR`] seconds.
pub const HOUR_WIDTH: Width = Width { seconds: 3_600 };
/// 2024-01-01 00:00 UTC.
pub const START: i64 = 1_704_067_200;
pub const END: i64 = START + 48 * HOUR;

// Row positions of `orders`.
pub const AT: usize = 0;
pub const CUSTOMER: usize = 1;
pub const PRODUCT: usize = 2;
pub const QUANTITY: usize = 3;
pub const EVEN: usize = 4;

// The refs, in join order.
pub const CUSTOMERS: usize = 0;
pub const TIERS: usize = 1;
pub const TAGS: usize = 2;
pub const PROMOS: usize = 3;
pub const BLOCKED: usize = 4;

/// The dims, in join order.
pub const DIM_NAMES: [&str; 5] = ["customers", "tiers", "tags", "promos", "blocked"];

/// One join kind per ref, in join order.
pub type Kinds = [RefKind; 5];

pub const KEYS: [&str; 6] = ["customer", "product", "country", "label", "tag", "bucket"];
pub const MEASURES: [&str; 14] = [
    "n",
    "q",
    "q_max",
    "q_min",
    "q_median",
    "products",
    "q_n",
    "all_even",
    "any_even",
    "q_var",
    "q_sd_pop",
    "q_mode",
    "q_distinct",
    "q_distinct_avg",
];
/// A measure with a filter of its own, over a column a join reads.
pub const FILTERED: &str = "q_no";

/// Left, left, inner, semi, anti: one of each kind, and the chain.
pub const KINDS: Kinds = [
    RefKind::Left,
    RefKind::Left,
    RefKind::Inner,
    RefKind::Semi,
    RefKind::Anti,
];

/// The dims as the projections key them: dim name to key to its rows'
/// attributes, several rows a key allowed.
pub type Dims = BTreeMap<&'static str, BTreeMap<Vec<Datum>, Vec<Vec<Datum>>>>;

/// How the orders are keyed and joined, which sets the cells an ask
/// reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shape {
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
/// is the view's `WHERE`. Unless the `shape` is chained, tiers are joined
/// on the customer id itself, so every ref joins on a fact column.
pub fn spec(kinds: Kinds, filter: Vec<Condition>, shape: Shape) -> Spec {
    let mut by_keys = Vec::new();
    for key in ["customer", "product"] {
        if keyed(shape, key) {
            by_keys.push(by_key(key));
        }
    }
    let orders = Fact {
        as_text: Vec::new(),
        name: id("orders"),
        table: table("orders"),
        time: straum_domain::Time {
            column: id("at"),
            width: HOUR_WIDTH,
            wider: vec![Width {
                seconds: 24 * HOUR_WIDTH.seconds,
            }],
            retained: Vec::new(),
            name: id("bucket"),
        },
        refs: refs(kinds, shape),
        by_keys,
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

/// Whether a view of `shape` has `key`: the product is one unless the
/// view is [`Shape::Bound`].
pub fn keyed(shape: Shape, key: &str) -> bool {
    shape != Shape::Bound || key != "product"
}

/// The refs in join order, each of its kind in `kinds`.
fn refs(kinds: Kinds, shape: Shape) -> Vec<Ref> {
    let tiers_via = if shape == Shape::Chained {
        Via::Attribute {
            through: RefIndex(CUSTOMERS),
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
            vec![attribute("country", "country")],
        ),
        join(
            kinds[TIERS],
            "tiers",
            tiers_via,
            vec![attribute("label", "label")],
        ),
        join(
            kinds[TAGS],
            "tags",
            Via::Fact(id("product")),
            vec![attribute("tag", "tag")],
        ),
        join(
            kinds[PROMOS],
            "promos",
            Via::Fact(id("product")),
            vec![attribute("note", "note")],
        ),
        join(
            kinds[BLOCKED],
            "blocked",
            Via::Fact(id("customer")),
            vec![attribute("reason", "reason")],
        ),
    ]
}

/// A semi or anti join reads nothing; the same ref as a left or inner
/// join reads the dim's one column.
fn join(kind: RefKind, name: &str, via: Via, attributes: Vec<Attribute>) -> Ref {
    Ref {
        alias: id(name),
        dim: id(name),
        kind,
        via: vec![via],
        reads: match kind {
            RefKind::Left | RefKind::Inner => attributes,
            RefKind::Semi | RefKind::Anti => vec![],
        },
    }
}

fn measures() -> Vec<Measure> {
    vec![
        measure("n", Aggregate::Count),
        measure("q", Aggregate::Sum(id("qty"))),
        measure("q_max", Aggregate::Max(id("qty"))),
        measure("q_min", Aggregate::Min(id("qty"))),
        measure("q_median", percentile(5, 1, "qty")),
        measure("q_mode", Aggregate::Mode(id("qty"))),
        measure("q_distinct", Aggregate::SumDistinct(id("qty"))),
        measure("q_distinct_avg", Aggregate::AvgDistinct(id("qty"))),
        measure("q_var", Aggregate::VarSamp(id("qty"))),
        measure("q_sd_pop", Aggregate::StddevPop(id("qty"))),
        measure("products", Aggregate::CountDistinct(id("product"))),
        measure("q_n", Aggregate::CountNotNull(id("qty"))),
        measure("all_even", Aggregate::BoolAnd(id("even"))),
        measure("any_even", Aggregate::BoolOr(id("even"))),
        Measure {
            filter: vec![condition("country", vec![lit("NO")])],
            ..measure(FILTERED, Aggregate::Sum(id("qty")))
        },
    ]
}

/// An empty store of `spec`, with the types a load would set.
pub fn typed(spec: &Spec) -> Store {
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
            for attributes in each {
                store.add_row(dim, &dim_row(key, attributes)).unwrap();
            }
        }
    }
}

/// An order row; `even` is whether `quantity` is, a boolean for the
/// `bool_and` and `bool_or`.
pub fn order(at: i64, customer: Datum, product: Datum, quantity: Datum) -> Vec<Datum> {
    let even = match quantity {
        Datum::Int(quantity) => Datum::Bool(quantity % 2 == 0),
        _ => Datum::Null,
    };
    vec![Datum::Int(at), customer, product, quantity, even]
}

/// An ask over the two days of `measures` by `by`, `"bucket"` naming
/// the time bucket.
pub fn ask(measures: &[&str], by: &[&str], filter: Vec<Condition>) -> Ask {
    let mut keys = Vec::with_capacity(by.len());
    for key in by {
        keys.push(AskKey::Key(id(key)));
    }
    Ask {
        view: id("orders"),
        measures: names(measures),
        keys,
        filter,
        having: Vec::new(),
        from: Some(START),
        to: Some(END),
        bounded: None,
    }
}

/// A store and the reference's view of it: the spec, and the dims it
/// holds.
pub struct Loaded {
    pub store: Store,
    pub spec: Spec,
    pub dims: Dims,
}

impl Loaded {
    /// Adds a dim row to the store and to the reference's dims.
    pub fn add_dim(&mut self, dim: &'static str, key: Vec<Datum>, attributes: Vec<Datum>) {
        self.store
            .add_row(dim, &dim_row(&key, &attributes))
            .unwrap();
        self.dims
            .entry(dim)
            .or_default()
            .entry(key)
            .or_default()
            .push(attributes);
    }

    /// Removes a dim row from the store and from the reference's dims.
    pub fn remove_dim(&mut self, dim: &'static str, key: &[Datum], attributes: &[Datum]) {
        self.store
            .remove_row(dim, &dim_row(key, attributes))
            .unwrap();
        let by_key = self.dims.get_mut(dim).unwrap();
        let rows = by_key.get_mut(key).unwrap();
        let mut position = 0;
        while rows[position] != attributes {
            position += 1;
        }
        rows.remove(position);
        if rows.is_empty() {
            by_key.remove(key);
        }
    }

    /// Replaces the one dim row under a key of one column.
    pub fn replace_dim(&mut self, replaced: Replaced) {
        let key = vec![replaced.key];
        self.remove_dim(replaced.dim, &key, &replaced.old);
        self.add_dim(replaced.dim, key, replaced.new);
    }

    /// Clears a dim in the store and in the reference.
    pub fn clear_dim(&mut self, dim: &'static str) {
        self.store.clear(dim);
        self.dims.remove(dim);
    }
}

/// One dim row under a key of one column, `old` before and `new` after.
pub struct Replaced {
    pub dim: &'static str,
    pub key: Datum,
    pub old: Vec<Datum>,
    pub new: Vec<Datum>,
}

/// A store of `spec` over `dims` and the orders given.
pub fn load(spec: Spec, dims: Dims, orders: &[Vec<Datum>]) -> Loaded {
    let mut store = typed(&spec);
    load_dims(&mut store, &dims);
    for row in orders {
        store.add_row("orders", row).unwrap();
    }
    Loaded { store, spec, dims }
}
