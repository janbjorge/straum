use std::collections::HashSet;
use std::fmt;

use crate::ident::Ident;
use crate::spec::{Cond, Fact, Grain, Literal, Spec};
use crate::value::ColumnType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub fact: Ident,
    pub measures: Vec<Ident>,
    pub by: Vec<AskKey>,
    /// Every condition must hold.
    pub filter: Vec<Cond>,
    /// The half-open range `[from, to)`, Unix seconds, on bucket edges.
    pub from: i64,
    pub to: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskKey {
    /// A key by the name the view gives it.
    Key(Ident),
    Time(TimeKey),
}

/// Buckets are UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeKey {
    Bucket,
}

impl TimeKey {
    pub fn start(self, t: i64, grain: Grain) -> i64 {
        match self {
            TimeKey::Bucket => t.div_euclid(i64::from(grain.0)) * i64::from(grain.0),
        }
    }
}

impl Spec {
    pub fn check(&self, ask: &Ask) -> Result<&Fact, Vec<AskError>> {
        let Some(fact) = self.fact(ask.fact.as_str()) else {
            return Err(vec![AskError::UnknownFact(ask.fact.clone())]);
        };
        let grain = fact.time.grain;
        let mut errors = Vec::new();

        if ask.measures.is_empty() {
            errors.push(AskError::NoMeasures);
        }
        for m in &ask.measures {
            if fact.measure(m.as_str()).is_none() {
                errors.push(AskError::UnknownMeasure(m.clone()));
            }
        }

        let mut time = None;
        for key in &ask.by {
            match key {
                AskKey::Key(name) => {
                    if fact.resolve(name).is_none() {
                        errors.push(AskError::UnknownKey(name.clone()));
                    }
                }
                AskKey::Time(t) => {
                    if time.replace(*t).is_some() {
                        errors.push(AskError::TimeKeyTwice);
                    }
                }
            }
        }
        // An answer's columns go by name, so two can't share one.
        let mut names = HashSet::new();
        let keys = (ask.by.iter()).filter_map(|key| match key {
            AskKey::Key(name) => Some(name.as_str()),
            AskKey::Time(_) => None,
        });
        for name in keys.chain(ask.measures.iter().map(Ident::as_str)) {
            if !names.insert(name) {
                errors.push(AskError::AskedTwice(name.to_owned()));
            }
        }
        for cond in &ask.filter {
            if fact.resolve(&cond.key).is_none() {
                errors.push(AskError::UnknownKey(cond.key.clone()));
            }
        }

        if ask.from >= ask.to {
            errors.push(AskError::RangeEmpty {
                from: ask.from,
                to: ask.to,
            });
        }
        for at in [ask.from, ask.to] {
            if at.rem_euclid(i64::from(grain.0)) != 0 {
                errors.push(AskError::RangeUnaligned { at, grain });
            }
        }

        if errors.is_empty() {
            Ok(fact)
        } else {
            Err(errors)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskError {
    UnknownFact(Ident),
    UnknownMeasure(Ident),
    /// A name that is no key of the view: not a `by` key, not an
    /// attribute a ref reads.
    UnknownKey(Ident),
    NoMeasures,
    TimeKeyTwice,
    /// A key or measure named twice, or a key and a measure of one name.
    AskedTwice(String),
    /// A filter value not of its column's type, which would match nothing.
    WrongType {
        key: Ident,
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
        grain: Grain,
    },
}

impl fmt::Display for AskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AskError::UnknownFact(name) => write!(f, "unknown fact {name}"),
            AskError::UnknownMeasure(name) => write!(f, "unknown measure {name}"),
            AskError::UnknownKey(name) => write!(f, "{name} is not a key of the view"),
            AskError::NoMeasures => write!(f, "no measures asked"),
            AskError::TimeKeyTwice => write!(f, "more than one time key"),
            AskError::AskedTwice(name) => write!(f, "{name} asked twice"),
            AskError::WrongType { key, column, value } => {
                let value = match value {
                    Literal::Int(n) => format!("the integer {n}"),
                    Literal::Text(s) => format!("the text {s:?}"),
                    Literal::Bool(b) => format!("the boolean {b}"),
                };
                write!(f, "{key} is {column}, so it can't be {value}")
            }
            AskError::RangeEmpty { from, to } => write!(f, "empty range [{from}, {to})"),
            AskError::RangeUnaligned { at, grain } => {
                write!(f, "range end {at} is not on a {grain} bucket edge")
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
            AskKey::Time(t) => write!(f, "{t}"),
        }
    }
}

impl fmt::Display for TimeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TimeKey::Bucket => "bucket",
        })
    }
}
