//! What can be wrong with a spec: every error `Spec::new` reports,
//! and the text a person reads for each. A wrong join of a view is one
//! [`SpecError::Join`] carrying a [`JoinError`], so its text starts
//! with the view and the join it names.

use std::fmt;

use crate::identifier::Identifier;
use crate::spec::{Ref, RefKind, Via};
use crate::width::Width;

/// One thing wrong with a spec. `Spec::new` reports every one it finds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpecError {
    /// Two declarations share a name; `scope` is the enclosing dim, fact or
    /// ref.
    Duplicate {
        scope: Option<Identifier>,
        name: Identifier,
    },
    /// The narrowest width does not divide a day.
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
}

/// What is wrong with one join of a view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinError {
    /// The join is on one column twice.
    ViaReused { column: ViaColumn },
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
    /// The join goes through a dim column that no join before it carries,
    /// named `ref.column` as a via column is.
    ChainColumn { column: ViaColumn },
    /// The join reads a column its dim has not.
    UnknownAttribute {
        attribute: Identifier,
        dim: Identifier,
    },
    /// The join reads a key column of its dim, which its dim holds no
    /// value of: the column it joins on has it.
    ReadsKey { column: Identifier },
    /// The join goes through a semi or anti join, which keeps no row.
    ThroughExists { through: Identifier, kind: RefKind },
}

/// A column a join is on, as an error names it: the fact's, or
/// `join.column` for a column a join before it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViaColumn {
    Fact(Identifier),
    /// `join` is `None` when the via points past the view's joins; the
    /// text then names the join `?`.
    Joined {
        join: Option<Identifier>,
        column: Identifier,
    },
}

impl ViaColumn {
    /// The via column `via` of a join among `refs`, as an error names it.
    pub(crate) fn new(via: &Via, refs: &[Ref]) -> ViaColumn {
        match via {
            Via::Fact(column) => ViaColumn::Fact(column.clone()),
            Via::Attribute { through, column } => {
                let join = match refs.get(*through) {
                    Some(join) => Some(join.alias.clone()),
                    None => None,
                };
                ViaColumn::Joined {
                    join,
                    column: column.clone(),
                }
            }
        }
    }
}

impl fmt::Display for ViaColumn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ViaColumn::Fact(column) => write!(f, "{column}"),
            ViaColumn::Joined {
                join: Some(join),
                column,
            } => write!(f, "{join}.{column}"),
            ViaColumn::Joined { join: None, column } => write!(f, "?.{column}"),
        }
    }
}

impl fmt::Display for SpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SpecError::Duplicate { scope: None, name } => write!(
                f,
                "{name} declared twice: views and dims share one namespace, and a dim \
                 goes by its table's name without the schema"
            ),
            SpecError::Duplicate {
                scope: Some(scope),
                name,
            } => write!(f, "{scope}: {name} declared twice"),
            SpecError::BadWidth { fact, seconds } => {
                let width = Width { seconds: *seconds };
                write!(f, "view {fact}: width of {width} does not divide a day")
            }
            SpecError::BadBucket {
                fact,
                seconds,
                below,
            } => write!(
                f,
                "view {fact}: bucket of {} is not a wider multiple of {}",
                Width { seconds: *seconds },
                Width { seconds: *below }
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
                "view {fact}: the view filters on {key}, which names no key of the view"
            ),
            SpecError::NoMeasures { fact } => write!(f, "view {fact}: no measures"),
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
            JoinError::ReadsNothing => f.write_str(
                "reads no column of its dim; a key column reads the column it is joined to, \
                 so select another",
            ),
            JoinError::ReadsColumns { kind } => {
                write!(f, "is {kind} and reads columns, which it has none of")
            }
            JoinError::ChainColumn { column } => write!(
                f,
                "joins through column {column}, which no join before it carries"
            ),
            JoinError::UnknownAttribute { attribute, dim } => {
                write!(f, "reads {attribute}, which dim {dim} has no column of")
            }
            JoinError::ReadsKey { column } => write!(
                f,
                "reads key column {column}, which the column it joins on holds"
            ),
            JoinError::ThroughExists { through, kind } => write!(
                f,
                "joins through join {through}, which is {kind} and keeps no row"
            ),
        }
    }
}

impl std::error::Error for SpecError {}
