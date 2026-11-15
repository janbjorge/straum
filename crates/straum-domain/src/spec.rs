//! The spec: the views a user declares, as plain data. Straum SQL is
//! read into these types by `straum-notation`; [`Spec::new`] then checks
//! them and reports every error it finds as a [`SpecError`]. A view
//! (`Fact`) counts the rows of one table, its fact table, by its keys and
//! time buckets; a dim (`Dim`) is a table a view joins through a ref
//! (`Ref`). The store reads a checked spec to lay out its cells.

use std::collections::HashSet;
use std::fmt;

use crate::aggregate::Aggregate;
use crate::identifier::Identifier;
use crate::value::ColumnType;

/// Seconds per day. Every width divides it, so buckets roll up to days.
const DAY: u32 = 86_400;

/// A `schema.table` in PostgreSQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub schema: Identifier,
    pub name: Identifier,
}

/// A table facts join to by key, any number of rows per key. Straum holds
/// it projected to its key and the columns the views read, kept current
/// from the stream, and an ask reads a dim's values through a cell's join
/// columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dim {
    pub name: Identifier,
    pub table: Table,
    /// One column or more; a row is named by their values together.
    pub key: Vec<Identifier>,
    /// Every column any ref reads of it.
    pub attributes: Vec<Identifier>,
    /// The columns written `alias.column::text`: read as the text
    /// PostgreSQL prints for them, whatever their type there.
    pub as_text: Vec<Identifier>,
}

impl Dim {
    /// The columns a row of this dim carries, in order: the key columns,
    /// then the attributes.
    // Borrows the names; callers count or walk them once, so no list is
    // built for them.
    pub fn columns(&self) -> impl Iterator<Item = &Identifier> {
        self.key.iter().chain(&self.attributes)
    }
}

/// A view: one `CREATE VIEW` of the spec, counting the rows of its fact
/// table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    pub name: Identifier,
    pub table: Table,
    pub time: Time,
    /// In join order: a ref joined through another comes after it.
    pub refs: Vec<Ref>,
    pub by_keys: Vec<ByKey>,
    pub measures: Vec<Measure>,
    /// The view's `WHERE`: a row counts, for every measure, only when
    /// every condition holds.
    pub filter: Vec<Condition>,
    /// The columns written `alias.column::text`: read as the text
    /// PostgreSQL prints for them, whatever their type there.
    pub as_text: Vec<Identifier>,
}

/// A rollup key a fact row gives: a column's value, or whether a column
/// is null, which a nullable column such as `completed` means by. Every
/// key has the name the view gives it with `AS`, which is what asks use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ByKey {
    /// `column AS name`.
    Column {
        column: Identifier,
        name: Identifier,
    },
    /// `(column IS NULL) AS name`, or `IS NOT NULL` when `negated`: a
    /// boolean key whatever the column's type.
    IsNull {
        column: Identifier,
        name: Identifier,
        negated: bool,
    },
}

impl ByKey {
    /// The name asks use for the key.
    pub fn name(&self) -> &Identifier {
        match self {
            ByKey::Column { name, .. } | ByKey::IsNull { name, .. } => name,
        }
    }

    /// The fact column the key reads.
    pub(crate) fn column(&self) -> &Identifier {
        match self {
            ByKey::Column { column, .. } | ByKey::IsNull { column, .. } => column,
        }
    }

    /// The part of the rollup key this key gives.
    pub(crate) fn key_part(&self) -> KeyPart<'_> {
        match self {
            ByKey::Column { column, .. } => KeyPart::Column(column),
            ByKey::IsNull {
                column, negated, ..
            } => KeyPart::IsNull {
                column,
                negated: *negated,
            },
        }
    }
}

/// The view's time: the column `date_bin` reads, and the bucket widths
/// kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Time {
    pub column: Identifier,
    pub width: Width,
    /// Wider bucket widths kept beside `width`, narrowest first. Each is a
    /// whole multiple of the width before it, so one of its buckets is made
    /// of whole buckets of every narrower width.
    pub wider: Vec<Width>,
    pub origin: Origin,
}

/// The `date_bin` origin, which must be of the time column's type: with a
/// `timestamp` column, PostgreSQL would cast a `timestamptz 'epoch'`
/// through the session zone, and the view body would not mean what Straum
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

/// A bucket width.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Width {
    pub seconds: u32,
}

