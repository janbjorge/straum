//! `/views`: every view asks may read and how much of it Straum holds, in
//! plain text for people, as `straum views` prints it.

use jiff::Timestamp;
use straum_app::{Held, HeldDim, HeldView};
use straum_domain::ColumnType;

pub(crate) fn write(held: &Held) -> String {
    let views = held.views.iter().map(view);
    let dims = held.dims.iter().map(dim);
    views.chain(dims).collect::<Vec<_>>().join("\n")
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

fn column_type(column_type: ColumnType) -> String {
    match column_type {
        ColumnType::Int => "integer".to_owned(),
        ColumnType::Numeric { scale } => format!("numeric scale {scale}"),
        ColumnType::Bool => "boolean".to_owned(),
        ColumnType::Text => "text".to_owned(),
        ColumnType::Time { zoned: true } => "timestamptz".to_owned(),
        ColumnType::Time { zoned: false } => "timestamp".to_owned(),
    }
}

fn time(seconds: i64) -> String {
    // PostgreSQL keeps times past jiff's year 9999.
    Timestamp::from_second(seconds)
        .map_or_else(|_| format!("Unix second {seconds}"), |at| at.to_string())
}

#[cfg(test)]
mod tests {
    use straum_app::HeldKey;
    use straum_domain::{Identifier, Table, Width};

    use super::*;

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
