//! The reference: an ask answered straight from the live trips, joined to
//! the dims as they are now, with no cells, buckets or codes. The workload
//! compares every answer of the store against it.

use std::cmp::Ordering;
use std::collections::{BTreeMap, HashSet};

use crate::common::{
    Distinct, SPREAD_PLACES, bool_or_null, distinct, holds, in_range, int_or_null, mode,
    percentile_disc, spread,
};
use crate::views::trip_types;
use crate::{AT, DIST, DRO, Dims, FARE, HOME, PAY, PU, RIDER, SHARED, TIP, TOLLED, VENDOR, WIDTH};
use straum_domain::{
    Aggregate, Answer, AnswerColumn, AnswerRow, Ask, AskKey, Comparison, Condition, Datum, Fact,
    Identifier, Literal, Measure, RefKind, Spec, Value, Width,
};

/// Where a zone's borough and name lie among its attributes, and a combo's
/// or vendor's label among theirs.
const BOROUGH: usize = 0;
const ZONE: usize = 1;
const LABEL: usize = 0;

/// The rows of one group of an answer, under the values of its keys.
type Groups<'r> = BTreeMap<Vec<Datum>, Vec<&'r [Datum]>>;

/// A key's value for a row: a column, a null-ness key, or a dim value
/// joined as the dims are now, null when the join finds no row.
fn value(row: &[Datum], key: &Identifier, dims: &Dims) -> Datum {
    match key.as_str() {
        "known" => Datum::Bool(row[SHARED] != Datum::Null),
        "unpaid" => Datum::Bool(row[PAY] == Datum::Null),
        "home_borough" => joined(dims, "zones", &[row[HOME].clone()], BOROUGH),
        "pickup_borough" => joined(dims, "zones", &[row[PU].clone()], BOROUGH),
        "pickup_zone" => joined(dims, "zones", &[row[PU].clone()], ZONE),
        "dropoff_borough" => joined(dims, "zones", &[row[DRO].clone()], BOROUGH),
        "dropoff_zone" => joined(dims, "zones", &[row[DRO].clone()], ZONE),
        "combo_label" => {
            let key = [row[PU].clone(), row[VENDOR].clone()];
            joined(dims, "combos", &key, LABEL)
        }
        "vend_label" => joined(dims, "vendors", &[row[VENDOR].clone()], LABEL),
        name => row[column(name)].clone(),
    }
}

/// The attribute of the dim row under `key`, or null when the key has a
/// null or finds no row, as SQL's left join gives.
fn joined(dims: &Dims, dim: &str, key: &[Datum], attribute: usize) -> Datum {
    if key.contains(&Datum::Null) {
        return Datum::Null;
    }
    let Some(attributes) = dims[dim].get(key) else {
        return Datum::Null;
    };
    attributes[attribute].clone()
}

/// The scale a key's values print in: the dims' attributes are all text.
fn scale(key: &Identifier) -> u32 {
    match key.as_str() {
        "known" | "unpaid" | "pickup_borough" | "pickup_zone" | "dropoff_borough"
        | "dropoff_zone" | "combo_label" | "vend_label" | "home_borough" => 0,
        name => trip_types()[column(name)].scale(),
    }
}

/// Where a key, as an ask names it, sits in a row.
fn column(name: &str) -> usize {
    match name {
        "pay" => PAY,
        "vendor" => VENDOR,
        "shared" => SHARED,
        "pu" => PU,
        "dro" => DRO,
        "fare" => FARE,
        "tip" => TIP,
        "dist" => DIST,
        "rider" => RIDER,
        "tolled" => TOLLED,
        other => panic!("no column {other}"),
    }
}

/// Whether a row passes every condition of a filter.
fn passes(row: &[Datum], filter: &[Condition], dims: &Dims) -> bool {
    for condition in filter {
        if !holds(&value(row, &condition.key, dims), &condition.values) {
            return false;
        }
    }
    true
}

/// The reference answer: the ask over the raw rows joined to the dims, no
/// rollup, no buckets.
pub(crate) fn reference(spec: &Spec, ask: &Ask, rows: &[Vec<Datum>], dims: &Dims) -> Answer {
    let fact = spec.fact("trips").unwrap();
    let groups = groups(fact, ask, rows, dims);
    let columns = reference_columns(fact, ask);
    let mut answer_rows = Vec::with_capacity(groups.len());
    for (keys, rows) in groups {
        let mut values = Vec::with_capacity(ask.measures.len());
        for name in &ask.measures {
            values.push(measure(fact.measure(name.as_str()).unwrap(), &rows, dims));
        }
        let row = AnswerRow { keys, values };
        if meets_having(ask, &columns, &row) {
            answer_rows.push(row);
        }
    }
    Answer {
        columns,
        rows: answer_rows,
    }
}

