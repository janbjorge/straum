//! An ask: the request to `POST /ask`, parsed into plain data, and the
//! checks it must pass against the spec before the store reads a cell.
//! `straum-notation` reads the query into an [`Ask`]; [`Spec::check_ask`]
//! finds the view it reads and lists every [`AskError`] at once.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::fmt;

use crate::aggregate::Answers;
use crate::closest::closest;
use crate::identifier::Identifier;
use crate::spec::{Condition, Fact, Literal, Spec, Width};
use crate::value::{ColumnType, MAX_SCALE, decimal};

/// One `SELECT` over a view: its measures, the keys it groups by, its
/// filter and its time range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ask {
    pub view: Identifier,
    pub measures: Vec<Identifier>,
    pub keys: Vec<AskKey>,
    /// Every condition must hold of a row.
    pub filter: Vec<Condition>,
    /// Every condition must hold of a group for it to be answered.
    pub having: Vec<MeasureCondition>,
    /// The first bucket read, in Unix seconds on a bucket edge; `None`
    /// reads from the first bucket held.
    pub from: Option<i64>,
    /// The end of the range, not read, in Unix seconds on a bucket edge;
    /// `None` reads to the end of the last bucket held.
    pub to: Option<i64>,
}

/// `measure op value` in HAVING: a measure the ask selects, compared with
/// a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasureCondition {
    pub measure: Identifier,
    pub comparison: Comparison,
    pub value: Literal,
}

/// How HAVING compares a measure with its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

impl Comparison {
    /// Whether a measure that orders `order` against the value meets it.
    pub fn holds(self, order: Ordering) -> bool {
        match self {
            Comparison::Equal => order == Ordering::Equal,
            Comparison::NotEqual => order != Ordering::Equal,
            Comparison::Less => order == Ordering::Less,
            Comparison::LessOrEqual => order != Ordering::Greater,
            Comparison::Greater => order == Ordering::Greater,
            Comparison::GreaterOrEqual => order != Ordering::Less,
        }
    }
}

/// As the ask writes it.
impl fmt::Display for Comparison {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Comparison::Equal => "=",
            Comparison::NotEqual => "<>",
            Comparison::Less => "<",
            Comparison::LessOrEqual => "<=",
            Comparison::Greater => ">",
            Comparison::GreaterOrEqual => ">=",
        })
    }
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
            let mut views = Vec::new();
            for fact in self.facts() {
                views.push(&fact.name);
            }
            let instead = Instead::of(&ask.view, views);
            return Err(vec![AskError::UnknownView {
                name: ask.view.clone(),
                instead,
            }]);
        };
        let mut errors = Vec::new();
        check_measures(fact, ask, &mut errors);
        check_keys(fact, ask, &mut errors);
        check_names(ask, &mut errors);
        for condition in &ask.filter {
            let name = &condition.key;
            if fact.resolve(name).is_some() {
                continue;
            }
            if fact.measure(name.as_str()).is_some() {
                errors.push(AskError::MeasureInWhere(name.clone()));
            } else {
                errors.push(unknown_key(fact, name));
            }
        }
        for condition in &ask.having {
            check_having(fact, ask, condition, &mut errors);
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
        if fact.measure(measure.as_str()).is_some() {
            continue;
        }
        // A name SELECT lists but GROUP BY doesn't reads as a measure.
        if fact.resolve(measure).is_some() {
            errors.push(AskError::KeyNotGrouped(measure.clone()));
        } else {
            errors.push(unknown_measure(fact, measure));
        }
    }
}

