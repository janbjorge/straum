use std::collections::HashSet;
use std::fmt;

use crate::agg::Agg;
use crate::ident::Ident;
use crate::value::ColumnType;

/// Seconds per day. Every grain divides it, so buckets roll up to days.
const DAY: u32 = 86_400;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub schema: Ident,
    pub name: Ident,
}

/// A table facts join to by key, any number of rows per key. Straum holds
/// it projected to its
/// key and the columns the views read, kept current from the stream, and
/// an ask reads a dim's values through a cell's join columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dim {
    pub name: Ident,
    pub table: Table,
    /// One column or more; a row is named by their values together.
    pub key: Vec<Ident>,
    /// Every column any ref reads of it.
    pub attrs: Vec<Ident>,
}

impl Dim {
    /// The columns a row of this dim carries, in order: the key columns,
    /// then the attributes.
    pub fn columns(&self) -> impl Iterator<Item = &Ident> {
        self.key.iter().chain(&self.attrs)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    pub name: Ident,
    pub table: Table,
    pub time: Time,
    /// In join order: a ref joined through another comes after it.
    pub refs: Vec<Ref>,
    pub by: Vec<By>,
    pub measures: Vec<Measure>,
    /// The view's `WHERE`: a row counts, for every measure, only when
    /// every condition holds.
    pub filter: Vec<Cond>,
}

/// A rollup key a fact row gives: a column's value, or whether a column
/// is null, which a nullable column such as `completed` means by. Every
/// key has the name the view gives it with `AS`, which is what asks use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum By {
    /// `column AS name`.
    Column { column: Ident, name: Ident },
    /// `(column IS NULL) AS name`, or `IS NOT NULL` when `negated`: a
    /// boolean key whatever the column's type.
    IsNull {
        column: Ident,
        name: Ident,
        negated: bool,
    },
}

impl By {
    /// What asks call it.
    pub fn name(&self) -> &Ident {
        match self {
            By::Column { name, .. } | By::IsNull { name, .. } => name,
        }
    }

    /// The fact column it reads.
    pub(crate) fn column(&self) -> &Ident {
        match self {
            By::Column { column, .. } | By::IsNull { column, .. } => column,
        }
    }

    pub(crate) fn slot(&self) -> Slot<'_> {
        match self {
            By::Column { column, .. } => Slot::Column(column),
            By::IsNull {
                column, negated, ..
            } => Slot::IsNull {
                column,
                negated: *negated,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Time {
    pub column: Ident,
    pub grain: Grain,
    /// Wider bucket widths kept beside `grain`, narrowest first. Each is a
    /// whole multiple of the width before it, so one of its buckets is made
    /// of whole buckets of every narrower width.
    pub buckets: Vec<Grain>,
    pub origin: Origin,
}

/// The `date_bin` origin, which must be of the time column's type: with a
/// `timestamp` column, PostgreSQL would cast a `timestamptz 'epoch'`
/// through the session zone, and the view body would not mean what straum
/// computes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Timestamptz,
    Timestamp,
}

impl Origin {
    /// The column type that goes with this origin.
    pub fn column_type(self) -> ColumnType {
        match self {
            Origin::Timestamptz => ColumnType::Time { zoned: true },
            Origin::Timestamp => ColumnType::Time { zoned: false },
        }
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Origin::Timestamptz => "timestamptz 'epoch'",
            Origin::Timestamp => "timestamp 'epoch'",
        })
    }
}

/// Bucket width in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Grain(pub u32);

/// A join from the fact, or from a dim already joined, to a dim's key,
/// column for column, of any cardinality. The fact-side via columns are
/// rollup keys, so a cell is keyed by the dim rows it points at; the
/// dim's values are read from its projection when asked, as the dim is
/// then, so a dim change moves every answer at once. A key with several
/// rows repeats the cell once per row, as PostgreSQL repeats the fact
/// row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub name: Ident,
    pub dim: Ident,
    pub kind: RefKind,
    /// In the dim's key order.
    pub via: Vec<Via>,
    /// The dim columns this ref reads, each under the name asks use;
    /// none for a semi or anti join.
    pub attrs: Vec<Attr>,
}

