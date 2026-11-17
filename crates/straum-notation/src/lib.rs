//! Straum SQL: the spec format Straum reads at start, and the asks it
//! answers. `docs/src/spec.md` is its grammar (ADR-0004). `sqlparser`
//! parses the text into a tree; the walker in `sql/` reads the tree into
//! the domain's dims, facts and asks; `Spec::new` then checks the views
//! against each other. `definitions` cuts the text into each view's
//! statement as written, for a reader to see.
//!
//! PostgreSQL rejects view options, but Straum never sends a view to
//! PostgreSQL, so `WITH (buckets = ...)` needn't be one it takes.

#![forbid(unsafe_code)]

mod sql;

use std::fmt;

use straum_domain::{Definition, Spec, SpecError};

pub use sql::{SqlError, parse_ask};

/// Why a spec was refused: what the walker found wrong, or failing that,
/// what the spec's own checks did.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Sql(Vec<SqlError>),
    Spec(Vec<SpecError>),
}

/// Reads a spec's text into a checked [`Spec`].
pub fn parse(text: &str) -> Result<Spec, Error> {
    let sql::Views { dims, facts } = sql::parse(text).map_err(Error::Sql)?;
    Spec::new(dims, facts).map_err(Error::Spec)
}

/// Each view's `CREATE VIEW` statement in `text`, which `spec` was parsed
/// from, as written.
pub fn definitions(text: &str, spec: &Spec) -> Vec<Definition> {
    let mut definitions = Vec::with_capacity(spec.facts().len());
    // A spec that parsed holds only `CREATE VIEW` statements, each a view
    // in the order written.
    for (fact, text) in spec.facts().iter().zip(sql::statement_texts(text)) {
        definitions.push(Definition {
            view: fact.name.clone(),
            text,
        });
    }
    definitions
}

impl fmt::Display for SqlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(formatter, "line {line}: {}", self.message),
            None => write!(formatter, "{}", self.message),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Sql(errors) => write_lines(formatter, errors),
            Error::Spec(errors) => write_lines(formatter, errors),
        }
    }
}

// Any printable errors: the walker's and the spec's.
fn write_lines(formatter: &mut fmt::Formatter<'_>, lines: &[impl fmt::Display]) -> fmt::Result {
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            formatter.write_str("\n")?;
        }
        write!(formatter, "{line}")?;
    }
    Ok(())
}

impl std::error::Error for Error {}
