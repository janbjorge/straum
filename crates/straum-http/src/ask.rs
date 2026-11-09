//! An ask as JSON, read field by field. Fields straum doesn't know are
//! errors, never ignored.
//!
//! ```json
//! {
//!   "fact": "trips",
//!   "measures": ["trips", "fare"],
//!   "by": ["payment_type", "pickup.borough"],
//!   "time": "bucket",
//!   "filter": {"pickup.borough": ["Manhattan", "Queens"], "vendor_id": 2},
//!   "from": "2024-01-01T00:00:00+01:00",
//!   "to": 1706742000
//! }
//! ```
//!
//! - `fact`, `measures`, `from` and `to` are required.
//! - `by`: by columns, refs and ref attributes such as `pickup.borough`.
//! - `time`: `bucket`, grouped by before `by`. A field of its own, so it
//!   can't clash with a column.
//! - `filter`: each key's value, or any of a list of values.
//! - `from` and `to`: the half-open range, as RFC 3339 or Unix seconds.

use std::str::FromStr;

use jiff::Timestamp;
use straum_domain::{Ask, AskKey, Cond, Ident, KeyPath, Literal, TimeKey};

use crate::json::Json;

const DAY: i64 = 86_400;

/// How far `from` stays clear of the earliest time jiff can write, so a
/// time key that starts up to a month before it still prints.
const LOOK_BACK: i64 = 32 * DAY;

/// Every error found, each naming its field.
pub(crate) fn read(body: &[u8]) -> Result<Ask, Vec<String>> {
    let json = Json::parse(body).map_err(|e| vec![e])?;
    let Json::Object(fields) = json else {
        return Err(vec![format!("an ask is an object, not {}", json.kind())]);
    };
    let mut errors = Vec::new();
    let e = &mut errors;
    let (mut fact, mut measures, mut from, mut to) = (None, None, None, None);
    let (mut by, mut time, mut filter) = (Vec::new(), None, Vec::new());
    for (field, value) in &fields {
        match field.as_str() {
            "fact" => fact = ident("fact", value, e),
            "measures" => {
                let items = array("measures", value, e);
                measures = Some(each("measures", items, e, ident));
            }
            "by" => {
                let items = array("by", value, e);
                by = each("by", items, e, path);
            }
            "time" => time = text("time", value, e).and_then(|t| time_key(t, e)),
            "filter" => filter = conditions(value, e),
            "from" => from = instant("from", value, e),
            "to" => to = instant("to", value, e),
            other => e.push(format!("unknown field {other:?}")),
        }
    }
    for required in ["fact", "measures", "from", "to"] {
        if !fields.iter().any(|(field, _)| field == required) {
            e.push(format!("{required:?} is missing"));
        }
    }
    match (fact, measures, from, to) {
        (Some(fact), Some(measures), Some(from), Some(to)) if errors.is_empty() => {
            let by = (time.map(AskKey::Time).into_iter())
                .chain(by.into_iter().map(AskKey::Key))
                .collect();
            Ok(Ask {
                fact,
                measures,
                by,
                filter,
                from,
                to,
            })
        }
        _ => Err(errors),
    }
}

fn text<'a>(field: &str, value: &'a Json, errors: &mut Vec<String>) -> Option<&'a str> {
    match value {
        Json::Text(s) => Some(s),
        other => {
            errors.push(format!("{field:?} must be a string, not {}", other.kind()));
            None
        }
    }
}

fn array<'a>(field: &str, value: &'a Json, errors: &mut Vec<String>) -> &'a [Json] {
    match value {
        Json::Array(items) => items,
        other => {
            errors.push(format!("{field:?} must be an array, not {}", other.kind()));
            &[]
        }
    }
}

/// Reads each item, naming it `field[i]` in errors.
fn each<T>(
    field: &str,
    items: &[Json],
    errors: &mut Vec<String>,
    read: fn(&str, &Json, &mut Vec<String>) -> Option<T>,
) -> Vec<T> {
    (items.iter().enumerate())
        .filter_map(|(i, item)| read(&format!("{field}[{i}]"), item, errors))
        .collect()
}

fn ident(field: &str, value: &Json, errors: &mut Vec<String>) -> Option<Ident> {
    let s = text(field, value, errors)?;
    let ident = Ident::new(s);
    if ident.is_none() {
        errors.push(format!(
            "{field:?}: {s:?} is not a name: lowercase letters, digits and _, at most 63 bytes"
        ));
    }
    ident
}

/// `column`, `ref` or `ref.attr`.
fn path(field: &str, value: &Json, errors: &mut Vec<String>) -> Option<KeyPath> {
    let s = text(field, value, errors)?;
    let path = key_path(s);
    if path.is_none() {
        errors.push(format!("{field:?}: {s:?} is not a column, ref or ref.attr"));
    }
    path
}

