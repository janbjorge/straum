//! An ask as JSON, read field by field. Fields weir doesn't know are
//! errors, never ignored.
//!
//! ```json
//! {
//!   "fact": "trips",
//!   "measures": ["trips", "fare"],
//!   "by": ["payment_type", "pickup.borough"],
//!   "time": "day",
//!   "filter": {"pickup.borough": ["Manhattan", "Queens"], "vendor_id": 2},
//!   "zone": "Europe/Oslo",
//!   "from": "2024-01-01T00:00:00+01:00",
//!   "to": 1706742000
//! }
//! ```
//!
//! - `fact`, `measures`, `from` and `to` are required.
//! - `by`: by columns, refs and ref attributes such as `pickup.borough`.
//! - `time`: one of `bucket`, `hour`, `day`, `week` and `month`, grouped by
//!   before `by`. A field of its own, so it can't clash with a column.
//! - `filter`: each key's value, or any of a list of values.
//! - `zone`: a tz database name, `UTC` when left out.
//! - `from` and `to`: the half-open range, as RFC 3339 or Unix seconds.

use std::str::FromStr;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use weir_domain::{Ask, AskKey, Change, Cond, Ident, KeyPath, Literal, TimeKey, Zone};

use crate::json::Json;

const DAY: i64 = 86_400;

/// How far before `from` the zone's changes are read. A month that holds
/// `from` starts at most 31 days before it.
const LOOK_BACK: i64 = 32 * DAY;

/// An ask and the zone it names, which prints its time keys.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub(crate) ask: Ask,
    pub(crate) zone: TimeZone,
}

/// Every error found, each naming its field.
pub(crate) fn read(body: &[u8]) -> Result<Request, Vec<String>> {
    let json = Json::parse(body).map_err(|e| vec![e])?;
    let Json::Object(fields) = json else {
        return Err(vec![format!("an ask is an object, not {}", json.kind())]);
    };
    let mut errors = Vec::new();
    let e = &mut errors;
    let (mut fact, mut measures, mut from, mut to) = (None, None, None, None);
    let (mut by, mut time, mut filter, mut zone) = (Vec::new(), None, Vec::new(), None);
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
            "zone" => zone = text("zone", value, e).and_then(|z| time_zone(z, e)),
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
            let zone = zone.unwrap_or(TimeZone::UTC);
            let by = (time.map(AskKey::Time).into_iter())
                .chain(by.into_iter().map(AskKey::Key))
                .collect();
            Ok(Request {
                ask: Ask {
                    fact,
                    measures,
                    by,
                    filter,
                    zone: offsets(&zone, from, to),
                    from,
                    to,
                },
                zone,
            })
        }
        _ => Err(errors),
    }
}

/// The zone's UTC offsets from [`LOOK_BACK`] before `from` up to `to`.
/// Transitions that keep the offset, such as a renamed abbreviation, are
/// left out.
fn offsets(zone: &TimeZone, from: i64, to: i64) -> Zone {
    let start = Timestamp::from_second(from.saturating_sub(LOOK_BACK)).unwrap_or(Timestamp::MIN);
    let initial = i64::from(zone.to_offset(start).seconds());
    let mut changes: Vec<Change> = Vec::new();
    for transition in zone.following(start) {
        let at = transition.timestamp().as_second();
        if at >= to {
            break;
        }
        let offset = i64::from(transition.offset().seconds());
        if offset != changes.last().map_or(initial, |c| c.offset) {
            changes.push(Change { at, offset });
        }
    }
    Zone::new(initial, changes).expect("transitions come in order")
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
        "hour" => TimeKey::Hour,
        "day" => TimeKey::Day,
        "week" => TimeKey::Week,
        "month" => TimeKey::Month,
        other => {
            errors.push(format!(
                "\"time\": {other:?} is not bucket, hour, day, week or month"
            ));
            return None;
        }
    };
    Some(key)
}