impl Width {
    /// The start of the bucket `seconds` falls in. Buckets are UTC.
    pub fn bucket_start(self, seconds: i64) -> i64 {
        seconds.div_euclid(i64::from(self.seconds)) * i64::from(self.seconds)
    }
}

/// A join from the fact, or from a dim already joined, to a dim's key,
/// column for column, of any cardinality. The fact-side via columns are
/// rollup keys, so a cell is keyed by the dim rows it points at; the
/// dim's values are read from its projection when asked, as the dim is
/// then, so a dim change moves every answer at once. A key with several
/// rows repeats the cell once per row, as PostgreSQL repeats the fact
/// row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ref {
    pub alias: Identifier,
    pub dim: Identifier,
    pub kind: RefKind,
    /// In the dim's key order.
    pub via: Vec<Via>,
    /// The dim columns this ref reads, each under the name asks use;
    /// none for a semi or anti join.
    pub reads: Vec<Attribute>,
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
    Fact(Identifier),
    Attribute {
        /// Index into [`Fact::refs`], before this ref.
        through: usize,
        column: Identifier,
    },
}

/// `alias.column AS name`: a dim column a ref reads, and what asks call
/// it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attribute {
    pub name: Identifier,
    pub column: Identifier,
}

/// A named aggregate of the view, with its own `FILTER`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Measure {
    pub name: Identifier,
    pub aggregate: Aggregate,
    /// A row counts only when every condition holds.
    pub filter: Vec<Condition>,
}

/// `key = value`, or `key in (values)` with more than one value, `key`
/// being a name the view gives a key; see [`Fact::resolve`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub key: Identifier,
    pub values: Vec<Literal>,
}

/// A constant in a filter, or a parameter of an ask.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    Int(i64),
    Text(String),
    Bool(bool),
}

/// What a name in a view points at.
// Borrows from the fact it was resolved in, which outlives it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key<'a> {
    ByKey(&'a ByKey),
    /// An attribute a ref reads. A ref has no value of its own: asks name
    /// its attributes, or the fact columns it joins on.
    Attribute(&'a Ref, &'a Attribute),
}

/// One part of the rollup key: a fact column's value, whether a fact
/// column is null, or the time bucket.
// Borrows the fact's column names, so comparing parts copies no text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyPart<'a> {
    Column(&'a Identifier),
    IsNull {
        column: &'a Identifier,
        negated: bool,
    },
    Bucket,
}

