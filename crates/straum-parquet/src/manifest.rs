//! The manifest: which Parquet files hold which table, and how their
//! columns read. A line each:
//!
//! ```text
//! # The taxi trips, as the TLC publishes them.
//! table public.yellow_trips
//! file yellow_tripdata_2024-01.parquet
//! column pickup_at timestamptz from tpep_pickup_datetime in America/New_York
//! column fare_amount numeric(12,2) from fare_amount
//! ```
//!
//! A table has one file or more, found from the manifest's directory, and
//! a column for each one Straum may ask for: its name in the table, its
//! type there, and the file's column it is read `from`. One spelling per
//! type, as PostgreSQL writes it: `int`, `numeric(p,s)`, `bool`, `text`,
//! `timestamptz`, `timestamp`. `in ZONE` says whose clock a file's
//! timestamp without a zone shows, which a `timestamptz` needs to know.

use std::fmt;
use std::path::{Path, PathBuf};

use straum_domain::{ColumnType, Identifier};

/// The widest numeric Straum reads, as from PostgreSQL.
const MAX_PRECISION: u32 = 18;

const TYPES: &str = "int, numeric(p,s), bool, text, timestamptz or timestamp";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Manifest {
    pub(crate) tables: Vec<Table>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Table {
    pub(crate) schema: Identifier,
    pub(crate) name: Identifier,
    pub(crate) files: Vec<PathBuf>,
    pub(crate) columns: Vec<Column>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Column {
    pub(crate) name: Identifier,
    pub(crate) column_type: ColumnType,
    /// The file's column, by the name the file gives it.
    pub(crate) from: String,
    /// The IANA name of the zone whose clock the file's column shows.
    pub(crate) zone: Option<String>,
}

/// A line the manifest can't hold, counted from 1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManifestError {
    line: usize,
    what: String,
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.what)
    }
}

impl Table {
    pub(crate) fn column(&self, name: &Identifier) -> Option<&Column> {
        self.columns.iter().find(|column| column.name == *name)
    }
}

impl Manifest {
    pub(crate) fn table(&self, schema: &Identifier, name: &Identifier) -> Option<&Table> {
        (self.tables.iter()).find(|table| table.schema == *schema && table.name == *name)
    }
}

/// Reads a manifest whose files are found from `directory`.
pub(crate) fn parse(text: &str, directory: &Path) -> Result<Manifest, ManifestError> {
    let mut tables: Vec<Table> = Vec::new();
    // The line the last table opens on, for what it turns out to lack.
    let mut opened = 0;
    for (index, line) in text.lines().enumerate() {
        let at = |what: String| ManifestError {
            line: index + 1,
            what,
        };
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim_start();
        if word == "table" {
            if let Some(table) = tables.last() {
                complete(table, opened)?;
            }
            let (schema, name) = qualified(rest)
                .ok_or_else(|| at(format!("table takes schema.table, not {rest:?}")))?;
            if tables.iter().any(|t| t.schema == schema && t.name == name) {
                return Err(at(format!("table {schema}.{name} is given twice")));
            }
            tables.push(Table {
                schema,
                name,
                files: Vec::new(),
                columns: Vec::new(),
            });
            opened = index + 1;
            continue;
        }
        if !["file", "column"].contains(&word) {
            return Err(at(format!(
                "{word:?} starts no line; a line is table, file or column"
            )));
        }
        let Some(table) = tables.last_mut() else {
            return Err(at(format!("{word} comes before any table")));
        };
        if word == "file" {
            if rest.is_empty() {
                return Err(at("file takes a path".into()));
            }
            table.files.push(directory.join(rest));
            continue;
        }
        let column = column(rest).map_err(at)?;
        if table.column(&column.name).is_some() {
            return Err(at(format!("column {} is given twice", column.name)));
        }
        table.columns.push(column);
    }
    match tables.last() {
        Some(table) => complete(table, opened)?,
        None => {
            return Err(ManifestError {
                line: 1,
                what: "the manifest names no table".into(),
            });
        }
    }
    Ok(Manifest { tables })
}

/// A table needs somewhere to read and something to read there.
fn complete(table: &Table, opened: usize) -> Result<(), ManifestError> {
    let lacks = match (table.files.is_empty(), table.columns.is_empty()) {
        (true, _) => "file",
        (false, true) => "column",
        (false, false) => return Ok(()),
    };
    Err(ManifestError {
        line: opened,
        what: format!("table {}.{} has no {lacks}", table.schema, table.name),
    })
}

