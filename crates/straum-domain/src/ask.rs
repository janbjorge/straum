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
use crate::spec::{Condition, Fact, Literal, Spec};
use crate::value::{ColumnType, MAX_SCALE};
use crate::width::Width;

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
    /// The name the range bounds compare, which must be the view's time;
    /// `None` when neither end is written.
    pub bounded: Option<Identifier>,
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

/// A key an ask groups by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskKey {
    /// A key by the name the view gives it; the view's time is one, at
    /// its narrowest width.
    Key(Identifier),
    /// The view's time at a wider width it keeps, as
    /// `buckets(hour, '1 day')` writes it: each row one stored bucket.
    WiderBucket { name: Identifier, width: Width },
}

impl AskKey {
    /// The name the key reads.
    pub fn name(&self) -> &Identifier {
        match self {
            AskKey::Key(name) | AskKey::WiderBucket { name, .. } => name,
        }
    }
}

impl Fact {
    /// The width `key` groups the view's time by; `None` for a key that
    /// is not the time.
    pub fn time_width(&self, key: &AskKey) -> Option<Width> {
        match key {
            AskKey::Key(name) if *name == self.time.name => Some(self.time.width),
            AskKey::Key(_) => None,
            AskKey::WiderBucket { width, .. } => Some(*width),
        }
    }
}

impl Ask {
    /// The width of the buckets the ask answers by: the one it names in
    /// `buckets`, else the narrowest width, the view's own. `None` when
    /// `buckets` names a width `fact` does not keep, or reads something
    /// other than its time: such a width has no stored grid, and may be
    /// none at all, so no bound can be on one of its edges.
    pub fn row_width(&self, fact: &Fact) -> Option<Width> {
        for key in &self.keys {
            let AskKey::WiderBucket { name, width } = key else {
                continue;
            };
            if *name == fact.time.name && fact.time.wider.contains(width) {
                return Some(*width);
            }
            return None;
        }
        Some(fact.time.width)
    }
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
        check_names(fact, ask, &mut errors);
        for condition in &ask.filter {
            let name = &condition.key;
            if fact.resolve(name).is_some() {
                continue;
            }
            if *name == fact.time.name {
                errors.push(AskError::TimeInWhere(name.clone()));
            } else if fact.measure(name.as_str()).is_some() {
                errors.push(AskError::MeasureInWhere(name.clone()));
            } else {
                errors.push(unknown_key(fact, name));
            }
        }
        for condition in &ask.having {
            check_having(fact, ask, condition, &mut errors);
        }
        if let Some(name) = &ask.bounded {
            check_is_time(fact, name, &mut errors);
        }
        check_range(ask.row_width(fact), ask, &mut errors);
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
        if fact.resolve(measure).is_some() || *measure == fact.time.name {
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
            AskKey::Key(name) if *name == fact.time.name => {}
            AskKey::Key(name) => {
                if fact.resolve(name).is_none() {
                    errors.push(unknown_key(fact, name));
                }
                continue;
            }
            AskKey::WiderBucket { name, width } => {
                if *name != fact.time.name {
                    errors.push(AskError::NotTime {
                        name: name.clone(),
                        time: fact.time.name.clone(),
                    });
                    continue;
                }
                check_row_width(fact, *width, errors);
            }
        }
        if bucket {
            errors.push(AskError::TimeKeyTwice);
        }
        bucket = true;
    }
}

/// A wider width and the range bounds read the view's time and nothing
/// else.
fn check_is_time(fact: &Fact, name: &Identifier, errors: &mut Vec<AskError>) {
    if *name != fact.time.name {
        errors.push(AskError::NotTime {
            name: name.clone(),
            time: fact.time.name.clone(),
        });
    }
}

/// A row width must be one the view keeps, so each row is one stored
/// bucket: a width built from narrower buckets would read as many cells
/// as asking by those and adding them up.
fn check_row_width(fact: &Fact, width: Width, errors: &mut Vec<AskError>) {
    if width == fact.time.width {
        errors.push(AskError::OwnWidth {
            name: fact.time.name.clone(),
            width,
        });
        return;
    }
    if fact.time.wider.contains(&width) {
        return;
    }
    let mut kept = vec![fact.time.width];
    kept.extend(&fact.time.wider);
    errors.push(AskError::WidthNotKept { width, kept });
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

/// The view's time is a key an ask may name too.
fn unknown_key(fact: &Fact, name: &Identifier) -> AskError {
    let mut keys = vec![&fact.time.name];
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
        if fact.resolve(name).is_some() || *name == fact.time.name {
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

/// An answer's columns go by name, so two can't share one. The time
/// twice is [`AskError::TimeKeyTwice`].
fn check_names(fact: &Fact, ask: &Ask, errors: &mut Vec<AskError>) {
    let mut asked: Vec<&str> = Vec::new();
    for key in &ask.keys {
        if fact.time_width(key).is_none() {
            asked.push(key.name().as_str());
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

/// A bound is checked against the edges of `width` only when the view
/// keeps it; a refused width has its own error, and advice on its edges
/// would point at buckets nothing stores.
fn check_range(width: Option<Width>, ask: &Ask, errors: &mut Vec<AskError>) {
    if let (Some(from), Some(to)) = (ask.from, ask.to)
        && from >= to
    {
        errors.push(AskError::RangeEmpty { from, to });
    }
    let Some(width) = width else {
        return;
    };
    for end in [ask.from, ask.to] {
        let Some(at) = end else {
            continue;
        };
        if !width.is_edge(at) {
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
    /// A wider width that is the view's own, which the time's name alone
    /// says.
    OwnWidth {
        name: Identifier,
        width: Width,
    },
    /// A wider width or a range bound on a name that is not the view's
    /// time.
    NotTime {
        name: Identifier,
        time: Identifier,
    },
    /// The view's time in WHERE, which bounds it with `>=` and `<`.
    TimeInWhere(Identifier),
    /// A wider width the view doesn't keep, beside the ones it does.
    WidthNotKept {
        width: Width,
        kept: Vec<Width>,
    },
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
    /// A range end falls inside a bucket of the ask's row width. Asks
    /// never read raw rows, and a row covers a whole bucket.
    RangeUnaligned {
        at: i64,
        width: Width,
    },
}

/// As an answer names its column.
impl fmt::Display for AskKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AskKey::Key(_) | AskKey::WiderBucket { .. } => write!(f, "{}", self.name()),
        }
    }
}