impl Fact {
    /// The `by` keys, then every ref's fact-side via columns in ref
    /// order, each part once, then the time bucket.
    // An iterator, as callers count, number or walk the parts; the parts
    // borrow the fact's names.
    pub fn rollup_key(&self) -> impl Iterator<Item = KeyPart<'_>> {
        let mut parts: Vec<KeyPart<'_>> = self.by_parts().collect();
        for column in self.fact_vias() {
            let part = KeyPart::Column(column);
            if !parts.contains(&part) {
                parts.push(part);
            }
        }
        parts.push(KeyPart::Bucket);
        parts.into_iter()
    }

    /// The `by` keys' parts, each once.
    // An iterator for the same reason as `rollup_key`.
    pub(crate) fn by_parts(&self) -> impl Iterator<Item = KeyPart<'_>> {
        let mut parts: Vec<KeyPart<'_>> = Vec::new();
        for by in &self.by_keys {
            let part = by.key_part();
            if !parts.contains(&part) {
                parts.push(part);
            }
        }
        parts.into_iter()
    }

    /// Whether every ref joins on fact columns, none through another dim.
    pub(crate) fn is_direct(&self) -> bool {
        for join in &self.refs {
            for via in &join.via {
                if let Via::Attribute { .. } = via {
                    return false;
                }
            }
        }
        true
    }

    /// Whether a ref's via column is a part of the rollup key that no
    /// `by` key is. Only then are the cells finer than the `by` keys, and
    /// cells keyed by the dim columns fewer (ADR-0042).
    pub fn vias_add_parts(&self) -> bool {
        self.rollup_key().count() > self.by_parts().count() + 1
    }

    /// The fact columns the refs join on, in ref order, a column as often
    /// as it is joined on.
    // Borrows the names, as `Dim::columns` does.
    pub fn fact_vias(&self) -> impl Iterator<Item = &Identifier> {
        let mut columns = Vec::new();
        for join in &self.refs {
            for via in &join.via {
                if let Via::Fact(column) = via {
                    columns.push(column);
                }
            }
        }
        columns.into_iter()
    }

    /// Every ref's attributes, each with its ref, in ref order.
    // Borrows the refs, as `Dim::columns` does.
    pub fn attributes(&self) -> impl Iterator<Item = (&Ref, &Attribute)> {
        let mut attributes = Vec::new();
        for join in &self.refs {
            for attribute in &join.reads {
                attributes.push((join, attribute));
            }
        }
        attributes.into_iter()
    }

    /// The columns a row of this fact is read from, in order: the time
    /// column, the columns the `by` keys read in rollup-key order, the via
    /// columns of the refs, then the measure inputs, each column once.
    pub fn columns(&self) -> Vec<Identifier> {
        let mut read: Vec<&Identifier> = Vec::new();
        for by in &self.by_keys {
            read.push(by.column());
        }
        read.extend(self.fact_vias());
        for measure in &self.measures {
            if let Some(input) = measure.aggregate.input() {
                read.push(input);
            }
        }
        let mut columns: Vec<Identifier> = vec![self.time.column.clone()];
        for column in read {
            if !columns.contains(column) {
                columns.push(column.clone());
            }
        }
        columns
    }

    /// The measure of that name, if the view has one.
    pub fn measure(&self, name: &str) -> Option<&Measure> {
        for measure in &self.measures {
            if measure.name.as_str() == name {
                return Some(measure);
            }
        }
        None
    }

    /// The key the view names so: a `by` key, or an attribute a ref
    /// reads. Keys and attributes share one namespace.
    pub fn resolve(&self, name: &Identifier) -> Option<Key<'_>> {
        for by in &self.by_keys {
            if by.name() == name {
                return Some(Key::ByKey(by));
            }
        }
        for join in &self.refs {
            for attribute in &join.reads {
                if attribute.name == *name {
                    return Some(Key::Attribute(join, attribute));
                }
            }
        }
        None
    }

    /// Every name an ask may group or filter by, keys then attributes.
    // Borrows the names, as `Dim::columns` does.
    pub fn keys(&self) -> impl Iterator<Item = &Identifier> {
        let mut names = Vec::new();
        for by in &self.by_keys {
            names.push(by.name());
        }
        for join in &self.refs {
            for attribute in &join.reads {
                names.push(&attribute.name);
            }
        }
        names.into_iter()
    }
}

/// Every dim and view of a spec, checked to fit together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Spec {
    dims: Vec<Dim>,
    facts: Vec<Fact>,
}

