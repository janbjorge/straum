//! Facts, the dims they point at, and their named measures. Built once from
//! the spec, checked, then read-only.

use std::collections::HashSet;
use std::fmt;
use std::iter;

use crate::ident::Ident;

/// Seconds per day. Every grain divides it, so buckets roll up to days.
const DAY: u32 = 86_400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub schema: Ident,
    pub name: Ident,
}

/// A lookup table facts point at. Its attributes are read after aggregation,
/// so a change to a dim row never touches a rollup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dim {
    pub name: Ident,
    pub table: Table,
    pub key: Ident,
    pub attrs: Vec<Ident>,
}

/// A table whose rows are aggregated into one rollup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    pub name: Ident,
    pub table: Table,
    pub key: Ident,
    pub time: Time,
    pub refs: Vec<Ref>,
    /// Fact columns that are part of the rollup key.
    pub by: Vec<Ident>,
    pub measures: Vec<Measure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Time {
    pub column: Ident,
    pub grain: Grain,
}

/// Bucket width in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grain(pub u32);

/// N:1 pointer from a fact column to a dim key. The only way a fact reaches
/// another table, so fan-out can't be declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub name: Ident,
    pub dim: Ident,
    pub via: Ident,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measure {
    pub name: Ident,
    pub agg: Agg,
    /// A row counts only when every condition holds.
    pub filter: Vec<Cond>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Agg {
    Count,
    Sum(Ident),
    Min(Ident),
    Max(Ident),
    Avg(Ident),
    CountDistinct(Ident),
}

impl Agg {
    /// The column the measure reads; `None` for count.
    pub fn input(&self) -> Option<&Ident> {
        match self {
            Agg::Count => None,
            Agg::Sum(c) | Agg::Min(c) | Agg::Max(c) | Agg::Avg(c) | Agg::CountDistinct(c) => {
                Some(c)
            }
        }
    }
}

/// `path = value`, or `path in (values)` with more than one value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cond {
    pub path: KeyPath,
    pub values: Vec<Literal>,
}

/// `column`, `ref` or `ref.attr`; see [`Spec::resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPath {
    pub head: Ident,
    pub attr: Option<Ident>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    Int(i64),
    Text(String),
    Bool(bool),
}