/// Whether the row's measures meet every HAVING condition, by
/// cross-multiplying, which the small values here never overflow.
fn meets_having(ask: &Ask, columns: &[AnswerColumn], row: &AnswerRow) -> bool {
    for condition in &ask.having {
        let mut position = 0;
        while ask.measures[position] != condition.measure {
            position += 1;
        }
        let scale = columns[ask.keys.len() + position].scale;
        let Some(order) = order(&row.values[position], scale, &condition.value) else {
            return false;
        };
        let holds = match condition.comparison {
            Comparison::Equal => order.is_eq(),
            Comparison::NotEqual => order.is_ne(),
            Comparison::Less => order.is_lt(),
            Comparison::LessOrEqual => order.is_le(),
            Comparison::Greater => order.is_gt(),
            Comparison::GreaterOrEqual => order.is_ge(),
        };
        if !holds {
            return false;
        }
    }
    true
}

/// `value` at `scale` against `wanted`: `numerator / (count * 10^scale)`
/// against `units / 10^wanted_scale`, both sides brought to the finer
/// scale and multiplied out. None for null.
fn order(value: &Value, scale: u32, wanted: &Literal) -> Option<Ordering> {
    let (numerator, count) = match value {
        Value::Null => return None,
        Value::Bool(held) => {
            let Literal::Bool(wanted) = wanted else {
                panic!("a boolean measure is compared with a boolean")
            };
            return Some(held.cmp(wanted));
        }
        Value::Int(units) => (*units, 1),
        Value::Avg { sum, count } => (*sum, i128::from(*count)),
        Value::Wide(_) => unreachable!("the trips' values are far from the ends of bigint"),
    };
    let (units, wanted_scale) = match wanted {
        Literal::Int(integer) => (i128::from(*integer), 0),
        Literal::Decimal { units, scale } => (*units, *scale),
        other => panic!("a number measure is compared with a number, not {other:?}"),
    };
    let finer = scale.max(wanted_scale);
    let held = numerator * 10_i128.pow(finer - scale);
    if units == 0 {
        return Some(held.cmp(&0));
    }
    // A variance's scale can pass the 38 places an i128 holds a power of
    // ten for.
    let Some(asked) = units
        .checked_mul(count)
        .zip(10_i128.checked_pow(finer - wanted_scale))
        .and_then(|(product, shift)| product.checked_mul(shift))
    else {
        // Only an i64 extreme against a fine scale gets here: past any
        // held value, on its side of zero.
        return Some(0.cmp(&units));
    };
    Some(held.cmp(&asked))
}

/// The rows the ask reads, grouped by the values of its keys. Without
/// keys there is one group, empty or not, as SQL gives without `GROUP BY`.
// `'r`: the groups borrow the rows, which the workload keeps.
fn groups<'r>(fact: &Fact, ask: &Ask, rows: &'r [Vec<Datum>], dims: &Dims) -> Groups<'r> {
    let mut groups = Groups::new();
    if ask.keys.is_empty() {
        groups.insert(Vec::new(), Vec::new());
    }
    let vend_kind = join_kind(fact, "vend");
    for row in rows {
        let Datum::Int(time) = row[AT] else { continue };
        if !in_range(ask, time) || !passes(row, &ask.filter, dims) {
            continue;
        }
        if vend_kind == RefKind::Inner && !vendor_joins(row, dims) {
            continue;
        }
        let mut key = Vec::with_capacity(ask.keys.len());
        for asked in &ask.keys {
            key.push(match asked {
                AskKey::Key(name) if name.as_str() == "bucket" => {
                    Datum::Int(Width { seconds: WIDTH }.bucket_start(time).unwrap())
                }
                AskKey::Key(name) => value(row, name, dims),
                AskKey::WiderBucket { width, .. } => Datum::Int(width.bucket_start(time).unwrap()),
            });
        }
        groups.entry(key).or_default().push(row);
    }
    groups
}

/// The kind of the view's join named `alias`.
fn join_kind(fact: &Fact, alias: &str) -> RefKind {
    for reference in &fact.refs {
        if reference.alias.as_str() == alias {
            return reference.kind;
        }
    }
    panic!("no join {alias}")
}

/// Whether the trip's vendor finds a row: its key is not null and held.
fn vendor_joins(row: &[Datum], dims: &Dims) -> bool {
    let vendor = &row[VENDOR..=VENDOR];
    vendor[0] != Datum::Null && dims["vendors"].contains_key(vendor)
}