impl Spec {
    /// Column types and keys are checked against PostgreSQL elsewhere.
    pub fn new(dims: Vec<Dim>, facts: Vec<Fact>) -> Result<Spec, Vec<SpecError>> {
        let mut errors = Vec::new();

        let mut declared: Vec<&Identifier> = Vec::new();
        for dim in &dims {
            declared.push(&dim.name);
        }
        for fact in &facts {
            declared.push(&fact.name);
        }
        let mut names = HashSet::new();
        for name in declared {
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

    /// Every dim, in declaration order.
    pub fn dims(&self) -> &[Dim] {
        &self.dims
    }

    /// Every view, in declaration order.
    pub fn facts(&self) -> &[Fact] {
        &self.facts
    }

    /// Every table the spec reads, each once, though a fact and a dim may
    /// read the same one: the dims' first, then the facts', in
    /// declaration order.
    pub fn tables(&self) -> Vec<&Table> {
        let mut tables: Vec<&Table> = Vec::new();
        for dim in &self.dims {
            if !tables.contains(&&dim.table) {
                tables.push(&dim.table);
            }
        }
        for fact in &self.facts {
            if !tables.contains(&&fact.table) {
                tables.push(&fact.table);
            }
        }
        tables
    }

    /// The dim of that name, if the spec has one.
    pub fn dim(&self, name: &str) -> Option<&Dim> {
        for dim in &self.dims {
            if dim.name.as_str() == name {
                return Some(dim);
            }
        }
        None
    }

    /// The view of that name, if the spec has one.
    pub fn fact(&self, name: &str) -> Option<&Fact> {
        for fact in &self.facts {
            if fact.name.as_str() == name {
                return Some(fact);
            }
        }
        None
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
    let mut keys: HashSet<&Identifier> = HashSet::new();
    check_by(fact, &mut keys, errors);
    let mut ref_names = HashSet::new();
    for (position, join) in fact.refs.iter().enumerate() {
        if !ref_names.insert(&join.alias) {
            errors.push(SpecError::Duplicate {
                scope: Some(fact.name.clone()),
                name: join.alias.clone(),
            });
        }
        check_ref(fact, position, dims, &mut keys, errors);
    }
    check_measures(fact, &mut keys, errors);
}

fn check_time(fact: &Fact, errors: &mut Vec<SpecError>) {
    let name = &fact.name;
    let seconds = fact.time.width.seconds;
    if seconds == 0 || !DAY.is_multiple_of(seconds) {
        errors.push(SpecError::BadWidth {
            fact: name.clone(),
            seconds,
        });
    }
    let mut below = fact.time.width;
    for &bucket in &fact.time.wider {
        if bucket.seconds <= below.seconds || !bucket.seconds.is_multiple_of(below.seconds) {
            errors.push(SpecError::BadBucket {
                fact: name.clone(),
                seconds: bucket.seconds,
                below: below.seconds,
            });
        }
        below = bucket;
    }
}

// `'a`, here and in the checks below: the set of names taken borrows them
// from the fact, so checking copies none.
fn check_by<'a>(fact: &'a Fact, keys: &mut HashSet<&'a Identifier>, errors: &mut Vec<SpecError>) {
    let name = &fact.name;
    for by in &fact.by_keys {
        if *by.column() == fact.time.column {
            errors.push(match by {
                ByKey::Column { column, .. } => SpecError::TimeAsKey {
                    fact: name.clone(),
                    column: column.clone(),
                },
                ByKey::IsNull {
                    column, name: key, ..
                } => SpecError::NullTime {
                    fact: name.clone(),
                    name: key.clone(),
                    column: column.clone(),
                },
            });
        }
        if !keys.insert(by.name()) {
            errors.push(SpecError::Duplicate {
                scope: Some(name.clone()),
                name: by.name().clone(),
            });
        }
    }
}

/// The error `error` of the join `join` of `fact`.
fn join_error(fact: &Fact, join: &Ref, error: JoinError) -> SpecError {
    SpecError::Join {
        fact: fact.name.clone(),
        join: join.alias.clone(),
        error,
    }
}

fn check_ref<'a>(
    fact: &'a Fact,
    position: usize,
    dims: &[Dim],
    keys: &mut HashSet<&'a Identifier>,
    errors: &mut Vec<SpecError>,
) {
    check_vias(fact, position, dims, errors);
    let join = &fact.refs[position];
    let dim = check_ref_dim(fact, join, dims, errors);
    check_ref_kind(fact, join, errors);
    check_reads(fact, join, dim, keys, errors);
}

fn check_vias(fact: &Fact, position: usize, dims: &[Dim], errors: &mut Vec<SpecError>) {
    let join = &fact.refs[position];
    let mut via = HashSet::new();
    for column in &join.via {
        if !via.insert(column) {
            let column = via_name(column, &fact.refs);
            errors.push(join_error(fact, join, JoinError::ViaReused { column }));
        }
        if let Via::Attribute { through, column } = column
            && !carried_before(fact, position, dims, *through, column)
        {
            let error = JoinError::ChainColumn {
                through: *through,
                column: column.clone(),
            };
            errors.push(join_error(fact, join, error));
        }
    }
}

// A chain reads its key from a ref joined before it, whose dim must have
// the column.
fn carried_before(
    fact: &Fact,
    position: usize,
    dims: &[Dim],
    through: usize,
    column: &Identifier,
) -> bool {
    if through >= position {
        return false;
    }
    let parent = &fact.refs[through];
    let Some(dim) = find_dim(dims, &parent.dim) else {
        return false;
    };
    dim.attributes.contains(column)
}

fn find_dim<'d>(dims: &'d [Dim], name: &Identifier) -> Option<&'d Dim> {
    for dim in dims {
        if dim.name == *name {
            return Some(dim);
        }
    }
    None
}

fn check_ref_dim<'d>(
    fact: &Fact,
    join: &Ref,
    dims: &'d [Dim],
    errors: &mut Vec<SpecError>,
) -> Option<&'d Dim> {
    let Some(dim) = find_dim(dims, &join.dim) else {
        let error = JoinError::UnknownDim {
            dim: join.dim.clone(),
        };
        errors.push(join_error(fact, join, error));
        return None;
    };
    if dim.key.len() != join.via.len() {
        let error = JoinError::KeyLength {
            via: join.via.len(),
            dim: dim.name.clone(),
            key: dim.key.len(),
        };
        errors.push(join_error(fact, join, error));
    }
    Some(dim)
}

