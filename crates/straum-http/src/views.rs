//! `/views`: every view asks may read and how much of it Straum holds, in
//! plain text for people, as `straum views` prints it. A program that
//! sends `Accept: application/json`, as `straum shell` does to complete
//! names, gets each view's names and the range of buckets it holds:
//!
//! ```json
//! {"views": [{"name": "trips", "from": "2024-01-01T00:00:00Z",
//!             "to": "2024-01-03T00:00:00Z",
//!             "keys": [{"name": "bucket", "type": "timestamptz"},
//!                      {"name": "fare", "type": "numeric", "scale": 2}],
//!             "measures": ["trips"]}]}
//! ```
//!
//! `from` and `to` are null for a view with no bucket. A type is
//! `integer`, `numeric` with its `scale`, `boolean`, `text`,
//! `timestamptz` or `timestamp`.

use straum_app::{Held, HeldDim, HeldView};
use straum_domain::ColumnType;

use crate::json::quoted;
use crate::time::rfc3339;

pub(crate) fn write(held: &Held) -> String {
    let views = held.views.iter().map(view);
    let dims = held.dims.iter().map(dim);
    views.chain(dims).collect::<Vec<_>>().join("\n")
}

pub(crate) fn json(held: &Held) -> Vec<u8> {
    let views: Vec<String> = held.views.iter().map(view_json).collect();
    format!("{{\"views\":[{}]}}", views.join(",")).into_bytes()
}

fn view_json(view: &HeldView) -> String {
    let edge = |seconds: Option<i64>| {
        (seconds.and_then(rfc3339)).map_or("null".to_owned(), |at| quoted(&at))
    };
    let key = |name: &str, column: ColumnType| {
        let scale = match column {
            ColumnType::Numeric { scale } => format!(",\"scale\":{scale}"),
            _ => String::new(),
        };
        format!(
            "{{\"name\":{},\"type\":{}{scale}}}",
            quoted(name),
            quoted(type_name(column))
        )
    };
    let keys = (view.keys.iter()).map(|k| key(k.name.as_str(), k.column_type));
    let keys: Vec<String> = std::iter::once(key("bucket", view.bucket))
        .chain(keys)
        .collect();
    let measures: Vec<String> = view.measures.iter().map(|m| quoted(m.as_str())).collect();
    format!(
        "{{\"name\":{},\"from\":{},\"to\":{},\"keys\":[{}],\"measures\":[{}]}}",
        quoted(view.name.as_str()),
        edge(view.range.as_ref().map(|range| range.start)),
        edge(view.range.as_ref().map(|range| range.end)),
        keys.join(","),
        measures.join(",")
    )
}

fn view(view: &HeldView) -> String {
    let widths: Vec<String> = view.widths.iter().map(ToString::to_string).collect();
    let mut holds = counted(view.cells, "cell");
    if let Some(bound) = view.bound_cells {
        holds.push_str(&format!(" and {}", counted(bound, "bound cell")));
    }
    if let Some(range) = &view.range {
        // The end is the first second past the last bucket, which is what
        // an ask's `bucket < $n` takes.
        holds.push_str(&format!(", {} to {}", time(range.start), time(range.end)));
    }
    let bucket = format!("bucket {}", column_type(view.bucket));
    let keys =
        (view.keys.iter()).map(|key| format!("{} {}", key.name, column_type(key.column_type)));
    let keys: Vec<String> = std::iter::once(bucket).chain(keys).collect();
    let measures: Vec<&str> = view.measures.iter().map(|m| m.as_str()).collect();
    format!(
        "view {}\n  buckets:  {}\n  holds:    {holds}\n  keys:     {}\n  measures: {}\n",
        view.name,
        widths.join(", "),
        keys.join(", "),
        measures.join(", ")
    )
}

fn dim(dim: &HeldDim) -> String {
    let HeldDim { table, rows } = dim;
    let rows = counted(*rows, "row");
    format!("dim {}.{}\n  holds:    {rows}\n", table.schema, table.name)
}

fn counted(count: usize, thing: &str) -> String {
    let plural = if count == 1 { "" } else { "s" };
    format!("{count} {thing}{plural}")
}

/// PostgreSQL's name for the type.
fn type_name(column_type: ColumnType) -> &'static str {
    match column_type {
        ColumnType::Int => "integer",
        ColumnType::Numeric { .. } => "numeric",
        ColumnType::Bool => "boolean",
        ColumnType::Text => "text",
        ColumnType::Time { zoned: true } => "timestamptz",
        ColumnType::Time { zoned: false } => "timestamp",
    }
}

fn column_type(column_type: ColumnType) -> String {
    match column_type {
        ColumnType::Numeric { scale } => format!("numeric scale {scale}"),
        other => type_name(other).to_owned(),
    }
}

fn time(seconds: i64) -> String {
    // PostgreSQL keeps times past jiff's year 9999.
    rfc3339(seconds).unwrap_or_else(|| format!("Unix second {seconds}"))
}

#[cfg(test)]
mod tests {
    use straum_app::HeldKey;
    use straum_domain::{Identifier, Table, Width};

