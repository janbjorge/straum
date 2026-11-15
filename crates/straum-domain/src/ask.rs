//! An ask: the request to `POST /ask`, parsed into plain data, and the
//! checks it must pass against the spec before the store reads a cell.
//! `straum-notation` reads the query into an [`Ask`]; [`Spec::check_ask`]
//! finds the view it reads and lists every [`AskError`] at once.

use std::collections::HashSet;
use std::fmt;

use crate::identifier::Identifier;
use crate::spec::{Condition, Fact, Literal, Spec, Width};
use crate::value::ColumnType;

/// One `SELECT` over a view: its measures, the keys it groups by, its
/// filter and its time range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub view: Identifier,
    pub measures: Vec<Identifier>,
    pub keys: Vec<AskKey>,
    /// Every condition must hold.
    pub filter: Vec<Condition>,
    /// The half-open range `[from, to)`, Unix seconds, on bucket edges.
    pub from: i64,
    pub to: i64,
}

/// A key an ask groups by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskKey {
    /// A key by the name the view gives it.
    Key(Identifier),
    /// The time bucket.
    Bucket,
}

impl Spec {
    /// The view `ask` reads, once every check passes.
    pub fn check_ask(&self, ask: &Ask) -> Result<&Fact, Vec<AskError>> {
        let Some(fact) = self.fact(ask.view.as_str()) else {
            return Err(vec![AskError::UnknownView(ask.view.clone())]);
        };
        let mut errors = Vec::new();
        check_measures(fact, ask, &mut errors);
        check_keys(fact, ask, &mut errors);
        check_names(ask, &mut errors);
        for condition in &ask.filter {
            if fact.resolve(&condition.key).is_none() {
                errors.push(AskError::UnknownKey(condition.key.clone()));
            }
        }
        check_range(fact.time.width, ask, &mut errors);
        if errors.is_empty() {
            Ok(fact)
        } else {
            Err(errors)
        }
    }
}

fn check_measures(fact: &Fact, ask: &Ask, errors: &mut Vec<AskError>) {
    if ask.measures.is_empty() {
        errors.push(AskError::NoMeasures);
    }
    for measure in &ask.measures {
        if fact.measure(measure.as_str()).is_none() {
            errors.push(AskError::UnknownMeasure(measure.clone()));
        }
    }
}

fn check_keys(fact: &Fact, ask: &Ask, errors: &mut Vec<AskError>) {
    let mut bucket = false;
    for key in &ask.keys {
        match key {
            AskKey::Key(name) => {
                if fact.resolve(name).is_none() {
                    errors.push(AskError::UnknownKey(name.clone()));
                }
            }
            AskKey::Bucket => {
                if bucket {
                    errors.push(AskError::TimeKeyTwice);
                }
                bucket = true;
            }
        }
    }
}

/// An answer's columns go by name, so two can't share one.
fn check_names(ask: &Ask, errors: &mut Vec<AskError>) {
    let mut asked: Vec<&str> = Vec::new();
    for key in &ask.keys {
        if let AskKey::Key(name) = key {
            asked.push(name.as_str());
        }
    }
    for measure in &ask.measures {
        asked.push(measure.as_str());
    }
    let mut names = HashSet::new();
    for name in asked {
        if !names.insert(name) {
            errors.push(AskError::AskedTwice(name.to_owned()));
        }
    }
}

fn check_range(width: Width, ask: &Ask, errors: &mut Vec<AskError>) {
    if ask.from >= ask.to {
        errors.push(AskError::RangeEmpty {
            from: ask.from,
            to: ask.to,
        });
    }
    for at in [ask.from, ask.to] {
        if at.rem_euclid(i64::from(width.seconds)) != 0 {
            errors.push(AskError::RangeUnaligned { at, width });
        }
    }
}

/// One thing wrong with an ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskError {
    UnknownView(Identifier),
    UnknownMeasure(Identifier),
    /// A name that is no key of the view: not a `by` key, not an
    /// attribute a ref reads.
    UnknownKey(Identifier),
    NoMeasures,
    TimeKeyTwice,
    /// A key or measure named twice, or a key and a measure of one name.
    AskedTwice(String),
    /// A filter value not of its column's type, which would match nothing.
    WrongType {
        key: Identifier,
        column: ColumnType,
        value: Literal,
    },
    RangeEmpty {
        from: i64,
        to: i64,
    },
    /// A range end falls inside a bucket. Asks never read raw rows.
    RangeUnaligned {
        at: i64,
        width: Width,
    },
}

impl fmt::Display for AskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AskError::UnknownView(name) => write!(f, "unknown view {name}"),
            AskError::UnknownMeasure(name) => write!(f, "unknown measure {name}"),
            AskError::UnknownKey(name) => write!(f, "{name} is not a key of the view"),
            AskError::NoMeasures => write!(f, "no measures asked"),
            AskError::TimeKeyTwice => write!(f, "more than one time key"),
            AskError::AskedTwice(name) => write!(f, "{name} asked twice"),
            AskError::WrongType { key, column, value } => {
                let value = match value {
                    Literal::Int(integer) => format!("the integer {integer}"),
                    Literal::Text(text) => format!("the text {text:?}"),
                    Literal::Bool(boolean) => format!("the boolean {boolean}"),
                };
                write!(f, "{key} is {column}, so it can't be {value}")
            }
            AskError::RangeEmpty { from, to } => write!(f, "empty range [{from}, {to})"),
            AskError::RangeUnaligned { at, width } => {
                write!(f, "bound {at} is not on a {width} bucket edge")
            }
        }
    }
}

impl std::error::Error for AskError {}

/// As an answer names its column.
impl fmt::Display for AskKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AskKey::Key(name) => write!(f, "{name}"),
            AskKey::Bucket => f.write_str("bucket"),
        }
    }
}
