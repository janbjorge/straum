//! What a load says about itself. Before the first table the writer
//! names each table with the source's estimate of it and the columns
//! read, in one line; while a table loads, [`Loading`] says how far it
//! is, for the status; after each, a line gives its rows and their rate.

use std::fmt;

use straum_domain::{Identifier, Spec, Table};

use crate::source::Estimate;
use crate::writer::format_mebibytes;

const PERCENT: u64 = 100;

const GROUPED: &str = "facts_by_table names the spec's own views";

/// One load: one dim, or the views that read one fact table, read in one
/// pass over their table.
pub(crate) struct Load {
    /// `dim` or `view`.
    pub(crate) kind: &'static str,
    pub(crate) names: Vec<Identifier>,
    /// The table read, `schema.name`.
    pub(crate) table: String,
    /// Every column any of `names` reads, once each.
    pub(crate) columns: Vec<Identifier>,
}

/// How far the load is, while the tables load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Loading {
    /// The table loading now, `schema.name`.
    pub table: String,
    /// Its place among the tables, from 1.
    pub position: usize,
    /// How many tables the load reads.
    pub tables: usize,
    pub estimate: Estimate,
    /// Rows read of the table so far.
    pub rows_read: u64,
    /// Bytes of commits the stream holds while the tables load.
    pub held_bytes: usize,
    /// As [`Limits::max_stream_memory`](crate::Limits::max_stream_memory).
    pub max_stream_memory: usize,
}

/// Every load of the spec, in the order they run: dims first, so a
/// fact's rows find theirs when asked.
pub(crate) fn loads(spec: &Spec) -> Vec<Load> {
    let mut loads = Vec::new();
    for dim in spec.dims() {
        loads.push(Load {
            kind: "dim",
            names: vec![dim.name.clone()],
            table: qualified(&dim.table),
            columns: dim.columns().cloned().collect(),
        });
    }
    for names in spec.facts_by_table() {
        let mut table = String::new();
        let mut columns = Vec::new();
        for name in &names {
            let fact = spec.fact(name.as_str()).expect(GROUPED);
            table = qualified(&fact.table);
            for column in fact.columns() {
                if !columns.contains(&column) {
                    columns.push(column);
                }
            }
        }
        loads.push(Load {
            kind: "view",
            names,
            table,
            columns,
        });
    }
    loads
}

fn qualified(table: &Table) -> String {
    format!("{}.{}", table.schema, table.name)
}

/// `loading 2 tables, estimated 1004 rows and 0.1 MiB: public.zones (dim
/// zones) 4 rows, 0.0 MiB, columns id, borough; ...`. The total's rows
/// are unknown when any table's are.
pub(crate) fn planned(loads: &[Load], estimates: &[Estimate]) -> String {
    let mut rows: Option<u64> = Some(0);
    let mut bytes = 0;
    let mut tables = Vec::new();
    for (load, estimate) in loads.iter().zip(estimates) {
        rows = match (rows, estimate.rows) {
            (Some(total), Some(more)) => Some(total.saturating_add(more)),
            _ => None,
        };
        bytes = estimate.bytes.saturating_add(bytes);
        let mut columns = Vec::new();
        for column in &load.columns {
            columns.push(column.as_str());
        }
        tables.push(format!(
            "{} ({}) {}, {}, columns {}",
            load.table,
            named(load.kind, &load.names),
            rows_of(estimate.rows),
            format_mebibytes(estimate.bytes),
            columns.join(", ")
        ));
    }
    let plural = if loads.len() == 1 { "" } else { "s" };
    format!(
        "loading {} table{plural}, estimated {} and {}: {}",
        loads.len(),
        rows_of(rows),
        format_mebibytes(bytes),
        tables.join("; ")
    )
}

/// `loaded view trips in 1.2 s: 1000 rows, 2.5 MiB on disk, 833 rows/s`.
/// No rate when no time passed.
#[expect(
    clippy::cast_precision_loss,
    reason = "a rate shown without decimals loses nothing that shows"
)]
pub(crate) fn loaded(load: &Load, rows: u64, bytes: u64, seconds: f64) -> String {
    let rate = if seconds > 0.0 {
        format!(", {:.0} rows/s", rows as f64 / seconds)
    } else {
        String::new()
    };
    format!(
        "loaded {} in {seconds:.1} s: {rows} rows, {} on disk{rate}",
        named(load.kind, &load.names),
        format_mebibytes(bytes)
    )
}

/// `view trips` for one name, `views trips, dropoffs` for several.
fn named(kind: &str, names: &[Identifier]) -> String {
    let mut listed = Vec::new();
    for name in names {
        listed.push(name.as_str());
    }
    let plural = if names.len() == 1 { "" } else { "s" };
    format!("{kind}{plural} {}", listed.join(", "))
}

fn rows_of(rows: Option<u64>) -> String {
    match rows {
        Some(rows) => format!("{rows} rows"),
        None => "unknown rows".to_owned(),
    }
}