fn check_ref_kind(fact: &Fact, join: &Ref, errors: &mut Vec<SpecError>) {
    let reads = !join.reads.is_empty();
    match join.kind {
        RefKind::Left | RefKind::Inner if !reads => {
            errors.push(join_error(fact, join, JoinError::ReadsNothing));
        }
        RefKind::Semi | RefKind::Anti if reads => {
            let error = JoinError::ReadsColumns { kind: join.kind };
            errors.push(join_error(fact, join, error));
        }
        _ => {}
    }
}

fn check_reads<'a>(
    fact: &'a Fact,
    join: &'a Ref,
    dim: Option<&Dim>,
    keys: &mut HashSet<&'a Identifier>,
    errors: &mut Vec<SpecError>,
) {
    let mut read = HashSet::new();
    for attribute in &join.reads {
        if !read.insert(&attribute.column) {
            errors.push(SpecError::Duplicate {
                scope: Some(join.alias.clone()),
                name: attribute.column.clone(),
            });
        } else if let Some(dim) = dim
            && !dim_has_column(dim, &attribute.column)
        {
            let error = JoinError::UnknownAttribute {
                attribute: attribute.column.clone(),
                dim: dim.name.clone(),
            };
            errors.push(join_error(fact, join, error));
        }
        if !keys.insert(&attribute.name) {
            errors.push(SpecError::Duplicate {
                scope: Some(fact.name.clone()),
                name: attribute.name.clone(),
            });
        }
    }
}

fn dim_has_column(dim: &Dim, wanted: &Identifier) -> bool {
    for column in dim.columns() {
        if column == wanted {
            return true;
        }
    }
    false
}

/// Measures share the key namespace too: an ask names both alike.
fn check_measures<'a>(
    fact: &'a Fact,
    keys: &mut HashSet<&'a Identifier>,
    errors: &mut Vec<SpecError>,
) {
    let name = &fact.name;
    if fact.measures.is_empty() {
        errors.push(SpecError::NoMeasures { fact: name.clone() });
    }
    if names_bucket(fact) {
        errors.push(SpecError::NamedBucket { fact: name.clone() });
    }
    for measure in &fact.measures {
        if !keys.insert(&measure.name) {
            errors.push(SpecError::Duplicate {
                scope: Some(name.clone()),
                name: measure.name.clone(),
            });
        }
        check_filter(fact, Some(&measure.name), &measure.filter, errors);
    }
    check_filter(fact, None, &fact.filter, errors);
}

/// Whether a key or measure of the view is named `bucket`.
fn names_bucket(fact: &Fact) -> bool {
    for key in fact.keys() {
        if key.as_str() == "bucket" {
            return true;
        }
    }
    for measure in &fact.measures {
        if measure.name.as_str() == "bucket" {
            return true;
        }
    }
    false
}

/// Every condition of `filter`, a measure's or else the view's, names a
/// key of the view.
fn check_filter(
    fact: &Fact,
    measure: Option<&Identifier>,
    filter: &[Condition],
    errors: &mut Vec<SpecError>,
) {
    for condition in filter {
        if fact.resolve(&condition.key).is_none() {
            errors.push(SpecError::UnknownKey {
                fact: fact.name.clone(),
                measure: measure.cloned(),
                key: condition.key.clone(),
            });
        }
    }
}

/// A via column as a message names it: the fact's, or `ref.column`.
fn via_name(via: &Via, refs: &[Ref]) -> String {
    match via {
        Via::Fact(column) => column.to_string(),
        Via::Attribute { through, column } => {
            let through = match refs.get(*through) {
                Some(join) => join.alias.as_str(),
                None => "?",
            };
            format!("{through}.{column}")
        }
    }
}

