//! The reference: an ask answered straight from the live trips, joined to
//! the dims as they are now, with no cells, buckets or codes. The workload
//! compares every answer of the store against it.

use std::collections::{BTreeMap, HashSet};

use crate::common::{holds, percentile_disc};
use crate::views::trip_types;
use crate::{AT, DIST, DRO, Dims, FARE, HOME, PAY, PU, RIDER, SHARED, TIP, TOLLED, VENDOR, WIDTH};
use straum_domain::{
    Aggregate, Answer, AnswerColumn, AnswerRow, Ask, AskKey, Condition, Datum, Fact, Identifier,
    Measure, RefKind, Spec, Value, Width,
};

/// A key's value for a row: a column, a null-ness key, or a dim value
/// joined as the dims are now, null when the join finds no row.
pub(crate) fn value(row: &[Datum], key: &Identifier, dims: &Dims) -> Datum {
    match key.as_str() {
        "known" => Datum::Bool(row[SHARED] != Datum::Null),
        "unpaid" => Datum::Bool(row[PAY] == Datum::Null),
        "home_borough" => joined(dims, "zones", &[row[HOME].clone()], 0),
        "pickup_borough" => joined(dims, "zones", &[row[PU].clone()], 0),
        "pickup_zone" => joined(dims, "zones", &[row[PU].clone()], 1),
        "dropoff_borough" => joined(dims, "zones", &[row[DRO].clone()], 0),
        "dropoff_zone" => joined(dims, "zones", &[row[DRO].clone()], 1),
        "combo_label" => joined(dims, "combos", &[row[PU].clone(), row[VENDOR].clone()], 0),
        "vend_label" => joined(dims, "vendors", &[row[VENDOR].clone()], 0),
        name => row[column(name)].clone(),
    }
}

pub(crate) fn joined(dims: &Dims, dim: &str, key: &[Datum], attribute: usize) -> Datum {
    if key.contains(&Datum::Null) {
        return Datum::Null;
    }
    let Some(attributes) = dims[dim].get(key) else {
        return Datum::Null;
    };
    attributes[attribute].clone()
}

/// The scale a key's values print in: the dims' attributes are all text.
pub(crate) fn scale(key: &Identifier) -> u32 {
    match key.as_str() {
        "known" | "unpaid" | "pickup_borough" | "pickup_zone" | "dropoff_borough"
        | "dropoff_zone" | "combo_label" | "vend_label" | "home_borough" => 0,
        name => trip_types()[column(name)].scale(),
    }
}

/// Where a key, as an ask names it, sits in a row.
pub(crate) fn column(name: &str) -> usize {
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

pub(crate) fn passes(row: &[Datum], filter: &[Condition], dims: &Dims) -> bool {
    filter
        .iter()
        .all(|condition| holds(&value(row, &condition.key, dims), &condition.values))
}

/// The reference answer: the ask over the raw rows joined to the dims: no rollup, no buckets.
pub(crate) fn reference(spec: &Spec, ask: &Ask, rows: &[Vec<Datum>], dims: &Dims) -> Answer {
    let fact = spec.fact("trips").unwrap();
    let mut groups: BTreeMap<Vec<Datum>, Vec<&[Datum]>> = BTreeMap::new();
    if ask.keys.is_empty() {
        groups.insert(Vec::new(), Vec::new());
    }
    let vend = (fact.refs.iter())
        .find(|fact_ref| fact_ref.alias.as_str() == "vend")
        .unwrap();
    for row in rows {
        let Datum::Int(time) = row[AT] else { continue };
        if time < ask.from || time >= ask.to || !passes(row, &ask.filter, dims) {
            continue;
        }
        let vendor = &row[VENDOR..=VENDOR];
        let joins = vendor[0] != Datum::Null && dims["vendors"].contains_key(vendor);
        if vend.kind == RefKind::Inner && !joins {
            continue;
        }
        let key = ask
            .keys
            .iter()
            .map(|asked| match asked {
                AskKey::Key(name) => value(row, name, dims),
                AskKey::Bucket => Datum::Int(Width { seconds: WIDTH }.bucket_start(time)),
            })
            .collect();
        groups.entry(key).or_default().push(row);
    }
    let rows = groups
        .into_iter()
        .map(|(keys, rows)| {
            let values = ask
                .measures
                .iter()
                .map(|name| measure(fact.measure(name.as_str()).unwrap(), &rows, dims))
                .collect();
            AnswerRow { keys, values }
        })
        .collect();
    Answer {
        columns: reference_columns(fact, ask),
        rows,
    }
}

pub(crate) fn reference_columns(fact: &Fact, ask: &Ask) -> Vec<AnswerColumn> {
    let keys = ask.keys.iter().map(|asked| match asked {
        AskKey::Key(name) => AnswerColumn {
            name: name.to_string(),
            scale: scale(name),
            is_time: false,
        },
        AskKey::Bucket => AnswerColumn {
            name: asked.to_string(),
            scale: 0,
            is_time: true,
        },
    });
    let measures = ask.measures.iter().map(|name| AnswerColumn {
        name: name.to_string(),
        scale: measure_scale(&fact.measure(name.as_str()).unwrap().aggregate),
        is_time: false,
    });
    keys.chain(measures).collect()
}

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
        | Aggregate::PercentileDisc { column: input, .. } => {
            trip_types()[column(input.as_str())].scale()
        }
    }
}

