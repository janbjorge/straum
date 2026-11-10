use std::collections::HashSet;
use std::fmt;

use crate::ident::Ident;
use crate::spec::{Cond, Fact, Grain, KeyPath, Literal, Spec};
use crate::value::ColumnType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub fact: Ident,
    pub measures: Vec<Ident>,
    pub by: Vec<AskKey>,
    /// Every condition must hold.
    pub filter: Vec<Cond>,
    /// The half-open range `[from, to)`, Unix seconds. An end not given
    /// is open: the ask reads every bucket on that side.
    pub from: Option<i64>,
    pub to: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskKey {
    Key(KeyPath),
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
                AskKey::Key(path) => {
                    if fact.resolve(path).is_none() {
                        errors.push(unknown(fact, path));
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
            AskKey::Key(path) => Some(path.to_string()),
            AskKey::Time(_) => None,
        });
        for name in keys.chain(ask.measures.iter().map(ToString::to_string)) {
            if !names.insert(name.clone()) {
                errors.push(AskError::AskedTwice(name));
            }
        }
        for cond in &ask.filter {
            if fact.resolve(&cond.path).is_none() {
                errors.push(unknown(fact, &cond.path));
            }
        }

        if let (Some(from), Some(to)) = (ask.from, ask.to)
            && from >= to
        {
            errors.push(AskError::RangeEmpty { from, to });
        }
        for at in [ask.from, ask.to].into_iter().flatten() {
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

/// Why a path resolved to nothing: a ref has no value of its own, and
/// reads only the attributes the view selects, so each is named by what
/// the ref has.
fn unknown(fact: &Fact, path: &KeyPath) -> AskError {
    let Some(r) = fact.refs.iter().find(|r| r.name == path.head) else {
        return AskError::UnknownKey(path.clone());
    };
    match &path.attr {
        None => AskError::RefAsKey {
            name: r.name.clone(),
            attrs: r.attrs.clone(),
        },
        Some(attr) => AskError::AttrNotRead {
            name: r.name.clone(),
            attr: attr.clone(),
            attrs: r.attrs.clone(),
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskError {
    UnknownFact(Ident),
    UnknownMeasure(Ident),
    UnknownKey(KeyPath),
    /// A ref asked for as a key.
    RefAsKey {
        name: Ident,
        attrs: Vec<Ident>,
    },
    /// An attribute of a ref that the view does not read.
    AttrNotRead {
        name: Ident,
        attr: Ident,
        attrs: Vec<Ident>,
    },
    NoMeasures,
    TimeKeyTwice,
    /// A key or measure named twice, or a key and a measure of one name.
    AskedTwice(String),
    /// A filter value not of its column's type, which would match nothing.
    WrongType {
        path: KeyPath,
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

fn attrs(name: &Ident, attrs: &[Ident]) -> String {
    let attrs: Vec<String> = attrs.iter().map(|a| format!("{name}.{a}")).collect();
    attrs.join(", ")
}

impl fmt::Display for AskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AskError::UnknownFact(name) => write!(f, "unknown fact {name}"),
            AskError::UnknownMeasure(name) => write!(f, "unknown measure {name}"),
            AskError::UnknownKey(path) => {
                write!(f, "{path} is not a by column or ref attribute")
            }
            AskError::RefAsKey { name, attrs: read } => write!(
                f,
                "{name} is a join and has no value of its own; ask for {}",
                attrs(name, read)
            ),
            AskError::AttrNotRead {
                name,
                attr,
                attrs: read,
            } => write!(
                f,
                "{name}.{attr} is not read by the view, which reads {}",
                attrs(name, read)
            ),
            AskError::NoMeasures => write!(f, "no measures asked"),
            AskError::TimeKeyTwice => write!(f, "more than one time key"),
            AskError::AskedTwice(name) => write!(f, "{name} asked twice"),
            AskError::WrongType {
                path,
                column,
                value,
            } => {
                let value = match value {
                    Literal::Int(n) => format!("the integer {n}"),
                    Literal::Text(s) => format!("the text {s:?}"),
                    Literal::Bool(b) => format!("the boolean {b}"),
                };
                write!(f, "{path} is {column}, so it can't be {value}")
            }
            AskError::RangeEmpty { from, to } => write!(f, "empty range [{from}, {to})"),
            AskError::RangeUnaligned { at, grain } => {
                write!(f, "range end {at} is not on a {grain} bucket edge")
            }
        }
    }
}

impl std::error::Error for AskError {}

impl fmt::Display for TimeKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            TimeKey::Bucket => "bucket",
        })
    }
}