fn time_zone(s: &str, errors: &mut Vec<String>) -> Option<TimeZone> {
    let zone = TimeZone::get(s).ok();
    if zone.is_none() {
        errors.push(format!(
            "\"zone\": {s:?} is not in the tz database, such as \"Europe/Oslo\""
        ));
    }
    zone
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
    // A period's start may lie a month before `from`, and must still be a
    // time jiff can write.
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
    /// Europe/Oslo's 2024 summer time starts.
    const SUMMER_2024: i64 = 1_711_846_800;

    fn id(s: &str) -> Ident {
        Ident::new(s).unwrap()
    }

    fn read_str(body: &str) -> Result<Request, Vec<String>> {
        read(body.as_bytes())
    }

    #[test]
    fn reads_every_field() {
        let request = read_str(
            r#"{
                "fact": "trips",
                "measures": ["trips", "fare"],
                "by": ["kind", "pickup", "pickup.borough"],
                "time": "day",
                "filter": {"pickup.borough": ["Manhattan", "Queens"], "kind": 2, "shared": false},
                "zone": "Europe/Oslo",
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
            request.ask,
            Ask {
                fact: id("trips"),
                measures: vec![id("trips"), id("fare")],
                by: vec![
                    AskKey::Time(TimeKey::Day),
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
                // From 2023-11-30, a month before `from`.
                zone: Zone::new(
                    3_600,
                    vec![Change {
                        at: SUMMER_2024,
                        offset: 7_200
                    }]
                )
                .unwrap(),
                from: JAN,
                to: APR,
            }
        );
        assert_eq!(request.zone.iana_name(), Some("Europe/Oslo"));
    }

    #[test]
    fn leaves_out_what_is_optional() {
        let request =
            read_str(r#"{"fact": "trips", "measures": [], "from": 0, "to": 86400}"#).unwrap();
        assert_eq!(
            request.ask,
            Ask {
                fact: id("trips"),
                measures: Vec::new(),
                by: Vec::new(),
                filter: Vec::new(),
                zone: Zone::utc(),
                from: 0,
                to: 86_400,
            }
        );
        assert_eq!(request.zone, TimeZone::UTC);
    }

    #[test]
    fn zone_offsets_cover_the_range() {
        let oslo = TimeZone::get("Europe/Oslo").unwrap();
        // Before the range, from a month before `from`, up to `to`.
        let summer = Change {
            at: SUMMER_2024,
            offset: 7_200,
        };
        assert_eq!(
            offsets(&oslo, APR, APR + DAY),
            Zone::new(3_600, vec![summer]).unwrap()
        );
        assert_eq!(
            offsets(&oslo, JAN, SUMMER_2024),
            Zone::new(3_600, Vec::new()).unwrap()
        );
        assert_eq!(
            offsets(&oslo, JAN, SUMMER_2024 + 1),
            Zone::new(3_600, vec![summer]).unwrap()
        );
        assert_eq!(offsets(&TimeZone::UTC, JAN, APR), Zone::utc());
        // British Standard Time: +01:00 all year from 1968-10-27 to
        // 1971-10-31. The tz database has a transition at its start that
        // keeps the offset; it is left out.
        let london = TimeZone::get("Europe/London").unwrap();
        let october_1968 = Timestamp::from_second(-38_880_000).unwrap();
        let kept = london.following(october_1968).next().unwrap();
        assert_eq!(
            (kept.timestamp().as_second(), kept.offset().seconds()),
            (-37_242_000, 3_600)
        );
        let back = Change {
            at: 57_722_400,
            offset: 0,
        };
        assert_eq!(
            offsets(&london, -38_880_000 + LOOK_BACK, 60_000_000),
            Zone::new(3_600, vec![back]).unwrap()
        );
        // Ranges at the ends of time don't overflow.
        assert_eq!(offsets(&TimeZone::UTC, i64::MIN, i64::MIN + 1), Zone::utc());
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
                r#"{"fact": "t", "measures": [], "by": {}, "time": "year", "zone": "Mars/Base", "from": 0, "to": 1}"#,
                vec![
                    r#""by" must be an array, not an object"#,
                    r#""time": "year" is not bucket, hour, day, week or month"#,
                    r#""zone": "Mars/Base" is not in the tz database, such as "Europe/Oslo""#,
                ],
            ),
            (
                r#"{"fact": "t", "measures": [], "time": 1, "zone": null, "from": 0, "to": 1}"#,
                vec![
                    r#""time" must be a string, not an integer"#,
                    r#""zone" must be a string, not null"#,
                ],
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