fn qualified(text: &str) -> Option<(Identifier, Identifier)> {
    let (schema, name) = text.split_once('.')?;
    Some((Identifier::new(schema)?, Identifier::new(name)?))
}

/// `NAME TYPE from FILE_COLUMN`, then `in ZONE` for a `timestamptz` read
/// from a clock.
fn column(text: &str) -> Result<Column, String> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let (name, type_name, from, zone) = match words[..] {
        [name, type_name, "from", from] => (name, type_name, from, None),
        [name, type_name, "from", from, "in", zone] => (name, type_name, from, Some(zone)),
        _ => {
            return Err(format!(
                "column takes NAME TYPE from FILE_COLUMN, then in ZONE if need be, not {text:?}"
            ));
        }
    };
    let name = Identifier::new(name).ok_or_else(|| format!("{name:?} is no column name"))?;
    let column_type =
        column_type(type_name).ok_or_else(|| format!("{type_name:?} is no type; write {TYPES}"))?;
    if zone.is_some() && column_type != (ColumnType::Time { zoned: true }) {
        return Err(format!(
            "column {name} is {column_type}; only a timestamptz is read in a zone"
        ));
    }
    Ok(Column {
        name,
        column_type,
        from: from.to_owned(),
        zone: zone.map(str::to_owned),
    })
}

