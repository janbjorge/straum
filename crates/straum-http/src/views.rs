//! `GET /views`: every view asks may read and how much of it Straum holds.
//! `routes.rs` takes a `Held` from the app and hands it here. `write`
//! turns it into plain text for people, as `straum views` prints it;
//! `json` turns it into each view's names, the range of buckets it
//! holds and each width it keeps only for a time, for a program that
//! sends `Accept: application/json`, as `straum shell` does to complete
//! names:
//!
//! ```json
//! {"views": [{"name": "trips", "from": "2024-01-01T00:00:00Z",
//!             "to": "2024-01-03T00:00:00Z",
//!             "kept": [{"width": "15 minutes", "for": "2 days",
//!                       "from": "2024-01-02T00:00:00Z"}],
//!             "keys": [{"name": "bucket", "type": "timestamptz"},
//!                      {"name": "fare", "type": "numeric", "scale": 2}],
//!             "measures": ["trips"]}]}
//! ```
//!
//! `from` and `to` are null for a view with no bucket, and a kept
//! width's `from` before its first horizon; `kept` is empty for a view
//! that keeps every width for good. A type is
//! `integer`, `numeric` with its `scale`, `boolean`, `text`,
//! `timestamptz`, `timestamp`, `date` or `uuid`. `openapi.json` holds
//! this shape as a schema.

use std::fmt::Write as _;

use straum_app::{Held, HeldDim, HeldKept, HeldView, ViewKey};
use straum_domain::{ColumnType, Identifier, Width};

use crate::json::{NULL, quoted, quoted_or_null};
use crate::time::{rfc3339, rfc3339_or_unix_second};

/// Every view, then every dim, as blocks of plain text.
pub(crate) fn write(held: &Held) -> String {
    let mut blocks = Vec::with_capacity(held.views.len() + held.dims.len());
    for held_view in &held.views {
        blocks.push(view_text(held_view));
    }
    for held_dim in &held.dims {
        blocks.push(dim_text(held_dim));
    }
    blocks.join("\n")
}

/// Every view's names and range, as the JSON body `straum shell` reads,
/// ending in a newline as `--json` prints it.
pub(crate) fn json(held: &Held) -> Vec<u8> {
    let mut views = Vec::with_capacity(held.views.len());
    for held_view in &held.views {
        views.push(view_json(held_view));
    }
    format!("{{\"views\":[{}]}}\n", views.join(",")).into_bytes()
}

fn view_json(view: &HeldView) -> String {
    let time = &view.time;
    let mut keys = vec![key_json(time.name.as_str(), time.column_type)];
    for key in &view.keys {
        keys.push(key_json(key.name.as_str(), key.column_type));
    }
    let mut measures = Vec::with_capacity(view.measures.len());
    for measure in &view.measures {
        measures.push(quoted(measure.as_str()));
    }
    let (from, to) = match &view.range {
        Some(range) => (edge_json(range.start), edge_json(range.end)),
        None => (NULL.to_owned(), NULL.to_owned()),
    };
    let mut kept = Vec::with_capacity(view.kept.len());
    for held_kept in &view.kept {
        kept.push(kept_json(held_kept));
    }
    format!(
        "{{\"name\":{},\"time\":{},\"from\":{from},\"to\":{to},\"kept\":[{}],\"keys\":[{}],\"measures\":[{}]}}",
        quoted(view.name.as_str()),
        quoted(time.name.as_str()),
        kept.join(","),
        keys.join(","),
        measures.join(",")
    )
}

fn kept_json(kept: &HeldKept) -> String {
    let from = match kept.kept_from {
        Some(seconds) => edge_json(seconds),
        None => NULL.to_owned(),
    };
    format!(
        "{{\"width\":{},\"for\":{},\"from\":{from}}}",
        quoted(&kept.width.to_string()),
        quoted(&kept.kept_for.to_string())
    )
}

fn edge_json(seconds: i64) -> String {
    // A time past jiff's year 9999 is null too, as it has no RFC 3339
    // form here.
    quoted_or_null(rfc3339(seconds).as_deref())
}

fn key_json(name: &str, column: ColumnType) -> String {
    let scale = match column {
        ColumnType::Numeric { scale } => format!(",\"scale\":{scale}"),
        _ => String::new(),
    };
    format!(
        "{{\"name\":{},\"type\":{}{scale}}}",
        quoted(name),
        quoted(type_name(column))
    )
}

