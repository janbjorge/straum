//! The reference the store is checked against: an ask answered from the
//! raw order rows and the dims as they are now, joined as PostgreSQL
//! would. Each order expands into one view row per way its joins resolve;
//! the rows are grouped by the ask's keys and every measure is worked out
//! from the group's rows directly.

use std::collections::{BTreeMap, HashSet};

use crate::common::{GroupInputs, SPREAD_PLACES, aggregate_value, bucket_key, holds, in_range};
use crate::view::{
    AT, BLOCKED, CUSTOMER, CUSTOMERS, Dims, EVEN, HOUR, Loaded, PRODUCT, QUANTITY, TAGS, TIERS,
};
use straum_domain::{
    Aggregate, Answer, AnswerColumn, AnswerRow, Ask, AskKey, Condition, Datum, Fact, Identifier,
    Measure, RefKind, Spec, Value, Via, Width,
};

/// One way an order's joins resolve: per ref, the dim row matched.
type JoinRow = Vec<Option<Vec<Datum>>>;

/// One row of the view: an order and one way its joins resolve.
struct ViewRow<'a> {
    // Borrowed: the reference reads every order once per check.
    order: &'a [Datum],
    joined: JoinRow,
}

/// Every way an order's joins resolve, each ref of the fact by its own
/// kind, in ref order, as PostgreSQL would repeat the row.
fn join_rows(row: &[Datum], dims: &Dims, fact: &Fact) -> Vec<JoinRow> {
    let chained = matches!(fact.refs[TIERS].via[0], Via::Attribute { .. });
    let mut rows: Vec<JoinRow> = vec![Vec::new()];
    for (index, reference) in fact.refs.iter().enumerate() {
        let mut next = Vec::new();
        for joined in &rows {
            let key = match index {
                TIERS if chained => joined_value(joined, CUSTOMERS, 1),
                CUSTOMERS | TIERS | BLOCKED => row[CUSTOMER].clone(),
                _ => row[PRODUCT].clone(),
            };
            let found = matching_rows(dims, reference.dim.as_str(), &key);
            // A left join keeps a row no dim row matches, once; a semi
            // join keeps a matched row once, an anti join an unmatched one.
            let kept_once = match reference.kind {
                RefKind::Left | RefKind::Anti => found.is_empty(),
                RefKind::Semi => !found.is_empty(),
                RefKind::Inner => false,
            };
            if kept_once {
                next.push(joined_with(joined, None));
            } else if matches!(reference.kind, RefKind::Left | RefKind::Inner) {
                for matched in found {
                    next.push(joined_with(joined, Some(matched)));
                }
            }
        }
        rows = next;
    }
    rows
}

/// The dim rows under `key`: none for a null key, which joins nothing.
fn matching_rows(dims: &Dims, dim: &str, key: &Datum) -> Vec<Vec<Datum>> {
    if *key == Datum::Null {
        return Vec::new();
    }
    let Some(by_key) = dims.get(dim) else {
        return Vec::new();
    };
    let Some(rows) = by_key.get(std::slice::from_ref(key)) else {
        return Vec::new();
    };
    rows.clone()
}

fn joined_with(joined: &JoinRow, matched: Option<Vec<Datum>>) -> JoinRow {
    let mut joined = joined.clone();
    joined.push(matched);
    joined
}

/// A column of the dim row a join matched, null if it matched none.
fn joined_value(joined: &JoinRow, join: usize, column: usize) -> Datum {
    match &joined[join] {
        Some(matched) => matched[column].clone(),
        None => Datum::Null,
    }
}

/// A key's value for one order under one way its joins resolved.
fn value(row: &[Datum], joined: &JoinRow, key: &Identifier) -> Datum {
    match key.as_str() {
        "customer" => row[CUSTOMER].clone(),
        "product" => row[PRODUCT].clone(),
        "country" => joined_value(joined, CUSTOMERS, 0),
        "label" => joined_value(joined, TIERS, 0),
        "tag" => joined_value(joined, TAGS, 0),
        other => panic!("no key {other}"),
    }
}

/// Whether one view row passes every condition of `filter`.
fn passes(row: &[Datum], joined: &JoinRow, filter: &[Condition]) -> bool {
    for condition in filter {
        if !holds(&value(row, joined, &condition.key), &condition.values) {
            return false;
        }
    }
    true
}