/// What the join keeps of a fact row, as SQL's join kinds do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefKind {
    /// Every row, once per dim row matched, with null dim values when
    /// none is: `LEFT OUTER JOIN`.
    Left,
    /// Rows with a match, once per dim row matched: `INNER JOIN`.
    Inner,
    /// Rows with a match, once: `WHERE EXISTS (...)`.
    Semi,
    /// Rows without a match, once: `WHERE NOT EXISTS (...)`.
    Anti,
}

/// A column a join reads its key from: the fact's, or a column of a dim
/// joined before, which that dim's projection then carries.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Via {
    Fact(Ident),
    Attr {
        /// Index into [`Fact::refs`], before this ref.
        through: usize,
        column: Ident,
    },
}

/// `alias.column AS name`: a dim column a ref reads, and what asks call
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attr {
    pub name: Ident,
    pub column: Ident,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measure {
    pub name: Ident,
    pub agg: Agg,
    /// A row counts only when every condition holds.
    pub filter: Vec<Cond>,
}

/// `key = value`, or `key in (values)` with more than one value, `key`
/// being a name the view gives a key; see [`Fact::resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cond {
    pub key: Ident,
    pub values: Vec<Literal>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    Int(i64),
    Text(String),
    Bool(bool),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key<'a> {
    By(&'a By),
    /// An attribute a ref reads. A ref has no value of its own: asks name
    /// its attributes, or the fact columns it joins on.
    Attr(&'a Ref, &'a Attr),
}

/// One part of the rollup key: a fact column's value, whether a fact
/// column is null, or the time bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slot<'a> {
    Column(&'a Ident),
    IsNull { column: &'a Ident, negated: bool },
    Bucket,
}

impl Fact {
    /// The `by` keys, then every ref's fact-side via columns in ref
    /// order, each slot once, then the time bucket.
    pub fn rollup_key(&self) -> impl Iterator<Item = Slot<'_>> {
        let mut slots: Vec<Slot<'_>> = Vec::new();
        let vias = self.fact_vias().map(Slot::Column);
        for slot in self.by.iter().map(By::slot).chain(vias) {
            if !slots.contains(&slot) {
                slots.push(slot);
            }
        }
        slots.push(Slot::Bucket);
        slots.into_iter()
    }

    /// The fact columns the refs join on, in ref order, a column as often
    /// as it is joined on.
    pub fn fact_vias(&self) -> impl Iterator<Item = &Ident> {
        self.refs
            .iter()
            .flat_map(|r| &r.via)
            .filter_map(|v| match v {
                Via::Fact(column) => Some(column),
                Via::Attr { .. } => None,
            })
    }

    /// Every ref's attributes, each with its ref, in ref order.
    pub fn attrs(&self) -> impl Iterator<Item = (&Ref, &Attr)> {
        self.refs
            .iter()
            .flat_map(|r| r.attrs.iter().map(move |a| (r, a)))
    }

    /// The columns a row of this fact is read from, in order: the time
    /// column, the columns the `by` keys read in rollup-key order, the via
    /// columns of the refs, then the measure inputs, each column once.
    pub fn columns(&self) -> Vec<Ident> {
        let mut columns: Vec<Ident> = vec![self.time.column.clone()];
        let read = (self.by.iter().map(By::column))
            .chain(self.fact_vias())
            .chain(self.measures.iter().filter_map(|m| m.agg.input()));
        for column in read {
            if !columns.contains(column) {
                columns.push(column.clone());
            }
        }
        columns
    }

    pub fn measure(&self, name: &str) -> Option<&Measure> {
        self.measures.iter().find(|m| m.name.as_str() == name)
    }

    /// The key the view names so: a `by` key, or an attribute a ref
    /// reads. Keys and attributes share one namespace.
    pub fn resolve(&self, name: &Ident) -> Option<Key<'_>> {
        if let Some(by) = self.by.iter().find(|b| b.name() == name) {
            return Some(Key::By(by));
        }
        self.refs.iter().find_map(|r| {
            let attr = r.attrs.iter().find(|a| a.name == *name)?;
            Some(Key::Attr(r, attr))
        })
    }

    /// Every name an ask may group or filter by, keys then attributes.
    pub fn keys(&self) -> impl Iterator<Item = &Ident> {
        (self.by.iter().map(By::name)).chain(self.attrs().map(|(_, a)| &a.name))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    dims: Vec<Dim>,
    facts: Vec<Fact>,
}

impl Spec {
    /// Column types and keys are checked against PostgreSQL elsewhere.
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

    /// Every table the spec reads, each once, though a fact and a dim may
    /// read the same one: the dims' first, then the facts', in
    /// declaration order.
    pub fn tables(&self) -> Vec<&Table> {
        let mut tables: Vec<&Table> = Vec::new();
        let all = (self.dims.iter().map(|d| &d.table)).chain(self.facts.iter().map(|f| &f.table));
        for table in all {
            if !tables.contains(&table) {
                tables.push(table);
            }
        }
        tables
    }

    pub fn dim(&self, name: &str) -> Option<&Dim> {
        self.dims.iter().find(|d| d.name.as_str() == name)
    }

    pub fn fact(&self, name: &str) -> Option<&Fact> {
        self.facts.iter().find(|f| f.name.as_str() == name)
    }
}

fn check_dim(dim: &Dim, errors: &mut Vec<SpecError>) {
    if dim.key.is_empty() {
        errors.push(SpecError::NoKey {
            dim: dim.name.clone(),
        });
    }
    let mut columns = HashSet::new();
    for column in dim.columns() {
        if !columns.insert(column) {
            errors.push(SpecError::Duplicate {
                scope: Some(dim.name.clone()),
                name: column.clone(),
            });
        }
    }
}

fn check_fact(fact: &Fact, dims: &[Dim], errors: &mut Vec<SpecError>) {
    check_time(fact, errors);
    // `by` keys and the attributes refs read share one namespace, since
    // an ask names either by its name alone.
    let mut keys: HashSet<&Ident> = HashSet::new();
    check_by(fact, &mut keys, errors);
    let mut ref_names = HashSet::new();
    for (position, r) in fact.refs.iter().enumerate() {
        if !ref_names.insert(&r.name) {
            errors.push(SpecError::Duplicate {
                scope: Some(fact.name.clone()),
                name: r.name.clone(),
            });
        }
        check_ref(fact, position, dims, &mut keys, errors);
    }
    check_measures(fact, &mut keys, errors);
}

fn check_time(fact: &Fact, errors: &mut Vec<SpecError>) {
    let name = &fact.name;
    let Grain(seconds) = fact.time.grain;
    if seconds == 0 || !DAY.is_multiple_of(seconds) {
        errors.push(SpecError::BadGrain {
            fact: name.clone(),
            seconds,
        });
    }
    let mut below = fact.time.grain;
    for &bucket in &fact.time.buckets {
        if bucket.0 <= below.0 || !bucket.0.is_multiple_of(below.0) {
            errors.push(SpecError::BadBucket {
                fact: name.clone(),
                seconds: bucket.0,
                below: below.0,
            });
        }
        below = bucket;
    }
}

fn check_by<'a>(fact: &'a Fact, keys: &mut HashSet<&'a Ident>, errors: &mut Vec<SpecError>) {
    let name = &fact.name;
    for by in &fact.by {
        match by {
            By::Column { column, .. } if *column == fact.time.column => {
                errors.push(SpecError::TimeAsKey {
                    fact: name.clone(),
                    column: column.clone(),
                });
            }
            By::IsNull {
                column, name: key, ..
            } if *column == fact.time.column => {
                errors.push(SpecError::NullTime {
                    fact: name.clone(),
                    name: key.clone(),
                    column: column.clone(),
                });
            }
            By::Column { .. } | By::IsNull { .. } => {}
        }
        if !keys.insert(by.name()) {
            errors.push(SpecError::Duplicate {
                scope: Some(name.clone()),
                name: by.name().clone(),
            });
        }
    }
}