fn view_text(view: &HeldView) -> String {
    let mut widths = Vec::with_capacity(view.widths.len());
    for (width, cells) in view.widths.iter().zip(&view.cells) {
        widths.push(width_text(*width, *cells, &view.kept));
    }
    let date_bin_cells = view.cells.first().copied().unwrap_or_default();
    let mut holds = counted(date_bin_cells, "cell");
    if let Some(bound) = view.bound_cells {
        let _ = write!(holds, " and {}", counted(bound, "bound cell"));
    }
    if let Some(range) = &view.range {
        // The end is the first second past the last bucket, which is what
        // an ask's `<time> < $n` takes.
        let _ = write!(
            holds,
            ", {} to {}",
            rfc3339_or_unix_second(range.start),
            rfc3339_or_unix_second(range.end)
        );
    }
    let time = &view.time;
    let mut keys = vec![format!("{} {}", time.name, column_type(time.column_type))];
    for key in &view.keys {
        keys.push(format!("{} {}", key.name, column_type(key.column_type)));
    }
    let measures: Vec<&str> = view.measures.iter().map(Identifier::as_str).collect();
    format!(
        "view {}\n  buckets:  {}\n  holds:    {holds}\n  keys:     {}\n  measures: {}\n",
        view.name,
        widths.join(", "),
        keys.join(", "),
        measures.join(", ")
    )
}

/// A width with the cells it holds, and for one kept only for a time how
/// long and, once the store is advanced, from when.
fn width_text(width: Width, cells: usize, kept: &[HeldKept]) -> String {
    let mut text = format!("{width} ({})", counted(cells, "cell"));
    for held_kept in kept {
        if held_kept.width != width {
            continue;
        }
        let _ = write!(text, " kept for {}", held_kept.kept_for);
        if let Some(kept_from) = held_kept.kept_from {
            let _ = write!(text, " from {}", rfc3339_or_unix_second(kept_from));
        }
    }
    text
}

/// A dim, and the view keys an ask reads it through: an ask names a view,
/// never a dim.
fn dim_text(dim: &HeldDim) -> String {
    let HeldDim {
        table,
        rows,
        asked_as,
    } = dim;
    let rows = counted(*rows, "row");
    let mut text = format!("dim {}.{}\n  holds:    {rows}\n", table.schema, table.name);
    if asked_as.is_empty() {
        return text;
    }
    let mut keys = Vec::with_capacity(asked_as.len());
    for ViewKey { view, key } in asked_as {
        keys.push(format!("{view}.{key}"));
    }
    let _ = writeln!(text, "  asked as: {}", keys.join(", "));
    text
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
        ColumnType::Date => "date",
        ColumnType::Uuid => "uuid",
    }
}