fn key_path(s: &str) -> Option<KeyPath> {
    Some(match s.split_once('.') {
        Some((head, attr)) => KeyPath {
            head: Ident::new(head)?,
            attr: Some(Ident::new(attr)?),
        },
        None => KeyPath {
            head: Ident::new(s)?,
            attr: None,
        },
    })
}

fn time_key(s: &str, errors: &mut Vec<String>) -> Option<TimeKey> {
    let key = match s {
        "bucket" => TimeKey::Bucket,
        other => {
            errors.push(format!("\"time\": {other:?} is not bucket"));
            return None;
        }
    };
    Some(key)
}

/// Unix seconds, from RFC 3339 or an integer, in whole seconds.
fn instant(field: &str, value: &Json, errors: &mut Vec<String>) -> Option<i64> {
    let at = match value {
        Json::Int(seconds) => Timestamp::from_second(*seconds).map_err(|e| e.to_string()),
        Json::Text(s) => Timestamp::from_str(s).map_err(|e| e.to_string()),
        other => Err(format!(
            "must be RFC 3339 or Unix seconds, not {}",
            other.kind()
        )),
    };
    let earliest = Timestamp::MIN.as_second() + LOOK_BACK;
    let at = at.and_then(|at| match at.subsec_nanosecond() {
        0 if at.as_second() < earliest => Err(format!("{at} is too early")),
        0 => Ok(at.as_second()),
        _ => Err(format!("{at} is not a whole second")),
    });
    at.map_err(|e| errors.push(format!("{field:?}: {e}"))).ok()
}

fn conditions(value: &Json, errors: &mut Vec<String>) -> Vec<Cond> {
    let Json::Object(fields) = value else {
        errors.push(format!(
            "\"filter\" must be an object, not {}",
            value.kind()
        ));
        return Vec::new();
    };
    let mut conds = Vec::new();
    for (key, value) in fields {
        let field = format!("filter.{key}");
        let path = key_path(key);
        if path.is_none() {
            errors.push(format!(
                "\"filter\": {key:?} is not a column, ref or ref.attr"
            ));
        }
        let values = match value {
            Json::Array(items) if items.is_empty() => {
                errors.push(format!("{field:?}: an empty list matches nothing"));
                Vec::new()
            }
            Json::Array(items) => each(&field, items, errors, literal),
            single => literal(&field, single, errors).into_iter().collect(),
        };
        if let Some(path) = path {
            conds.push(Cond { path, values });
        }
    }
    conds
}