fn check_keys(fact: &Fact, ask: &Ask, errors: &mut Vec<AskError>) {
    let mut bucket = false;
    for key in &ask.keys {
        match key {
            AskKey::Key(name) => {
                if fact.resolve(name).is_none() {
                    errors.push(unknown_key(fact, name));
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

fn unknown_measure(fact: &Fact, name: &Identifier) -> AskError {
    let mut measures = Vec::new();
    for measure in &fact.measures {
        measures.push(&measure.name);
    }
    AskError::UnknownMeasure {
        name: name.clone(),
        instead: Instead::of(name, measures),
    }
}

/// The time bucket is a key an ask may name too.
fn unknown_key(fact: &Fact, name: &Identifier) -> AskError {
    let bucket = Identifier::new("bucket").expect("bucket is an identifier");
    let mut keys = vec![&bucket];
    keys.extend(fact.keys());
    AskError::UnknownKey {
        name: name.clone(),
        instead: Instead::of(name, keys),
    }
}

/// A HAVING condition compares a measure the ask selects, so the answer
/// shows each value a group was kept for, with a value of its kind.
fn check_having(fact: &Fact, ask: &Ask, condition: &MeasureCondition, errors: &mut Vec<AskError>) {
    let name = &condition.measure;
    let Some(measure) = fact.measure(name.as_str()) else {
        if fact.resolve(name).is_some() {
            errors.push(AskError::KeyInHaving(name.clone()));
        } else {
            errors.push(unknown_measure(fact, name));
        }
        return;
    };
    if !ask.measures.contains(name) {
        errors.push(AskError::HavingNotSelected(name.clone()));
    }
    let wrong = match (measure.aggregate.answers(), &condition.value) {
        (Answers::Numbers, Literal::Int(_)) => None,
        (Answers::Numbers, Literal::Decimal { scale, .. }) if *scale <= MAX_SCALE => None,
        (Answers::Numbers, Literal::Decimal { .. }) => Some(AskError::TooManyDigits {
            measure: name.clone(),
            value: condition.value.clone(),
        }),
        (Answers::Booleans, Literal::Bool(_)) => match condition.comparison {
            Comparison::Equal | Comparison::NotEqual => None,
            comparison => Some(AskError::BooleanOrder {
                measure: name.clone(),
                comparison,
            }),
        },
        (Answers::Numbers, _) => Some(AskError::NotANumber {
            measure: name.clone(),
            value: condition.value.clone(),
        }),
        (Answers::Booleans, _) => Some(AskError::NotABoolean {
            measure: name.clone(),
            value: condition.value.clone(),
        }),
    };
    errors.extend(wrong);
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
    if let (Some(from), Some(to)) = (ask.from, ask.to)
        && from >= to
    {
        errors.push(AskError::RangeEmpty { from, to });
    }
    for end in [ask.from, ask.to] {
        let Some(at) = end else {
            continue;
        };
        if at.rem_euclid(i64::from(width.seconds)) != 0 {
            errors.push(AskError::RangeUnaligned { at, width });
        }
    }
}

/// What an error about a name nothing has offers in its place: the name
/// most likely meant, else every name there is, so the next try needn't
/// guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instead {
    Closest(Identifier),
    Every(Vec<Identifier>),
}

impl Instead {
    fn of(written: &Identifier, names: Vec<&Identifier>) -> Instead {
        let mut texts = Vec::with_capacity(names.len());
        for name in &names {
            texts.push(name.as_str());
        }
        if let Some(near) = closest(written.as_str(), &texts) {
            for name in &names {
                if name.as_str() == near {
                    return Instead::Closest((*name).clone());
                }
            }
        }
        let mut every = Vec::with_capacity(names.len());
        for name in names {
            every.push(name.clone());
        }
        Instead::Every(every)
    }

    /// `; did you mean x?`, or `; the {kind} are a, b`, after an error.
    fn write(&self, formatter: &mut fmt::Formatter<'_>, kind: &str) -> fmt::Result {
        match self {
            Instead::Closest(name) => write!(formatter, "; did you mean {name}?"),
            Instead::Every(names) if names.is_empty() => Ok(()),
            Instead::Every(names) => {
                let mut texts = Vec::with_capacity(names.len());
                for name in names {
                    texts.push(name.as_str());
                }
                write!(formatter, "; the {kind} are {}", texts.join(", "))
            }
        }
    }
}

/// One thing wrong with an ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskError {
    UnknownView {
        name: Identifier,
        instead: Instead,
    },
    UnknownMeasure {
        name: Identifier,
        instead: Instead,
    },
    /// A name that is no key of the view: not a `by` key, not an
    /// attribute a ref reads.
    UnknownKey {
        name: Identifier,
        instead: Instead,
    },
    /// A key the ask selects without grouping by it.
    KeyNotGrouped(Identifier),
    NoMeasures,
    TimeKeyTwice,
    /// A key or measure named twice, or a key and a measure of one name.
    AskedTwice(String),
    /// A measure in WHERE, which filters rows; HAVING compares measures.
    MeasureInWhere(Identifier),
    /// A key in HAVING, which compares measures; WHERE filters keys.
    KeyInHaving(Identifier),
    /// A HAVING measure the ask doesn't select.
    HavingNotSelected(Identifier),
    /// A number measure in HAVING against a value that is no number.
    NotANumber {
        measure: Identifier,
        value: Literal,
    },
    /// A `bool_and` or `bool_or` in HAVING against a value that is no
    /// boolean.
    NotABoolean {
        measure: Identifier,
        value: Literal,
    },
    /// A boolean measure in HAVING with an order, which booleans lack.
    BooleanOrder {
        measure: Identifier,
        comparison: Comparison,
    },
    /// A HAVING value with more digits after the point than a comparison
    /// keeps exactly.
    TooManyDigits {
        measure: Identifier,
        value: Literal,
    },
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
            AskError::UnknownView { name, instead } => {
                write!(f, "unknown view {name}")?;
                instead.write(f, "views")
            }
            AskError::UnknownMeasure { name, instead } => {
                write!(f, "unknown measure {name}")?;
                instead.write(f, "view's measures")
            }
            AskError::UnknownKey { name, instead } => {
                write!(f, "{name} is not a key of the view")?;
                instead.write(f, "view's keys")
            }
            AskError::KeyNotGrouped(name) => write!(f, "{name} is a key; add it to GROUP BY"),
            AskError::NoMeasures => write!(f, "no measures asked"),
            AskError::TimeKeyTwice => write!(f, "more than one time key"),
            AskError::AskedTwice(name) => write!(f, "{name} asked twice"),
            AskError::MeasureInWhere(name) => {
                write!(f, "{name} is a measure; compare it in HAVING")
            }
            AskError::KeyInHaving(name) => write!(f, "{name} is a key; filter it in WHERE"),
            AskError::HavingNotSelected(name) => {
                write!(f, "HAVING compares {name}, which the ask doesn't select")
            }
            AskError::NotANumber { measure, value } => {
                let value = described(value);
                write!(
                    f,
                    "{measure} is a number, so it can't be compared with {value}"
                )
            }
            AskError::NotABoolean { measure, value } => {
                let value = described(value);
                write!(
                    f,
                    "{measure} is a boolean, so it can't be compared with {value}"
                )
            }
            AskError::BooleanOrder {
                measure,
                comparison,
            } => write!(
                f,
                "{measure} is a boolean; compare it with = or <>, not {comparison}"
            ),
            AskError::TooManyDigits { measure, value } => {
                let value = described(value);
                write!(
                    f,
                    "{measure} can't be compared with {value}: \
                     at most {MAX_SCALE} digits after the point"
                )
            }
            AskError::WrongType { key, column, value } => {
                let value = described(value);
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

/// A value as an error names it.
fn described(value: &Literal) -> String {
    match value {
        Literal::Int(integer) => format!("the integer {integer}"),
        Literal::Text(text) => format!("the text {text:?}"),
        Literal::Bool(boolean) => format!("the boolean {boolean}"),
        Literal::Decimal { units, scale } => format!("the number {}", decimal(*units, *scale)),
    }
}

/// As an answer names its column.
impl fmt::Display for AskKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AskKey::Key(name) => write!(f, "{name}"),
            AskKey::Bucket => f.write_str("bucket"),
        }
    }
}