/// The keys, then the measures, named and scaled as the store names them.
fn reference_columns(fact: &Fact, ask: &Ask) -> Vec<AnswerColumn> {
    let mut columns = Vec::with_capacity(ask.keys.len() + ask.measures.len());
    for asked in &ask.keys {
        columns.push(match asked {
            AskKey::Key(name) if name.as_str() != "bucket" => AnswerColumn {
                name: name.to_string(),
                scale: scale(name),
                is_time: false,
            },
            AskKey::Key(_) | AskKey::WiderBucket { .. } => AnswerColumn {
                name: asked.to_string(),
                scale: 0,
                is_time: true,
            },
        });
    }
    for name in &ask.measures {
        columns.push(AnswerColumn {
            name: name.to_string(),
            scale: measure_scale(&fact.measure(name.as_str()).unwrap().aggregate),
            is_time: false,
        });
    }
    columns
}

/// A measure over numbers keeps its column's scale; a count has none.
pub(crate) fn measure_scale(aggregate: &Aggregate) -> u32 {
    match aggregate {
        Aggregate::Count
        | Aggregate::CountNotNull(_)
        | Aggregate::CountDistinct(_)
        | Aggregate::BoolAnd(_)
        | Aggregate::BoolOr(_) => 0,
        Aggregate::Sum(input)
        | Aggregate::Min(input)
        | Aggregate::Max(input)
        | Aggregate::Avg(input)
        | Aggregate::PercentileDisc { column: input, .. }
        | Aggregate::Mode(input)
        | Aggregate::SumDistinct(input)
        | Aggregate::AvgDistinct(input) => trip_types()[column(input.as_str())].scale(),
        // A variance is in the column's units squared.
        Aggregate::VarSamp(input) | Aggregate::VarPop(input) => {
            2 * trip_types()[column(input.as_str())].scale() + SPREAD_PLACES
        }
        Aggregate::StddevSamp(input) | Aggregate::StddevPop(input) => {
            trip_types()[column(input.as_str())].scale() + SPREAD_PLACES
        }
    }
}

/// The measure over the rows of one group that pass its own filter.
fn measure(measure: &Measure, rows: &[&[Datum]], dims: &Dims) -> Value {
    let mut kept = Vec::with_capacity(rows.len());
    for row in rows {
        if passes(row, &measure.filter, dims) {
            kept.push(*row);
        }
    }
    let rows = kept;
    match &measure.aggregate {
        Aggregate::Count => Value::Int(rows.len() as i128),
        Aggregate::CountNotNull(input) => Value::Int(not_null_in(&rows, input).len() as i128),
        Aggregate::Sum(input) => {
            let values = ints_in(&rows, input);
            if values.is_empty() {
                return Value::Null;
            }
            Value::Int(values.iter().sum())
        }
        Aggregate::Avg(input) => {
            let values = ints_in(&rows, input);
            if values.is_empty() {
                return Value::Null;
            }
            Value::Avg {
                sum: values.iter().sum(),
                count: values.len() as u64,
            }
        }
        Aggregate::Min(input) => int_or_null(ints_in(&rows, input).into_iter().min()),
        Aggregate::Max(input) => int_or_null(ints_in(&rows, input).into_iter().max()),
        Aggregate::CountDistinct(input) => {
            let distinct: HashSet<&Datum> = not_null_in(&rows, input).into_iter().collect();
            Value::Int(distinct.len() as i128)
        }
        Aggregate::BoolAnd(input) => bool_or_null(bools_in(&rows, input).into_iter().min()),
        Aggregate::BoolOr(input) => bool_or_null(bools_in(&rows, input).into_iter().max()),
        Aggregate::PercentileDisc { fraction, column } => {
            percentile_disc(ints_in(&rows, column), *fraction)
        }
        Aggregate::Mode(input) => mode(ints_in(&rows, input)),
        Aggregate::SumDistinct(input) => distinct(&ints_in(&rows, input), Distinct::Sum),
        Aggregate::AvgDistinct(input) => distinct(&ints_in(&rows, input), Distinct::Avg),
        Aggregate::VarSamp(input)
        | Aggregate::VarPop(input)
        | Aggregate::StddevSamp(input)
        | Aggregate::StddevPop(input) => spread(&measure.aggregate, &ints_in(&rows, input)),
    }
}

/// The values of a column that are not null, one per row that has one.
// `'r`: the values borrow the rows.
fn not_null_in<'r>(rows: &[&'r [Datum]], input: &Identifier) -> Vec<&'r Datum> {
    let mut values = Vec::new();
    for row in rows {
        let value = &row[column(input.as_str())];
        if *value != Datum::Null {
            values.push(value);
        }
    }
    values
}

fn ints_in(rows: &[&[Datum]], input: &Identifier) -> Vec<i128> {
    let mut ints = Vec::new();
    for row in rows {
        if let Datum::Int(int) = row[column(input.as_str())] {
            ints.push(i128::from(int));
        }
    }
    ints
}

fn bools_in(rows: &[&[Datum]], input: &Identifier) -> Vec<bool> {
    let mut bools = Vec::new();
    for row in rows {
        if let Datum::Bool(flag) = row[column(input.as_str())] {
            bools.push(flag);
        }
    }
    bools
}
