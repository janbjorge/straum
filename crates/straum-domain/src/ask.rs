use std::fmt;

use crate::ident::Ident;
use crate::spec::{Cond, Fact, Grain, KeyPath, Spec};

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
                        errors.push(AskError::UnknownKey(path.clone()));
                    }
                }
                AskKey::Time(t) => {
                    if time.replace(*t).is_some() {
                        errors.push(AskError::TimeKeyTwice);
                    }
                }
            }
        }
        for cond in &ask.filter {
            if self.resolve(fact, &cond.path).is_none() {
                errors.push(AskError::UnknownKey(cond.path.clone()));
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
    UnknownKey(KeyPath),
    NoMeasures,
    TimeKeyTwice,
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
            AskError::NoMeasures => write!(f, "no measures asked"),
            AskError::TimeKeyTwice => write!(f, "more than one time key"),
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
