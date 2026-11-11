//! straum SQL: the spec format straum reads at start, and the asks it
//! answers.
//!
//! straum SQL is not SQL straum runs. It is a small subset of PostgreSQL,
//! made only for straum: `CREATE VIEW` for specs, `SELECT` over a view for
//! asks (see `sql/ask.rs`); anything outside it is an error. `sqlparser` parses the text and this crate walks the AST. Each view
//! is one fact:
//!
//! ```sql
//! CREATE VIEW trips WITH (buckets = '15 minutes') AS
//! SELECT date_bin('15 minutes', f.pickup_at, timestamptz 'epoch') AS bucket,
//!        f.payment_type AS payment_type,
//!        f.pu_location_id AS pu_location_id,
//!        pickup.borough AS borough,
//!        count(*) AS trips,
//!        sum(f.fare_amount) AS fare,
//!        count(*) FILTER (WHERE pickup.borough = 'Manhattan') AS manhattan
//! FROM public.yellow_trips AS f
//! LEFT OUTER JOIN public.taxi_zones AS pickup ON pickup.location_id = f.pu_location_id
//! GROUP BY 1, 2, 3, 4;
//! ```
//!
//! One spelling per meaning, nothing inferred (ADR-0029): every schema,
//! alias, column owner and output name is written, and every other
//! spelling is an error that names the one to write.
//!
//! - `WITH (buckets = '15 minutes, 1 day, 7 days')`: every bucket width
//!   straum keeps, the `date_bin` width first, each wider one a whole
//!   multiple of the width before it. PostgreSQL rejects view options;
//!   straum never sends a view to PostgreSQL, so it needn't take them.
//! - `FROM schema.table AS alias`: the fact.
//! - `LEFT OUTER JOIN schema.table AS alias ON alias.key = fact.column`,
//!   with more equalities joined by `AND` for a key of several columns,
//!   the joined table's column on the left: a ref named `alias` to the
//!   dim `table`. The dim columns the view selects are the ref's
//!   attributes, which come with each fact row and key its cells; a join
//!   must select one. The fact columns joined on may be selected as `by`
//!   columns or left out.
//! - `date_bin('<n> <unit>', fact.column, timestamptz 'epoch') AS bucket`:
//!   the bucket; `timestamp 'epoch'` for a column without a zone.
//! - Other fact columns are `by` columns; `(fact.column IS NULL) AS name`
//!   or `IS NOT NULL` is a boolean `by` key.
//! - Every `SELECT` item carries `AS name`; the name is what asks use.
//! - `count(*)`, `count(DISTINCT c)`, `sum`, `min`, `max`, `avg`, each `AS`
//!   a name and with an optional `FILTER (WHERE ...)` of `=` and `IN`, or
//!   `IS NULL` and `IS NOT NULL` over a column with a null-ness key, joined
//!   by `AND`, every column as `alias.column`.
//! - GROUP BY lists the keys' positions in SELECT, in order, the bucket
//!   included.
//! - Every column is `alias.column`, the fact's included.

#![forbid(unsafe_code)]

mod sql;

use std::fmt;

use straum_domain::{Ask, Literal, Spec, SpecError};

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

/// An ask, its values taken from `parameters` by `$n`.
pub fn parse_ask(query: &str, parameters: &[Literal]) -> Result<Ask, Vec<SqlError>> {
    sql::parse_ask(query, parameters)
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
