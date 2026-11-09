//! straum SQL: the spec format straum reads at start.
//!
//! straum SQL is not SQL straum runs. It is a small subset of PostgreSQL
//! `CREATE VIEW` syntax, made only for straum specs; anything outside it is an
//! error. `sqlparser` parses the text and this crate walks the AST. Each view
//! is one fact:
//!
//! ```sql
//! CREATE VIEW trips WITH (key = id) AS
//! SELECT date_bin('15 minutes', f.pickup_at, timestamptz 'epoch') AS bucket,
//!        f.payment_type,
//!        f.pu_location_id,
//!        count(*) AS trips,
//!        sum(f.fare_amount) AS fare,
//!        count(*) FILTER (WHERE pickup.borough = 'Manhattan') AS manhattan
//! FROM public.yellow_trips f
//! LEFT JOIN public.taxi_zones pickup ON pickup.location_id = f.pu_location_id
//! GROUP BY 1, 2, 3;
//! ```
//!
//! - `WITH (key = column)`: the fact table's primary key. PostgreSQL rejects
//!   view options, so straum only ever sends the `SELECT` to PostgreSQL.
//! - `buckets = '1 day, 7 days'`: optional wider bucket widths, each a wider
//!   whole multiple of the width before it.
//! - `FROM schema.table alias`: the fact.
//! - `LEFT JOIN schema.table alias ON alias.key = fact.column`: a ref named
//!   `alias` to the dim `table`. The fact column must be selected.
//! - `date_bin('<n> <unit>', fact.column, timestamptz 'epoch')`: the bucket.
//! - Other fact columns are `by` columns; dim columns are attributes, read
//!   after aggregation.
//! - `count(*)`, `count(DISTINCT c)`, `sum`, `min`, `max`, `avg`, each `AS`
//!   a name and with an optional `FILTER (WHERE ...)` of `=` and `IN` joined
//!   by `AND`.
//! - GROUP BY names exactly the selected columns and the bucket.
//! - Every column is qualified with its table alias.

#![forbid(unsafe_code)]

mod sql;

use std::fmt;

use straum_domain::{Spec, SpecError};

pub use sql::SqlError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Sql(Vec<SqlError>),
    Spec(Vec<SpecError>),
}

pub fn parse(src: &str) -> Result<Spec, Error> {
    let (dims, facts) = sql::parse(src).map_err(Error::Sql)?;
    Spec::new(dims, facts).map_err(Error::Spec)
}

impl fmt::Display for SqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(f, "{}", self.msg)
        } else {
            write!(f, "line {}: {}", self.line, self.msg)
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let lines: Vec<String> = match self {
            Error::Sql(errors) => errors.iter().map(ToString::to_string).collect(),
            Error::Spec(errors) => errors.iter().map(ToString::to_string).collect(),
        };
        write!(f, "{}", lines.join("\n"))
    }
}

impl std::error::Error for Error {}
