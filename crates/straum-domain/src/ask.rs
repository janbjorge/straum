use std::collections::HashSet;
use std::fmt;

use crate::ident::Ident;
use crate::spec::{Binding, Cond, Fact, Grain, KeyPath, Literal, Spec};
use crate::value::ColumnType;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub fact: Ident,
    pub measures: Vec<Ident>,
    pub by: Vec<AskKey>,
    /// Every condition must hold.
    pub filter: Vec<Cond>,
    /// Start of the half-open range `[from, to)`, Unix seconds.
    pub from: i64,
    pub to: i64,
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
                    if self.resolve(fact, path).is_none() {
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
            if self.resolve(fact, &cond.path).is_none() {
                errors.push(unknown(fact, &cond.path));
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

/// Why a path resolved to nothing: a ref on several columns or one bound
/// at ingest has no value of its own, and a ref bound at ingest has only
/// the attributes it bound, so each is named by what it has.
fn unknown(fact: &Fact, path: &KeyPath) -> AskError {
    let Some(r) = fact.refs.iter().find(|r| r.name == path.head) else {
        return AskError::UnknownKey(path.clone());
    };
    match (&r.binding, &path.attr) {
        (Binding::Early(attrs), None) => AskError::EarlyRef {
            name: r.name.clone(),
            attrs: attrs.clone(),
        },
        (Binding::Early(attrs), Some(attr)) => AskError::Unbound {
            name: r.name.clone(),
            attr: attr.clone(),
            attrs: attrs.clone(),
        },
        (Binding::Late, None) if r.via.len() > 1 => AskError::CompositeRef {
            name: r.name.clone(),
            columns: r.via.clone(),
        },
        _ => AskError::UnknownKey(path.clone()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskError {
    UnknownFact(Ident),
    UnknownMeasure(Ident),
    UnknownKey(KeyPath),
    /// A ref on several columns asked for as a key.
    CompositeRef {
        name: Ident,
        columns: Vec<Ident>,
    },
    /// A ref bound at ingest asked for as a key.
    EarlyRef {
        name: Ident,
        attrs: Vec<Ident>,
    },
    /// An attribute of a ref bound at ingest that the view did not bind.
    Unbound {
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

impl fmt::Display for AskError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AskError::UnknownFact(name) => write!(f, "unknown fact {name}"),
            AskError::UnknownMeasure(name) => write!(f, "unknown measure {name}"),
            AskError::UnknownKey(path) => {
                write!(f, "{path} is not a by column, ref or ref attribute")
            }
            AskError::CompositeRef { name, columns } => {
                let columns: Vec<String> = columns.iter().map(ToString::to_string).collect();
                write!(
                    f,
                    "{name} joins on {}; ask for those columns or for {name}.<attribute>",
                    columns.join(", ")
                )
            }
            AskError::EarlyRef { name, attrs } => {
                let attrs: Vec<String> = attrs.iter().map(|a| format!("{name}.{a}")).collect();
                write!(
                    f,
                    "{name} binds its attributes at ingest and has no id; ask for {}",
                    attrs.join(", ")
                )
            }
            AskError::Unbound { name, attr, attrs } => {
                let attrs: Vec<String> = attrs.iter().map(|a| format!("{name}.{a}")).collect();
                write!(
                    f,
                    "{name}.{attr} is not bound at ingest; the view binds {}",
                    attrs.join(", ")
                )
            }
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