/// One thing wrong with a spec. [`Spec::new`] reports every one it finds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecError {
    /// Two declarations share a name; `scope` is the enclosing dim, fact or
    /// ref.
    Duplicate {
        scope: Option<Identifier>,
        name: Identifier,
    },
    /// The `date_bin` width does not divide a day.
    BadWidth { fact: Identifier, seconds: u32 },
    /// A bucket width that isn't a wider whole multiple of the width
    /// before it.
    BadBucket {
        fact: Identifier,
        seconds: u32,
        below: u32,
    },
    /// The time column is a `by` column too, which the catalog would refuse
    /// anyway: a time never groups.
    TimeAsKey {
        fact: Identifier,
        column: Identifier,
    },
    /// A null-ness key over the time column, which could never be true in
    /// an answer: a row with no time is never answered.
    NullTime {
        fact: Identifier,
        name: Identifier,
        column: Identifier,
    },
    /// One join of a view, named by its alias, can't be made as written.
    Join {
        fact: Identifier,
        join: Identifier,
        error: JoinError,
    },
    /// A dim without key columns.
    NoKey { dim: Identifier },
    /// A filter, of a measure or the view's, names something that is no
    /// key of the view.
    UnknownKey {
        fact: Identifier,
        measure: Option<Identifier>,
        key: Identifier,
    },
    /// A view without measures.
    NoMeasures { fact: Identifier },
    /// Asks call the time bucket `bucket`, so nothing else may be.
    NamedBucket { fact: Identifier },
}

/// What is wrong with one join of a view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinError {
    /// The join is on one column twice.
    ViaReused { column: String },
    /// The join is on another number of columns than its dim's key has.
    KeyLength {
        via: usize,
        dim: Identifier,
        key: usize,
    },
    /// The join names a dim the spec has not.
    UnknownDim { dim: Identifier },
    /// A left or inner join that reads nothing, so it does nothing.
    ReadsNothing,
    /// A semi or anti join that reads columns, which it has none of.
    ReadsColumns { kind: RefKind },
    /// The join goes through a dim column that no join before it carries.
    ChainColumn { through: usize, column: Identifier },
    /// The join reads a column its dim has not.
    UnknownAttribute {
        attribute: Identifier,
        dim: Identifier,
    },
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpecError::Duplicate { scope: None, name } => write!(f, "{name} declared twice"),
            SpecError::Duplicate {
                scope: Some(scope),
                name,
            } => write!(f, "{scope}: {name} declared twice"),
            SpecError::BadWidth { fact, seconds } => {
                write!(f, "view {fact}: width of {seconds}s does not divide a day")
            }
            SpecError::BadBucket {
                fact,
                seconds,
                below,
            } => write!(
                f,
                "view {fact}: bucket of {seconds}s is not a wider multiple of {below}s"
            ),
            SpecError::TimeAsKey { fact, column } => write!(
                f,
                "view {fact}: {column} is the time column, so it can't be a by column too"
            ),
            SpecError::NullTime { fact, name, column } => write!(
                f,
                "view {fact}: {name} asks whether the time column {column} is null, \
                 which no ask could see: a row with no time is never answered"
            ),
            SpecError::Join { fact, join, error } => {
                write!(f, "view {fact}: join {join} {error}")
            }
            SpecError::NoKey { dim } => write!(f, "dim {dim}: no key columns"),
            SpecError::UnknownKey {
                fact,
                measure: Some(measure),
                key,
            } => write!(
                f,
                "view {fact}: measure {measure} filters on {key}, which names no key of the view"
            ),
            SpecError::UnknownKey {
                fact,
                measure: None,
                key,
            } => write!(
                f,
                "view {fact}: WHERE filters on {key}, which names no key of the view"
            ),
            SpecError::NoMeasures { fact } => write!(f, "view {fact}: no measures"),
            SpecError::NamedBucket { fact } => write!(
                f,
                "view {fact}: bucket is what asks call the time bucket; \
                 name the column, join or measure otherwise"
            ),
        }
    }
}

/// What follows `view <fact>: join <alias> ` in a [`SpecError::Join`].
impl fmt::Display for JoinError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JoinError::ViaReused { column } => write!(f, "joins on {column} twice"),
            JoinError::KeyLength { via, dim, key } => write!(
                f,
                "joins on {via} columns, but the key of dim {dim} has {key}"
            ),
            JoinError::UnknownDim { dim } => write!(f, "points at unknown dim {dim}"),
            JoinError::ReadsNothing => f.write_str("reads no column of its dim"),
            JoinError::ReadsColumns { kind } => {
                write!(f, "is {kind} and reads columns, which it has none of")
            }
            JoinError::ChainColumn { through, column } => write!(
                f,
                "joins through column {column} of join {through}, \
                 which no join before it carries"
            ),
            JoinError::UnknownAttribute { attribute, dim } => {
                write!(f, "reads {attribute}, which dim {dim} has no column of")
            }
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

impl fmt::Display for Width {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}s", self.seconds)
    }
}