fn column_type(column_type: ColumnType) -> String {
    match column_type {
        ColumnType::Numeric { scale } => format!("numeric scale {scale}"),
        other => type_name(other).to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use straum_app::{HeldKept, HeldKey};
    use straum_domain::{Identifier, Table};

    use super::*;
    use crate::json::{Field, Json};

    fn identifier(name: &str) -> Identifier {
        Identifier::new(name).unwrap()
    }

    fn key(name: &str, column_type: ColumnType) -> HeldKey {
        HeldKey {
            name: identifier(name),
            column_type,
        }
    }

    fn trips() -> HeldView {
        HeldView {
            name: identifier("trips"),
            widths: vec![Width { seconds: 900 }, Width { seconds: 86_400 }],
            time: key("bucket", ColumnType::Time { zoned: true }),
            keys: vec![
                key("kind", ColumnType::Int),
                key("fare", ColumnType::Numeric { scale: 2 }),
                key("shared", ColumnType::Bool),
                key("borough", ColumnType::Text),
            ],
            measures: vec![identifier("trips"), identifier("fare_sum")],
            cells: vec![412, 20],
            bound_cells: Some(1),
            range: Some(1_704_067_200..1_704_240_000),
            kept: Vec::new(),
        }
    }

    /// Quarter hours kept for 2 days, from `kept_from`.
    fn kept(kept_from: Option<i64>) -> Vec<HeldKept> {
        vec![HeldKept {
            width: Width { seconds: 900 },
            kept_for: Width { seconds: 172_800 },
            kept_from,
        }]
    }

    fn zones(rows: usize) -> HeldDim {
        HeldDim {
            table: Table {
                schema: identifier("public"),
                name: identifier("zones"),
            },
            rows,
            asked_as: vec![ViewKey {
                view: identifier("trips"),
                key: identifier("borough"),
            }],
        }
    }

    #[test]
    fn writes_views_then_dims() {
        let held = Held {
            views: vec![
                trips(),
                HeldView {
                    name: identifier("logins"),
                    widths: vec![Width { seconds: 90 }],
                    time: key("bucket", ColumnType::Time { zoned: false }),
                    keys: Vec::new(),
                    measures: vec![identifier("logins")],
                    cells: vec![1],
                    bound_cells: None,
                    range: None,
                    kept: Vec::new(),
                },
            ],
            dims: vec![zones(1)],
        };
        assert_eq!(
            write(&held),
            "\
view trips
  buckets:  15 minutes (412 cells), 1 day (20 cells)
  holds:    412 cells and 1 bound cell, 2024-01-01T00:00:00Z to 2024-01-03T00:00:00Z
  keys:     bucket timestamptz, kind integer, fare numeric scale 2, shared boolean, borough text
  measures: trips, fare_sum

view logins
  buckets:  90 seconds (1 cell)
  holds:    1 cell
  keys:     bucket timestamp
  measures: logins

dim public.zones
  holds:    1 row
  asked as: trips.borough
"
        );
    }

    #[test]
    fn date_and_uuid_keys_go_by_postgresql_names() {
        let view = HeldView {
            keys: vec![key("day", ColumnType::Date), key("rider", ColumnType::Uuid)],
            ..trips()
        };
        let held = Held {
            views: vec![view],
            dims: Vec::new(),
        };
        assert!(write(&held).contains("keys:     bucket timestamptz, day date, rider uuid\n"));
        let json = String::from_utf8(json(&held)).unwrap();
        assert!(json.contains(r#"{"name":"day","type":"date"},{"name":"rider","type":"uuid"}"#));
    }

    #[test]
    fn a_dim_no_view_reads_from_says_only_its_rows() {
        let unread = HeldDim {
            asked_as: Vec::new(),
            ..zones(2)
        };
        assert_eq!(dim_text(&unread), "dim public.zones\n  holds:    2 rows\n");
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
            dims: vec![zones(2)],
        };
        let keys = concat!(
            r#"{"name":"bucket","type":"timestamptz"},{"name":"kind","type":"integer"},"#,
            r#"{"name":"fare","type":"numeric","scale":2},{"name":"shared","type":"boolean"},"#,
            r#"{"name":"borough","type":"text"}"#
        );
        let start = r#"{"name":"trips","time":"bucket","#;
        let measures = r#""measures":["trips","fare_sum"]}"#;
        let expected = [
            r#"{"views":["#,
            start,
            r#""from":"2024-01-01T00:00:00Z","to":"2024-01-03T00:00:00Z","kept":[],"#,
            &format!(r#""keys":[{keys}],{measures},"#),
            start,
            r#""from":null,"to":null,"kept":[],"#,
            &format!(r#""keys":[{{"name":"bucket","type":"timestamptz"}}],{measures},"#),
            start,
            r#""from":"1970-01-01T00:00:00Z","to":null,"kept":[],"#,
            &format!(r#""keys":[{keys}],{measures}"#),
            "]}\n",
        ];
        assert_eq!(String::from_utf8(json(&held)).unwrap(), expected.concat());
    }

    fn names(fields: &[Field]) -> Vec<String> {
        let mut names = Vec::with_capacity(fields.len());
        for field in fields {
            names.push(field.name.clone());
        }
        names
    }

    #[test]
    fn views_as_json_hold_only_what_the_shell_reads() {
        let held = Held {
            views: vec![trips()],
            dims: vec![zones(2)],
        };
        let Ok(Json::Object(top)) = Json::parse(&json(&held)) else {
            panic!("not an object");
        };
        assert_eq!(names(&top), ["views"]);
        let Json::Array(views) = &top[0].value else {
            panic!("views is not an array");
        };
        let Json::Object(view) = &views[0] else {
            panic!("a view is not an object");
        };
        let view = names(view);
        assert_eq!(
            view,
            ["name", "time", "from", "to", "kept", "keys", "measures"]
        );
        for dropped in ["buckets", "cells", "bound_cells", "dims"] {
            assert!(!view.contains(&dropped.to_owned()), "{dropped}");
            assert!(!names(&top).contains(&dropped.to_owned()), "{dropped}");
        }
    }

    /// A width kept for a time says for how long and, once the store is
    /// advanced, from when; the widths kept for good say only their cells.
    #[test]
    fn a_kept_width_says_how_long_and_from_when() {
        let advanced = HeldView {
            kept: kept(Some(1_704_153_600)),
            ..trips()
        };
        let text = view_text(&advanced);
        let buckets = "buckets:  15 minutes (412 cells) kept for 2 days \
                       from 2024-01-02T00:00:00Z, 1 day (20 cells)\n";
        assert!(text.contains(buckets), "{text}");
        let fresh = HeldView {
            kept: kept(None),
            ..trips()
        };
        let text = view_text(&fresh);
        assert!(
            text.contains("buckets:  15 minutes (412 cells) kept for 2 days, 1 day (20 cells)\n"),
            "{text}"
        );
        let kept_for_good = "buckets:  15 minutes (412 cells), 1 day (20 cells)\n";
        assert!(view_text(&trips()).contains(kept_for_good));
    }

    #[test]
    fn a_kept_width_goes_into_the_json_with_its_horizon() {
        let advanced = HeldView {
            kept: kept(Some(1_704_153_600)),
            ..trips()
        };
        let set =
            r#""kept":[{"width":"15 minutes","for":"2 days","from":"2024-01-02T00:00:00Z"}],"#;
        assert!(
            view_json(&advanced).contains(set),
            "{}",
            view_json(&advanced)
        );
        let fresh = HeldView {
            kept: kept(None),
            ..trips()
        };
        let unset = r#""kept":[{"width":"15 minutes","for":"2 days","from":null}],"#;
        assert!(view_json(&fresh).contains(unset), "{}", view_json(&fresh));
        assert!(view_json(&trips()).contains(r#""kept":[],"#));
    }
}