fn check_ref<'a>(
    fact: &'a Fact,
    position: usize,
    dims: &[Dim],
    keys: &mut HashSet<&'a Ident>,
    errors: &mut Vec<SpecError>,
) {
    let name = &fact.name;
    let r = &fact.refs[position];
    let mut via = HashSet::new();
    for column in &r.via {
        if !via.insert(column) {
            errors.push(SpecError::ViaReused {
                fact: name.clone(),
                ref_name: r.name.clone(),
                column: via_name(column, &fact.refs),
            });
        }
        if let Via::Attr { through, column } = column {
            let parent = (*through < position).then(|| &fact.refs[*through]);
            let carried = parent
                .and_then(|p| dims.iter().find(|d| d.name == p.dim))
                .is_some_and(|d| d.attrs.contains(column));
            if !carried {
                errors.push(SpecError::ChainColumn {
                    fact: name.clone(),
                    ref_name: r.name.clone(),
                    through: *through,
                    column: column.clone(),
                });
            }
        }
    }
    let dim = dims.iter().find(|d| d.name == r.dim);
    match dim {
        None => errors.push(SpecError::UnknownDim {
            fact: name.clone(),
            ref_name: r.name.clone(),
            dim: r.dim.clone(),
        }),
        Some(dim) if dim.key.len() != r.via.len() => errors.push(SpecError::KeyWidth {
            fact: name.clone(),
            ref_name: r.name.clone(),
            via: r.via.len(),
            dim: dim.name.clone(),
            key: dim.key.len(),
        }),
        Some(_) => {}
    }
    let reads = !r.attrs.is_empty();
    match r.kind {
        RefKind::Left | RefKind::Inner if !reads => errors.push(SpecError::RefNoAttrs {
            fact: name.clone(),
            ref_name: r.name.clone(),
        }),
        RefKind::Semi | RefKind::Anti if reads => errors.push(SpecError::RefReads {
            fact: name.clone(),
            ref_name: r.name.clone(),
            kind: r.kind,
        }),
        _ => {}
    }
    let mut read = HashSet::new();
    for attr in &r.attrs {
        if !read.insert(&attr.column) {
            errors.push(SpecError::Duplicate {
                scope: Some(r.name.clone()),
                name: attr.column.clone(),
            });
        } else if let Some(dim) = dim
            && !dim.columns().any(|c| *c == attr.column)
        {
            errors.push(SpecError::UnknownAttr {
                fact: name.clone(),
                ref_name: r.name.clone(),
                attr: attr.column.clone(),
                dim: dim.name.clone(),
            });
        }
        if !keys.insert(&attr.name) {
            errors.push(SpecError::Duplicate {
                scope: Some(name.clone()),
                name: attr.name.clone(),
            });
        }
    }
}