    use super::*;
    use crate::json::Json;

    fn id(name: &str) -> Identifier {
        Identifier::new(name).unwrap()
    }

    fn key(name: &str, column_type: ColumnType) -> HeldKey {
        HeldKey {
            name: id(name),
            column_type,
        }
    }

    fn trips() -> HeldView {
        HeldView {
            name: id("trips"),
            widths: vec![Width { seconds: 900 }, Width { seconds: 86_400 }],
            bucket: ColumnType::Time { zoned: true },
            keys: vec![
                key("kind", ColumnType::Int),
                key("fare", ColumnType::Numeric { scale: 2 }),
                key("shared", ColumnType::Bool),
                key("borough", ColumnType::Text),
            ],
            measures: vec![id("trips"), id("fare_sum")],
            cells: 412,
            bound_cells: Some(1),
            range: Some(1_704_067_200..1_704_240_000),
        }
    }

    #[test]
    fn writes_views_then_dims() {
        let held = Held {
            views: vec![
                trips(),
                HeldView {
                    name: id("logins"),
                    widths: vec![Width { seconds: 90 }],
                    bucket: ColumnType::Time { zoned: false },
                    keys: Vec::new(),
                    measures: vec![id("logins")],
                    cells: 1,
                    bound_cells: None,
                    range: None,
                },
            ],
            dims: vec![HeldDim {
                table: Table {
                    schema: id("public"),
                    name: id("zones"),
                },
                rows: 1,
            }],
        };
        assert_eq!(
            write(&held),
            "\
view trips
  buckets:  900s, 86400s
  holds:    412 cells and 1 bound cell, 2024-01-01T00:00:00Z to 2024-01-03T00:00:00Z
  keys:     bucket timestamptz, kind integer, fare numeric scale 2, shared boolean, borough text
  measures: trips, fare_sum

view logins
  buckets:  90s
  holds:    1 cell
  keys:     bucket timestamp
  measures: logins

dim public.zones
  holds:    1 row
"
        );
    }

    #[test]
    fn writes_views_as_json() {
        let held = Held {
            views: vec![
                trips(),
                HeldView {
                    keys: Vec::new(),
                    bound_cells: None,
                    range: None,
                    ..trips()
                },
                HeldView {
                    range: Some(0..i64::MAX),
                    ..trips()
                },
            ],
            dims: vec![HeldDim {
                table: Table {
                    schema: id("public"),
                    name: id("zones"),
                },
                rows: 2,
            }],
        };
        let keys = concat!(
            r#"{"name":"bucket","type":"timestamptz"},{"name":"kind","type":"integer"},"#,
            r#"{"name":"fare","type":"numeric","scale":2},{"name":"shared","type":"boolean"},"#,
            r#"{"name":"borough","type":"text"}"#
        );
        let start = r#"{"name":"trips","#;
        let measures = r#""measures":["trips","fare_sum"]}"#;
        let expected = [
            r#"{"views":["#,
            start,
            r#""from":"2024-01-01T00:00:00Z","to":"2024-01-03T00:00:00Z","#,
            &format!(r#""keys":[{keys}],{measures},"#),
            start,
            r#""from":null,"to":null,"#,
            &format!(r#""keys":[{{"name":"bucket","type":"timestamptz"}}],{measures},"#),
            start,
            r#""from":"1970-01-01T00:00:00Z","to":null,"#,
            &format!(r#""keys":[{keys}],{measures}"#),
            r#"]}"#,
        ];
        assert_eq!(String::from_utf8(json(&held)).unwrap(), expected.concat());
    }

    #[test]
    fn views_as_json_hold_only_what_the_shell_reads() {
        let held = Held {
            views: vec![trips()],
            dims: vec![HeldDim {
                table: Table {
                    schema: id("public"),
                    name: id("zones"),
                },
                rows: 2,
            }],
        };
        let names = |fields: &[(String, Json)]| -> Vec<String> {
            fields.iter().map(|(name, _)| name.clone()).collect()
        };
        let Ok(Json::Object(top)) = Json::parse(&json(&held)) else {
            panic!("not an object");
        };
        assert_eq!(names(&top), ["views"]);
        let Json::Array(views) = &top[0].1 else {
            panic!("views is not an array");
        };
        let Json::Object(view) = &views[0] else {
            panic!("a view is not an object");
        };
        let view = names(view);
        assert_eq!(view, ["name", "from", "to", "keys", "measures"]);
        for dropped in ["buckets", "cells", "bound_cells", "dims"] {
            assert!(!view.iter().chain(&names(&top)).any(|name| name == dropped));
        }
    }

    #[test]
    fn a_time_jiff_cannot_write_is_its_unix_second() {
        let held = Held {
            views: vec![HeldView {
                range: Some(0..i64::MAX),
                ..trips()
            }],
            dims: Vec::new(),
        };
        let text = write(&held);
        let range = "1970-01-01T00:00:00Z to Unix second 9223372036854775807\n";
        assert!(text.contains(range), "{text}");
    }
}
