//! Straum SQL: the spec format Straum reads at start, and the asks it
//! answers. `docs/src/spec.md` is its grammar (ADR-0029); `sqlparser`
//! parses the text and this crate walks the AST.
//!
//! PostgreSQL rejects view options, but Straum never sends a view to
//! PostgreSQL, so `WITH (buckets = ...)` needn't be one it takes.

#![forbid(unsafe_code)]

mod sql;

use std::fmt;

use straum_domain::{Ask, Literal, Spec, SpecError};

pub use sql::SqlError;

#[derive(Debug, PartialEq, Eq)]
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