/// Measures share the key namespace too: an ask names both alike.
fn check_measures<'a>(fact: &'a Fact, keys: &mut HashSet<&'a Ident>, errors: &mut Vec<SpecError>) {
    let name = &fact.name;
    if fact.measures.is_empty() {
        errors.push(SpecError::NoMeasures { fact: name.clone() });
    }
    let mut named = fact.keys().chain(fact.measures.iter().map(|m| &m.name));
    if named.any(|n| n.as_str() == "bucket") {
        errors.push(SpecError::NamedBucket { fact: name.clone() });
    }
    for m in &fact.measures {
        if !keys.insert(&m.name) {
            errors.push(SpecError::Duplicate {
                scope: Some(name.clone()),
                name: m.name.clone(),
            });
        }
        for cond in &m.filter {
            if fact.resolve(&cond.key).is_none() {
                errors.push(SpecError::UnknownKey {
                    fact: name.clone(),
                    measure: Some(m.name.clone()),
                    key: cond.key.clone(),
                });
            }
        }
    }
    for cond in &fact.filter {
        if fact.resolve(&cond.key).is_none() {
            errors.push(SpecError::UnknownKey {
                fact: name.clone(),
                measure: None,
                key: cond.key.clone(),
            });
        }
    }
}

/// A via column as a message names it: the fact's, or `ref.column`.
fn via_name(via: &Via, refs: &[Ref]) -> String {
    match via {
        Via::Fact(column) => column.to_string(),
        Via::Attr { through, column } => {
            let through = refs.get(*through).map_or("?", |r| r.name.as_str());
            format!("{through}.{column}")
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecError {
    /// Two declarations share a name; `scope` is the enclosing dim, fact or
    /// ref.
    Duplicate {
        scope: Option<Ident>,
        name: Ident,
    },
    BadGrain {
        fact: Ident,
        seconds: u32,
    },
    /// A bucket width that isn't a wider whole multiple of the width
    /// before it.
    BadBucket {
        fact: Ident,
        seconds: u32,
        below: u32,
    },
    /// The time column is a `by` column too, which the catalog would refuse
    /// anyway: a time never groups.
    TimeAsKey {
        fact: Ident,
        column: Ident,
    },
    /// A null-ness key over the time column, which could never be true in
    /// an answer: a row with no time is never answered.
    NullTime {
        fact: Ident,
        name: Ident,
        column: Ident,
    },
    /// A ref joins on one column twice.
    ViaReused {
        fact: Ident,
        ref_name: Ident,
        column: String,
    },
    /// A ref joins on another number of columns than its dim's key has.
    KeyWidth {
        fact: Ident,
        ref_name: Ident,
        via: usize,
        dim: Ident,
        key: usize,
    },
    UnknownDim {
        fact: Ident,
        ref_name: Ident,
        dim: Ident,
    },
    /// A left or inner ref that reads nothing, so the join does nothing.
    RefNoAttrs {
        fact: Ident,
        ref_name: Ident,
    },
    /// A semi or anti ref that reads columns, which it has none of.
    RefReads {
        fact: Ident,
        ref_name: Ident,
        kind: RefKind,
    },
    /// A ref joined through a dim column that no ref before it carries.
    ChainColumn {
        fact: Ident,
        ref_name: Ident,
        through: usize,
        column: Ident,
    },
    /// A ref reads an attribute its dim has no column of.
    UnknownAttr {
        fact: Ident,
        ref_name: Ident,
        attr: Ident,
        dim: Ident,
    },
    NoKey {
        dim: Ident,
    },
    /// A filter, of a measure or the view's, names something that is no
    /// key of the view.
    UnknownKey {
        fact: Ident,
        measure: Option<Ident>,
        key: Ident,
    },
    NoMeasures {
        fact: Ident,
    },
    /// Asks call the time bucket `bucket`, so nothing else may be.
    NamedBucket {
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
            SpecError::BadBucket {
                fact,
                seconds,
                below,
            } => write!(
                f,
                "fact {fact}: bucket of {seconds}s is not a wider multiple of {below}s"
            ),
            SpecError::TimeAsKey { fact, column } => {
                write!(
                    f,
                    "fact {fact}: {column} is the time column, so it can't be a by column too"
                )
            }
            SpecError::NullTime { fact, name, column } => write!(
                f,
                "fact {fact}: {name} asks whether the time column {column} is null, \
                 which no ask could see: a row with no time is never answered"
            ),
            SpecError::ViaReused {
                fact,
                ref_name,
                column,
            } => write!(f, "fact {fact}: ref {ref_name} joins on {column} twice"),
            SpecError::KeyWidth {
                fact,
                ref_name,
                via,
                dim,
                key,
            } => write!(
                f,
                "fact {fact}: ref {ref_name} joins on {via} columns, but the key of dim {dim} has {key}"
            ),
            SpecError::RefNoAttrs { fact, ref_name } => {
                write!(f, "fact {fact}: ref {ref_name} reads no column of its dim")
            }
            SpecError::RefReads {
                fact,
                ref_name,
                kind,
            } => write!(
                f,
                "fact {fact}: ref {ref_name} is {kind} and reads columns, which it has none of"
            ),
            SpecError::ChainColumn {
                fact,
                ref_name,
                through,
                column,
            } => write!(
                f,
                "fact {fact}: ref {ref_name} joins through column {column} of ref {through}, \
                 which no ref before it carries"
            ),
            SpecError::UnknownAttr {
                fact,
                ref_name,
                attr,
                dim,
            } => write!(
                f,
                "fact {fact}: ref {ref_name} reads {attr}, which dim {dim} has no column of"
            ),
            SpecError::NoKey { dim } => write!(f, "dim {dim}: no key columns"),
            SpecError::UnknownDim {
                fact,
                ref_name,
                dim,
            } => {
                write!(f, "fact {fact}: ref {ref_name} points at unknown dim {dim}")
            }
            SpecError::UnknownKey {
                fact,
                measure: Some(measure),
                key,
            } => write!(
                f,
                "fact {fact}: measure {measure} filters on {key}, which names no key of the view"
            ),
            SpecError::UnknownKey {
                fact,
                measure: None,
                key,
            } => write!(
                f,
                "fact {fact}: WHERE filters on {key}, which names no key of the view"
            ),
            SpecError::NoMeasures { fact } => write!(f, "fact {fact}: no measures"),
            SpecError::NamedBucket { fact } => write!(
                f,
                "fact {fact}: bucket is what asks call the time bucket; \
                 name the column, ref or measure otherwise"
            ),
        }
    }
}

impl std::error::Error for SpecError {}

impl fmt::Display for RefKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            RefKind::Left => "a left outer join",
            RefKind::Inner => "an inner join",
            RefKind::Semi => "a semi join",
            RefKind::Anti => "an anti join",
        })
    }
}

impl fmt::Display for Grain {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}s", self.0)
    }
}