fn mebibytes(bytes: usize) -> String {
    format_mebibytes(u64::try_from(bytes).unwrap_or(u64::MAX))
}

impl Loading {
    /// How much of the estimate is read, up to all of it; `None` when the
    /// estimate is unknown or no rows.
    pub fn percent_read(&self) -> Option<u64> {
        let estimated = self.estimate.rows?;
        if estimated == 0 {
            return None;
        }
        let percent = self.rows_read.saturating_mul(PERCENT) / estimated;
        Some(percent.min(PERCENT))
    }
}

/// `loading public.trips, table 2 of 4, 41% of rows; commits held 3.0 MiB
/// of 1024.0 MiB`.
impl fmt::Display for Loading {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "loading {}, table {} of {}",
            self.table, self.position, self.tables
        )?;
        if let Some(percent) = self.percent_read() {
            write!(formatter, ", {percent}% of rows")?;
        }
        write!(
            formatter,
            "; commits held {} of {}",
            mebibytes(self.held_bytes),
            mebibytes(self.max_stream_memory)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Estimate, Load, Loading, loaded, loads, planned};

    const MIB: u64 = 1 << 20;

    fn trips() -> Loading {
        Loading {
            table: "public.trips".to_owned(),
            position: 2,
            tables: 4,
            estimate: Estimate {
                rows: Some(1000),
                bytes: MIB,
            },
            rows_read: 410,
            held_bytes: 3 << 20,
            max_stream_memory: 1 << 30,
        }
    }

    #[test]
    fn says_how_far_a_table_is() {
        assert_eq!(
            trips().to_string(),
            "loading public.trips, table 2 of 4, 41% of rows; \
             commits held 3.0 MiB of 1024.0 MiB"
        );
        // More rows than the estimate are all of it, no more.
        let past = Loading {
            rows_read: 5000,
            ..trips()
        };
        assert_eq!(past.percent_read(), Some(100));
    }

    #[test]
    fn gives_no_percent_without_an_estimate() {
        for rows in [None, Some(0)] {
            let unknown = Loading {
                estimate: Estimate { rows, bytes: 0 },
                ..trips()
            };
            assert_eq!(unknown.percent_read(), None);
            let text = unknown.to_string();
            assert_eq!(
                text,
                "loading public.trips, table 2 of 4; commits held 3.0 MiB of 1024.0 MiB"
            );
        }
    }

    fn plan() -> Vec<Load> {
        let spec = straum_notation::parse(
            "CREATE VIEW trips AS
             SELECT buckets(t.at, '1 hour') AS bucket, t.kind AS kind, count(*) AS trips
             FROM public.trips AS t GROUP BY 1, 2;
             CREATE VIEW fares AS
             SELECT buckets(t.at, '1 day') AS bucket, sum(t.fare) AS fare
             FROM public.trips AS t GROUP BY 1;
             CREATE VIEW logins AS
             SELECT buckets(l.at, '1 day') AS bucket, count(*) AS logins
             FROM public.logins AS l GROUP BY 1;",
        )
        .unwrap();
        loads(&spec)
    }

    #[test]
    fn plans_each_table_once_with_its_columns_and_a_total() {
        let loads = plan();
        let estimates = [
            Estimate {
                rows: Some(1000),
                bytes: 2 * MIB,
            },
            Estimate {
                rows: Some(24),
                bytes: MIB,
            },
        ];
        assert_eq!(
            planned(&loads, &estimates),
            "loading 2 tables, estimated 1024 rows and 3.0 MiB: \
             public.trips (views trips, fares) 1000 rows, 2.0 MiB, columns at, kind, fare; \
             public.logins (view logins) 24 rows, 1.0 MiB, columns at"
        );
    }

    #[test]
    fn a_table_of_unknown_rows_leaves_the_total_unknown() {
        let loads = plan();
        let estimates = [
            Estimate {
                rows: None,
                bytes: 0,
            },
            Estimate {
                rows: Some(24),
                bytes: MIB,
            },
        ];
        let line = planned(&loads, &estimates);
        assert!(
            line.starts_with("loading 2 tables, estimated unknown rows and 1.0 MiB: "),
            "{line}"
        );
        assert!(
            line.contains("(views trips, fares) unknown rows, 0.0 MiB"),
            "{line}"
        );
        assert!(!line.contains("-1"), "{line}");
    }

    #[test]
    fn a_loaded_table_gives_its_rows_and_rate() {
        let loads = plan();
        assert_eq!(
            loaded(&loads[0], 1000, 2 * MIB, 0.5),
            "loaded views trips, fares in 0.5 s: 1000 rows, 2.0 MiB on disk, 2000 rows/s"
        );
        assert_eq!(
            loaded(&loads[1], 0, 0, 0.0),
            "loaded view logins in 0.0 s: 0 rows, 0.0 MiB on disk"
        );
    }
}