fn literal(field: &str, value: &Json, errors: &mut Vec<String>) -> Option<Literal> {
    let literal = match value {
        Json::Int(n) => Literal::Int(*n),
        Json::Text(s) => Literal::Text(s.clone()),
        Json::Bool(b) => Literal::Bool(*b),
        // `= NULL` matches nothing in SQL, so it can only be a mistake.
        other => {
            errors.push(format!(
                "{field:?} must be an integer, a string or a boolean, not {}",
                other.kind()
            ));
            return None;
        }
    };
    Some(literal)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2024-01-01 00:00 and 2024-04-01 00:00 UTC.
    const JAN: i64 = 1_704_067_200;
    const APR: i64 = 1_711_929_600;

    fn id(s: &str) -> Ident {
        Ident::new(s).unwrap()
    }

    fn read_str(body: &str) -> Result<Ask, Vec<String>> {
        read(body.as_bytes())
    }

    #[test]
    fn reads_every_field() {
        let request = read_str(
            r#"{
                "fact": "trips",
                "measures": ["trips", "fare"],
                "by": ["kind", "pickup", "pickup.borough"],
                "time": "bucket",
                "filter": {"pickup.borough": ["Manhattan", "Queens"], "kind": 2, "shared": false},
                "from": "2024-01-01T01:00:00+01:00",
                "to": 1711929600
            }"#,
        )
        .unwrap();
        let path = |head, attr: Option<&str>| KeyPath {
            head: id(head),
            attr: attr.map(id),
        };
        assert_eq!(
            request,
            Ask {
                fact: id("trips"),
                measures: vec![id("trips"), id("fare")],
                by: vec![
                    AskKey::Time(TimeKey::Bucket),
                    AskKey::Key(path("kind", None)),
                    AskKey::Key(path("pickup", None)),
                    AskKey::Key(path("pickup", Some("borough"))),
                ],
                filter: vec![
                    Cond {
                        path: path("pickup", Some("borough")),
                        values: vec![
                            Literal::Text("Manhattan".into()),
                            Literal::Text("Queens".into())
                        ],
                    },
                    Cond {
                        path: path("kind", None),
                        values: vec![Literal::Int(2)],
                    },
                    Cond {
                        path: path("shared", None),
                        values: vec![Literal::Bool(false)],
                    },
                ],
                from: JAN,
                to: APR,
            }
        );
    }

    #[test]
    fn leaves_out_what_is_optional() {
        let request =
            read_str(r#"{"fact": "trips", "measures": [], "from": 0, "to": 86400}"#).unwrap();
        assert_eq!(
            request,
            Ask {
                fact: id("trips"),
                measures: Vec::new(),
                by: Vec::new(),
                filter: Vec::new(),
                from: 0,
                to: 86_400,
            }
        );
    }

    #[test]
    fn every_error_names_its_field() {
        let cases = [
            ("[]", vec!["an ask is an object, not an array"]),
            ("{", vec!["EOF while parsing an object at line 1 column 1"]),
            (
                "{}",
                vec![
                    r#""fact" is missing"#,
                    r#""measures" is missing"#,
                    r#""from" is missing"#,
                    r#""to" is missing"#,
                ],
            ),
            (
                r#"{"fact": "Trips", "measures": "trips", "from": 0, "to": 1, "limit": 3}"#,
                vec![
                    r#""fact": "Trips" is not a name: lowercase letters, digits and _, at most 63 bytes"#,
                    r#""measures" must be an array, not a string"#,
                    r#"unknown field "limit""#,
                ],
            ),
            (
                r#"{"fact": 1, "measures": ["a", 2, "B"], "by": ["a.b.c", "x", 3, ".a"], "from": 0, "to": 1}"#,
                vec![
                    r#""fact" must be a string, not an integer"#,
                    r#""measures[1]" must be a string, not an integer"#,
                    r#""measures[2]": "B" is not a name: lowercase letters, digits and _, at most 63 bytes"#,
                    r#""by[0]": "a.b.c" is not a column, ref or ref.attr"#,
                    r#""by[2]" must be a string, not an integer"#,
                    r#""by[3]": ".a" is not a column, ref or ref.attr"#,
                ],
            ),
            (
                r#"{"fact": "t", "measures": [], "by": {}, "time": "day", "zone": "UTC", "from": 0, "to": 1}"#,
                vec![
                    r#""by" must be an array, not an object"#,
                    r#""time": "day" is not bucket"#,
                    r#"unknown field "zone""#,
                ],
            ),
            (
                r#"{"fact": "t", "measures": [], "time": 1, "from": 0, "to": 1}"#,
                vec![r#""time" must be a string, not an integer"#],
            ),
            (
                r#"{"fact": "t", "measures": [], "from": "2024-01-01T00:00:00.5Z", "to": "2024-01-01"}"#,
                vec![
                    r#""from": 2024-01-01T00:00:00.5Z is not a whole second"#,
                    r#""to": failed to find time component"#,
                ],
            ),
            (
                r#"{"fact": "t", "measures": [], "from": 1.5, "to": 99999999999999}"#,
                vec![
                    r#""from": must be RFC 3339 or Unix seconds, not a number that isn't an integer"#,
                    r#""to": parameter 'Unix timestamp seconds' is not in the required range of -377705023201..=253402207200"#,
                ],
            ),
            (
                r#"{"fact": "t", "measures": [], "from": -377705023201, "to": "-009999-02-02T00:00:00Z"}"#,
                vec![
                    r#""from": -009999-01-02T01:59:59Z is too early"#,
                    r#""to": -009999-02-02T00:00:00Z is too early"#,
                ],
            ),
            (
                r#"{"fact": "t", "measures": [], "filter": [], "from": 0, "to": 1}"#,
                vec![r#""filter" must be an object, not an array"#],
            ),
            (
                r#"{"fact": "t", "measures": [], "from": 0, "to": 1,
                    "filter": {"A": 1, "b": [], "c": null, "d": [1, 2.5, {}], "e": {}}}"#,
                vec![
                    r#""filter": "A" is not a column, ref or ref.attr"#,
                    r#""filter.b": an empty list matches nothing"#,
                    r#""filter.c" must be an integer, a string or a boolean, not null"#,
                    r#""filter.d[1]" must be an integer, a string or a boolean, not a number that isn't an integer"#,
                    r#""filter.d[2]" must be an integer, a string or a boolean, not an object"#,
                    r#""filter.e" must be an integer, a string or a boolean, not an object"#,
                ],
            ),
        ];
        for (body, expected) in cases {
            let errors = read_str(body).unwrap_err();
            assert_eq!(errors.len(), expected.len(), "{body}: {errors:?}");
            for (got, want) in errors.iter().zip(&expected) {
                assert!(got.starts_with(want), "{body}: {got:?} is not {want:?}");
            }
        }
    }
}