/// What a [`KeyPath`] names on a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key<'a> {
    /// A `by` column.
    Column(&'a Ident),
    /// The dim row a ref points at.
    Ref(&'a Ref),
    /// An attribute of that dim row, mapped after aggregation.
    Attr(&'a Ref, &'a Ident),
}

/// One component of a rollup key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot<'a> {
    Column(&'a Ident),
    Ref(&'a Ref),
    Bucket,
}

impl Fact {
    /// `by` columns, then ref ids, then the time bucket. Dim attributes are
    /// never part of it.
    pub fn rollup_key(&self) -> impl Iterator<Item = Slot<'_>> {
        self.by
            .iter()
            .map(Slot::Column)
            .chain(self.refs.iter().map(Slot::Ref))
            .chain(iter::once(Slot::Bucket))
    }

    /// The columns a row of this fact carries, in order: the time column,
    /// the `by` columns and ref columns in rollup-key order, then the
    /// measure inputs not already among them.
    pub fn columns(&self) -> Vec<Ident> {
        let mut columns: Vec<Ident> = iter::once(&self.time.column)
            .chain(&self.by)
            .chain(self.refs.iter().map(|r| &r.via))
            .cloned()
            .collect();
        for m in &self.measures {
            if let Some(column) = m.agg.input()
                && !columns.contains(column)
            {
                columns.push(column.clone());
            }
        }
        columns
    }

    pub fn measure(&self, name: &str) -> Option<&Measure> {
        self.measures.iter().find(|m| m.name.as_str() == name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    dims: Vec<Dim>,
    facts: Vec<Fact>,
}

impl Spec {
    /// Checks the declarations against each other and returns every problem
    /// found. Column types and keys are checked against PostgreSQL elsewhere.
    pub fn new(dims: Vec<Dim>, facts: Vec<Fact>) -> Result<Spec, Vec<SpecError>> {
        let mut errors = Vec::new();

        let mut names = HashSet::new();
        for name in dims
            .iter()
            .map(|d| &d.name)
            .chain(facts.iter().map(|f| &f.name))
        {
            if !names.insert(name) {
                errors.push(SpecError::Duplicate {
                    scope: None,
                    name: name.clone(),
                });
            }
        }
        for dim in &dims {
            check_dim(dim, &mut errors);
        }
        for fact in &facts {
            check_fact(fact, &dims, &mut errors);
        }

        if errors.is_empty() {
            Ok(Spec { dims, facts })
        } else {
            Err(errors)
        }
    }

    /// In declaration order.
    pub fn dims(&self) -> &[Dim] {
        &self.dims
    }

    /// In declaration order.
    pub fn facts(&self) -> &[Fact] {
        &self.facts
    }

    pub fn dim(&self, name: &str) -> Option<&Dim> {
        self.dims.iter().find(|d| d.name.as_str() == name)
    }

    pub fn fact(&self, name: &str) -> Option<&Fact> {
        self.facts.iter().find(|f| f.name.as_str() == name)
    }

    /// A bare name is a `by` column or a ref; `ref.attr` is the dim's key or
    /// one of its attributes.
    pub fn resolve<'a>(&'a self, fact: &'a Fact, path: &KeyPath) -> Option<Key<'a>> {
        resolve(&self.dims, fact, path)
    }
}

fn resolve<'a>(dims: &'a [Dim], fact: &'a Fact, path: &KeyPath) -> Option<Key<'a>> {
    let Some(attr) = &path.attr else {
        if let Some(column) = fact.by.iter().find(|c| **c == path.head) {
            return Some(Key::Column(column));
        }
        return fact.refs.iter().find(|r| r.name == path.head).map(Key::Ref);
    };
    let r = fact.refs.iter().find(|r| r.name == path.head)?;
    let dim = dims.iter().find(|d| d.name == r.dim)?;
    let attr = iter::once(&dim.key)
        .chain(&dim.attrs)
        .find(|a| *a == attr)?;
    Some(Key::Attr(r, attr))
}

fn check_dim(dim: &Dim, errors: &mut Vec<SpecError>) {
    let mut columns = HashSet::new();
    for column in iter::once(&dim.key).chain(&dim.attrs) {
        if !columns.insert(column) {
            errors.push(SpecError::Duplicate {
                scope: Some(dim.name.clone()),
                name: column.clone(),
            });
        }
    }
}

fn check_fact(fact: &Fact, dims: &[Dim], errors: &mut Vec<SpecError>) {
    let name = &fact.name;

    let Grain(seconds) = fact.time.grain;
    if seconds == 0 || !DAY.is_multiple_of(seconds) {
        errors.push(SpecError::BadGrain {
            fact: name.clone(),
            seconds,
        });
    }

    // Each fact column plays at most one role in the rollup key.
    let mut columns = HashSet::new();
    let roles = iter::once(&fact.time.column)
        .chain(&fact.by)
        .chain(fact.refs.iter().map(|r| &r.via));
    for column in roles {
        if *column == fact.key {
            errors.push(SpecError::KeyInRollup {
                fact: name.clone(),
                column: column.clone(),
            });
        } else if !columns.insert(column) {
            errors.push(SpecError::ColumnReused {
                fact: name.clone(),
                column: column.clone(),
            });
        }
    }

    // `by` columns and ref names share one namespace in key paths.
    let mut keys: HashSet<&Ident> = fact.by.iter().collect();
    for r in &fact.refs {
        if !keys.insert(&r.name) {
            errors.push(SpecError::Duplicate {
                scope: Some(name.clone()),
                name: r.name.clone(),
            });
        }
        if !dims.iter().any(|d| d.name == r.dim) {
            errors.push(SpecError::UnknownDim {
                fact: name.clone(),
                ref_name: r.name.clone(),
                dim: r.dim.clone(),
            });
        }
    }

    if fact.measures.is_empty() {
        errors.push(SpecError::NoMeasures { fact: name.clone() });
    }
    let mut measures = HashSet::new();
    for m in &fact.measures {
        if !measures.insert(&m.name) {
            errors.push(SpecError::Duplicate {
                scope: Some(name.clone()),
                name: m.name.clone(),
            });
        }
        for cond in &m.filter {
            if resolve(dims, fact, &cond.path).is_none() {
                errors.push(SpecError::UnknownKey {
                    fact: name.clone(),
                    measure: m.name.clone(),
                    path: cond.path.clone(),
                });
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecError {
    /// Two declarations share a name; `scope` is the enclosing dim or fact.
    Duplicate {
        scope: Option<Ident>,
        name: Ident,
    },
    BadGrain {
        fact: Ident,
        seconds: u32,
    },
    /// The fact's own key would be a rollup key, so nothing aggregates.
    KeyInRollup {
        fact: Ident,
        column: Ident,
    },
    /// A column is used twice among `time`, `by` and ref `via`.
    ColumnReused {
        fact: Ident,
        column: Ident,
    },
    UnknownDim {
        fact: Ident,
        ref_name: Ident,
        dim: Ident,
    },
    /// A measure filter names something that is not in the rollup key.
    UnknownKey {
        fact: Ident,
        measure: Ident,
        path: KeyPath,
    },
    NoMeasures {
        fact: Ident,
    },
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpecError::Duplicate { scope: None, name } => write!(f, "{name} declared twice"),
            SpecError::Duplicate {
                scope: Some(scope),
                name,
            } => {
                write!(f, "{scope}: {name} declared twice")
            }
            SpecError::BadGrain { fact, seconds } => {
                write!(f, "fact {fact}: grain of {seconds}s does not divide a day")
            }
            SpecError::KeyInRollup { fact, column } => {
                write!(
                    f,
                    "fact {fact}: key column {column} can't be part of the rollup key"
                )
            }
            SpecError::ColumnReused { fact, column } => {
                write!(
                    f,
                    "fact {fact}: column {column} used twice among time, by and ref"
                )
            }
            SpecError::UnknownDim {
                fact,
                ref_name,
                dim,
            } => {
                write!(f, "fact {fact}: ref {ref_name} points at unknown dim {dim}")
            }
            SpecError::UnknownKey {
                fact,
                measure,
                path,
            } => write!(
                f,
                "fact {fact}: measure {measure} filters on {path}, \
                 which is not a by column, ref or ref attribute"
            ),
            SpecError::NoMeasures { fact } => write!(f, "fact {fact}: no measures"),
        }
    }
}

impl std::error::Error for SpecError {}

impl fmt::Display for Grain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}s", self.0)
    }
}

impl fmt::Display for KeyPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.attr {
            None => write!(f, "{}", self.head),
            Some(attr) => write!(f, "{}.{attr}", self.head),
        }
    }
}