fn column_type(text: &str) -> Option<ColumnType> {
    match text {
        "int" => Some(ColumnType::Int),
        "bool" => Some(ColumnType::Bool),
        "text" => Some(ColumnType::Text),
        "timestamptz" => Some(ColumnType::Time { zoned: true }),
        "timestamp" => Some(ColumnType::Time { zoned: false }),
        _ => {
            let inner = text.strip_prefix("numeric(")?.strip_suffix(')')?;
            let (precision, scale) = inner.split_once(',')?;
            let number = |text: &str| -> Option<u32> {
                let plain = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
                plain.then(|| text.parse().ok()).flatten()
            };
            let (precision, scale) = (number(precision)?, number(scale)?);
            ((1..=MAX_PRECISION).contains(&precision) && scale <= precision)
                .then_some(ColumnType::Numeric { scale })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAXI: &str = "\
# The taxi trips, as the TLC publishes them.

table public.taxi_zones
  file taxi_zones.parquet
  column location_id int from LocationID
  column borough text from Borough

table public.yellow_trips
file yellow tripdata 2024-01.parquet
file /elsewhere/yellow_tripdata_2024-02.parquet
column pickup_at   timestamptz from tpep_pickup_datetime in America/New_York
column dropoff_at\ttimestamp from tpep_dropoff_datetime
column fare_amount numeric(12,2) from fare_amount
column shared bool from shared
";

    fn id(name: &str) -> Identifier {
        Identifier::new(name).unwrap()
    }

    fn column(name: &str, column_type: ColumnType, from: &str, zone: Option<&str>) -> Column {
        Column {
            name: id(name),
            column_type,
            from: from.to_owned(),
            zone: zone.map(str::to_owned),
        }
    }

    fn refused(text: &str) -> String {
        parse(text, Path::new("/data")).unwrap_err().to_string()
    }

    #[test]
    fn reads_tables_files_and_columns() {
        let manifest = parse(TAXI, Path::new("/data")).unwrap();
        let zone = Some("America/New_York");
        assert_eq!(
            manifest.tables,
            [
                Table {
                    schema: id("public"),
                    name: id("taxi_zones"),
                    files: vec![PathBuf::from("/data/taxi_zones.parquet")],
                    columns: vec![
                        column("location_id", ColumnType::Int, "LocationID", None),
                        column("borough", ColumnType::Text, "Borough", None),
                    ],
                },
                Table {
                    schema: id("public"),
                    name: id("yellow_trips"),
                    files: vec![
                        // A path is the rest of its line, spaces and all.
                        PathBuf::from("/data/yellow tripdata 2024-01.parquet"),
                        PathBuf::from("/elsewhere/yellow_tripdata_2024-02.parquet"),
                    ],
                    columns: vec![
                        column(
                            "pickup_at",
                            ColumnType::Time { zoned: true },
                            "tpep_pickup_datetime",
                            zone
                        ),
                        column(
                            "dropoff_at",
                            ColumnType::Time { zoned: false },
                            "tpep_dropoff_datetime",
                            None
                        ),
                        column(
                            "fare_amount",
                            ColumnType::Numeric { scale: 2 },
                            "fare_amount",
                            None
                        ),
                        column("shared", ColumnType::Bool, "shared", None),
                    ],
                },
            ]
        );
        let found = manifest.table(&id("public"), &id("taxi_zones")).unwrap();
        assert_eq!(found.column(&id("borough")).unwrap().from, "Borough");
        assert_eq!(found.column(&id("zone")), None);
        assert_eq!(manifest.table(&id("other"), &id("taxi_zones")), None);
        assert_eq!(manifest.table(&id("public"), &id("zones")), None);
    }

    #[test]
    fn numerics_are_those_straum_reads() {
        let scale = |text| column_type(text).map(ColumnType::scale);
        assert_eq!(scale("numeric(12,2)"), Some(2));
        assert_eq!(scale("numeric(18,18)"), Some(18));
        assert_eq!(scale("numeric(1,0)"), Some(0));
        for bad in [
            "numeric",
            "numeric(12)",
            "numeric(19,2)",
            "numeric(0,0)",
            "numeric(2,3)",
            "numeric(12,-2)",
            "numeric(+12,2)",
            "numeric(12,2",
            "numeric(,)",
            "numeric(12,2,1)",
            "NUMERIC(12,2)",
        ] {
            assert_eq!(column_type(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_line_that_does_not_fit_is_named() {
        let cases = [
            ("", "line 1: the manifest names no table"),
            ("# nothing\n\n", "line 1: the manifest names no table"),
            ("file t.parquet\n", "line 1: file comes before any table"),
            (
                "\ncolumn a int from a\n",
                "line 2: column comes before any table",
            ),
            (
                "tables public.t\n",
                "line 1: \"tables\" starts no line; a line is table, file or column",
            ),
            ("table t\n", "line 1: table takes schema.table, not \"t\""),
            ("table\n", "line 1: table takes schema.table, not \"\""),
            (
                "table public.T\n",
                "line 1: table takes schema.table, not \"public.T\"",
            ),
            (
                "table public.t extra\n",
                "line 1: table takes schema.table, not \"public.t extra\"",
            ),
            ("table public.t\n", "line 1: table public.t has no file"),
            (
                "table public.t\nfile t.parquet\n",
                "line 1: table public.t has no column",
            ),
            (
                "table public.t\ncolumn a int from a\n\ntable public.u\n",
                "line 1: table public.t has no file",
            ),
            (
                "table public.t\nfile t\ncolumn a int from a\n# next\ntable public.u\nfile u\n",
                "line 5: table public.u has no column",
            ),
            (
                "table public.t\nfile t\ncolumn a int from a\ntable public.t\n",
                "line 4: table public.t is given twice",
            ),
            ("table public.t\nfile\n", "line 2: file takes a path"),
        ];
        for (text, error) in cases {
            assert_eq!(refused(text), error, "{text:?}");
        }
    }

    #[test]
    fn a_column_that_does_not_fit_is_named() {
        let shape = |text: &str| {
            format!(
                "column takes NAME TYPE from FILE_COLUMN, then in ZONE if need be, not {text:?}"
            )
        };
        let no_type = |text: &str| format!("{text:?} is no type; write {TYPES}");
        let cases = [
            ("column", shape("")),
            ("column a", shape("a")),
            ("column a int", shape("a int")),
            ("column a int a", shape("a int a")),
            ("column a int from", shape("a int from")),
            ("column a int from a b", shape("a int from a b")),
            ("column a int from a in", shape("a int from a in")),
            (
                "column a int from a in UTC now",
                shape("a int from a in UTC now"),
            ),
            // The type is one word, so PostgreSQL's own space is one too many.
            (
                "column a numeric(12, 2) from a",
                shape("a numeric(12, 2) from a"),
            ),
            (
                "column Fare int from a",
                "\"Fare\" is no column name".into(),
            ),
            ("column a integer from a", no_type("integer")),
            ("column a numeric(19,2) from a", no_type("numeric(19,2)")),
            (
                "column a timestamp from a in UTC",
                "column a is a timestamp; only a timestamptz is read in a zone".into(),
            ),
            (
                "column a int from a in UTC",
                "column a is an integer; only a timestamptz is read in a zone".into(),
            ),
            (
                "column b int from x\ncolumn b text from y",
                "column b is given twice".into(),
            ),
        ];
        for (lines, error) in cases {
            let last = 2 + lines.lines().count();
            assert_eq!(
                refused(&format!("table public.t\nfile t.parquet\n{lines}\n")),
                format!("line {last}: {error}")
            );
        }
    }
}