pub(crate) fn measure(measure: &Measure, rows: &[&[Datum]], dims: &Dims) -> Value {
    let rows: Vec<&[Datum]> = (rows.iter().copied())
        .filter(|row| passes(row, &measure.filter, dims))
        .collect();
    match &measure.aggregate {
        Aggregate::Count => Value::Int(rows.len() as i128),
        Aggregate::CountNotNull(input) => {
            let values = rows
                .iter()
                .filter(|row| row[column(input.as_str())] != Datum::Null);
            Value::Int(values.count() as i128)
        }
        Aggregate::Sum(input) => {
            let values = ints_in(&rows, input);
            int_or_null((!values.is_empty()).then(|| values.iter().sum()))
        }
        Aggregate::Avg(input) => {
            let values = ints_in(&rows, input);
            if values.is_empty() {
                Value::Null
            } else {
                Value::Avg {
                    sum: values.iter().sum(),
                    count: values.len() as u64,
                }
            }
        }
        Aggregate::Min(input) => int_or_null(ints_in(&rows, input).into_iter().min()),
        Aggregate::Max(input) => int_or_null(ints_in(&rows, input).into_iter().max()),
        Aggregate::CountDistinct(input) => {
            let seen: HashSet<_> = rows
                .iter()
                .map(|row| &row[column(input.as_str())])
                .filter(|datum| **datum != Datum::Null)
                .collect();
            Value::Int(seen.len() as i128)
        }
        Aggregate::BoolAnd(input) => bool_or_null(bools_in(&rows, input).into_iter().min()),
        Aggregate::BoolOr(input) => bool_or_null(bools_in(&rows, input).into_iter().max()),
        Aggregate::PercentileDisc { fraction, column } => {
            percentile_disc(ints_in(&rows, column), *fraction)
        }
    }
}

pub(crate) fn ints_in(rows: &[&[Datum]], input: &Identifier) -> Vec<i128> {
    let mut ints = Vec::new();
    for row in rows {
        if let Datum::Int(int) = row[column(input.as_str())] {
            ints.push(i128::from(int));
        }
    }
    ints
}

pub(crate) fn bools_in(rows: &[&[Datum]], input: &Identifier) -> Vec<bool> {
    let mut bools = Vec::new();
    for row in rows {
        if let Datum::Bool(flag) = row[column(input.as_str())] {
            bools.push(flag);
        }
    }
    bools
}

pub(crate) fn int_or_null(found: Option<i128>) -> Value {
    found.map_or(Value::Null, Value::Int)
}

pub(crate) fn bool_or_null(found: Option<bool>) -> Value {
    found.map_or(Value::Null, Value::Bool)
}
