//! What an ask error says, in Straum SQL's words. The domain finds what is
//! wrong with an ask as plain data ([`AskError`]); this turns each into
//! the line a client reads, naming the clause or spelling to write, so
//! the syntax stays in this crate (ADR-0002).

use std::fmt;

use straum_domain::{
    AskError, ColumnType, Comparison, Identifier, Instead, Literal, MAX_SCALE, Width, decimal,
};

/// The line a client reads for `error`.
pub fn describe_ask_error(error: &AskError) -> String {
    Described { error }.to_string()
}

/// The comparison as an ask writes it.
pub fn comparison_text(comparison: Comparison) -> &'static str {
    match comparison {
        Comparison::Equal => "=",
        Comparison::NotEqual => "<>",
        Comparison::Less => "<",
        Comparison::LessOrEqual => "<=",
        Comparison::Greater => ">",
        Comparison::GreaterOrEqual => ">=",
    }
}

/// An ask error to write.
// `'error`: borrows the error only while it is written.
struct Described<'error> {
    error: &'error AskError,
}

impl fmt::Display for Described<'_> {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.error {
            AskError::UnknownView { name, instead } => {
                write!(out, "unknown view {name}")?;
                write_instead(out, instead, "views")
            }
            AskError::UnknownMeasure { name, instead } => {
                write!(out, "unknown measure {name}")?;
                write_instead(out, instead, "view's measures")
            }
            AskError::UnknownKey { name, instead } => {
                write!(out, "{name} is not a key of the view")?;
                write_instead(out, instead, "view's keys")
            }
            AskError::KeyNotGrouped(name) => write!(out, "{name} is a key; add it to GROUP BY"),
            AskError::NoMeasures => out.write_str("no measures asked"),
            AskError::TimeKeyTwice => out.write_str("more than one time key"),
            AskError::OwnWidth { name, width } => write!(
                out,
                "buckets({name}, '{width}') is the view's own width; write {name}"
            ),
            AskError::NotTime { name, time } => write!(
                out,
                "{name} is not the view's time; buckets and the range read {time}"
            ),
            AskError::TimeInWhere(name) => write!(
                out,
                "{name} is the view's time; bound it with {name} >= and {name} <"
            ),
            AskError::WidthNotKept { width, kept } => write_not_kept(out, *width, kept),
            AskError::AskedTwice(name) => write!(out, "{name} asked twice"),
            AskError::MeasureInWhere(name) => {
                write!(out, "{name} is a measure; compare it in HAVING")
            }
            AskError::KeyInHaving(name) => write!(out, "{name} is a key; filter it in WHERE"),
            AskError::HavingNotSelected(name) => {
                write!(out, "HAVING compares {name}, which the ask doesn't select")
            }
            AskError::NotANumber { measure, value } => {
                write_incomparable(out, measure, "number", value)
            }
            AskError::NotABoolean { measure, value } => {
                write_incomparable(out, measure, "boolean", value)
            }
            AskError::BooleanOrder {
                measure,
                comparison,
            } => write!(
                out,
                "{measure} is a boolean; compare it with = or <>, not {}",
                comparison_text(*comparison)
            ),
            AskError::TooManyDigits { measure, value } => write!(
                out,
                "{measure} can't be compared with {}: at most {MAX_SCALE} digits after the point",
                described(value)
            ),
            AskError::WrongType { key, column, value } => {
                write_wrong_type(out, key, *column, value)
            }
            AskError::RangeEmpty { from, to } => write!(out, "empty range [{from}, {to})"),
            AskError::RangeUnaligned { at, width } => write_unaligned(out, *at, *width),
        }
    }
}

/// A filter value not of its key's type, and how to write one that is.
fn write_wrong_type(
    formatter: &mut fmt::Formatter<'_>,
    key: &Identifier,
    column: ColumnType,
    value: &Literal,
) -> fmt::Result {
    write!(
        formatter,
        "{key} is {column}, so it can't be {}",
        described(value)
    )?;
    match column.spelling() {
        Some(spelling) => write!(formatter, "; {spelling}"),
        None => Ok(()),
    }
}

/// `; did you mean x?`, or `; the {kind} are a, b`, after an error.
fn write_instead(formatter: &mut fmt::Formatter<'_>, instead: &Instead, kind: &str) -> fmt::Result {
    match instead {
        Instead::Closest(name) => write!(formatter, "; did you mean {name}?"),
        Instead::Every(names) if names.is_empty() => Ok(()),
        Instead::Every(names) => write!(formatter, "; the {kind} are {}", joined(names)),
    }
}

fn joined(names: &[Identifier]) -> String {
    let mut texts = Vec::with_capacity(names.len());
    for name in names {
        texts.push(name.as_str());
    }
    texts.join(", ")
}

/// A HAVING value of another kind than its measure's.
fn write_incomparable(
    formatter: &mut fmt::Formatter<'_>,
    measure: &Identifier,
    kind: &str,
    value: &Literal,
) -> fmt::Result {
    write!(
        formatter,
        "{measure} is a {kind}, so it can't be compared with {}",
        described(value)
    )
}

/// [`AskError::WidthNotKept`], naming every width the view keeps.
fn write_not_kept(formatter: &mut fmt::Formatter<'_>, width: Width, kept: &[Width]) -> fmt::Result {
    let mut texts = Vec::with_capacity(kept.len());
    for kept_width in kept {
        texts.push(kept_width.to_string());
    }
    write!(
        formatter,
        "bucket width '{width}' is not kept by the view; it keeps {}",
        texts.join(", ")
    )
}

/// [`AskError::RangeUnaligned`], with the edges on either side, so the
/// next try needn't work out the grid. Past the last edge `i64` holds,
/// only the one before.
fn write_unaligned(formatter: &mut fmt::Formatter<'_>, at: i64, width: Width) -> fmt::Result {
    write!(formatter, "bound {at} is not on a {width} bucket edge")?;
    let before = width.bucket_start(at);
    match before.checked_add(i64::from(width.seconds)) {
        Some(after) => write!(formatter, "; the nearest are {before} and {after}"),
        None => write!(formatter, "; the nearest before it is {before}"),
    }
}

/// A value as an error names it.
fn described(value: &Literal) -> String {
    match value {
        Literal::Int(integer) => format!("the integer {integer}"),
        Literal::Text(text) => format!("the text {text:?}"),
        Literal::Bool(boolean) => format!("the boolean {boolean}"),
        Literal::Decimal { units, scale } => format!("the number {}", decimal(*units, *scale)),
    }
}