/// The values of the ask's keys for one view row.
fn group_key(ask: &Ask, row: &[Datum], joined: &JoinRow, at: i64) -> Vec<Datum> {
    let mut key = Vec::with_capacity(ask.keys.len());
    for ask_key in &ask.keys {
        let hour = Width {
            seconds: u32::try_from(HOUR).unwrap(),
        };
        key.push(match ask_key {
            AskKey::Key(name) if name.as_str() != "bucket" => value(row, joined, name),
            bucket => bucket_key(bucket, hour, at),
        });
    }
    key
}

/// The reference answer: the ask over the raw rows joined as the dims
/// are: every way each row's joins resolve is one row of the view, as it
/// would be in PostgreSQL.
pub fn reference(spec: &Spec, ask: &Ask, rows: &[Vec<Datum>], dims: &Dims) -> Answer {
    let fact = spec.fact("orders").unwrap();
    let mut groups: BTreeMap<Vec<Datum>, Vec<ViewRow<'_>>> = BTreeMap::new();
    if ask.keys.is_empty() {
        groups.insert(Vec::new(), Vec::new());
    }
    for row in rows {
        let Datum::Int(at) = row[AT] else { continue };
        if !in_range(ask, at) {
            continue;
        }
        for joined in join_rows(row, dims, fact) {
            if !passes(row, &joined, &fact.filter) || !passes(row, &joined, &ask.filter) {
                continue;
            }
            let key = group_key(ask, row, &joined, at);
            groups
                .entry(key)
                .or_default()
                .push(ViewRow { order: row, joined });
        }
    }
    let mut answer_rows = Vec::with_capacity(groups.len());
    for (keys, group) in groups {
        let mut values = Vec::with_capacity(ask.measures.len());
        for name in &ask.measures {
            values.push(reference_value(
                fact.measure(name.as_str()).unwrap(),
                &group,
            ));
        }
        answer_rows.push(AnswerRow { keys, values });
    }
    Answer {
        columns: answer_columns(ask),
        rows: answer_rows,
    }
}

/// The ask's keys, then its measures, over unscaled columns: a variance
/// or deviation keeps its places past them.
fn answer_columns(ask: &Ask) -> Vec<AnswerColumn> {
    let mut columns = Vec::with_capacity(ask.keys.len() + ask.measures.len());
    for key in &ask.keys {
        columns.push(AnswerColumn {
            name: key.to_string(),
            scale: 0,
            is_time: key.name().as_str() == "bucket",
        });
    }
    for name in &ask.measures {
        let scale = match name.as_str() {
            "q_var" | "q_sd_pop" => SPREAD_PLACES,
            _ => 0,
        };
        columns.push(AnswerColumn {
            name: name.to_string(),
            scale,
            is_time: false,
        });
    }
    columns
}

/// One measure over the view rows of a group, as SQL works it out.
fn reference_value(measure: &Measure, group: &[ViewRow<'_>]) -> Value {
    let mut rows = Vec::new();
    for row in group {
        if passes(row.order, &row.joined, &measure.filter) {
            rows.push(row.order);
        }
    }
    if let Aggregate::Avg(_) = measure.aggregate {
        unreachable!("no average here");
    }
    // The view's one count of distinct values is of the products; every
    // other measure reads the quantity, or whether it is even.
    let quantities = quantities(&rows);
    let mut products = HashSet::new();
    for row in &rows {
        if row[PRODUCT] != Datum::Null {
            products.insert(&row[PRODUCT]);
        }
    }
    let group = GroupInputs {
        rows: rows.len(),
        not_null: quantities.len(),
        ints: quantities,
        bools: evens(&rows),
        distinct: products.len(),
    };
    aggregate_value(&measure.aggregate, group)
}

/// The quantities of the rows that have one.
fn quantities(rows: &[&[Datum]]) -> Vec<i128> {
    let mut quantities = Vec::new();
    for row in rows {
        if let Datum::Int(quantity) = row[QUANTITY] {
            quantities.push(i128::from(quantity));
        }
    }
    quantities
}

/// Whether each row's quantity is even, for the rows that have one.
fn evens(rows: &[&[Datum]]) -> Vec<bool> {
    let mut evens = Vec::new();
    for row in rows {
        if let Datum::Bool(even) = row[EVEN] {
            evens.push(even);
        }
    }
    evens
}

/// The store's answer to `ask` is the reference's over `rows`.
pub fn check(loaded: &Loaded, rows: &[Vec<Datum>], ask: &Ask) {
    assert_eq!(
        loaded.store.ask(ask).unwrap(),
        reference(&loaded.spec, ask, rows, &loaded.dims),
        "{ask:?}"
    );
}
