use std::fmt;
use std::sync::Arc;

use crate::spec::Literal;

/// A column's type. Only the source knows it, so a load sets it per column;
/// asks compare values with it, and answers print numbers in its scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Int,
    /// Values are integers in units of `10^-scale`.
    Numeric {
        scale: u32,
    },
    Bool,
    Text,
    Time,
}

impl ColumnType {
    pub fn scale(self) -> u32 {
        match self {
            ColumnType::Numeric { scale } => scale,
            _ => 0,
        }
    }

    /// Whether `literal` is of the column's type. An integer fits a numeric
    /// column only at scale 0: at scale 2, `1` would read as 0.01.
    pub(crate) fn fits(self, literal: &Literal) -> bool {
        match literal {
            Literal::Int(_) => {
                matches!(self, ColumnType::Int | ColumnType::Numeric { scale: 0 })
            }
            Literal::Text(_) => self == ColumnType::Text,
            Literal::Bool(_) => self == ColumnType::Bool,
        }
    }
}

impl fmt::Display for ColumnType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ColumnType::Int => f.write_str("an integer"),
            ColumnType::Numeric { scale } => write!(f, "a numeric of scale {scale}"),
            ColumnType::Bool => f.write_str("a boolean"),
            ColumnType::Text => f.write_str("text"),
            ColumnType::Time => f.write_str("a timestamptz"),
        }
    }
}

/// One column value. Times are Unix seconds, numeric columns are integers in
/// the column's scale.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Datum {
    Null,
    Bool(bool),
    Int(i64),
    Text(Arc<str>),
}

impl Datum {
    /// SQL `=`: null matches nothing.
    pub(crate) fn matches(&self, lit: &Literal) -> bool {
        match (self, lit) {
            (Datum::Bool(a), Literal::Bool(b)) => a == b,
            (Datum::Int(a), Literal::Int(b)) => a == b,
            (Datum::Text(a), Literal::Text(b)) => **a == **b,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Null,
    /// Sums keep the column's scale.
    Int(i128),
    /// avg, left as sum and count so the caller picks the rounding.
    Avg {
        sum: i128,
        count: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every column type against every kind of value: only the matches
    /// listed fit.
    #[test]
    fn values_fit_only_their_column_type() {
        let types = [
            ColumnType::Int,
            ColumnType::Numeric { scale: 0 },
            ColumnType::Numeric { scale: 2 },
            ColumnType::Bool,
            ColumnType::Text,
            ColumnType::Time,
        ];
        let values = [
            Literal::Int(1),
            Literal::Text("1".into()),
            Literal::Bool(true),
        ];
        let mut fits = Vec::new();
        for column in types {
            for value in &values {
                if column.fits(value) {
                    fits.push((column, value.clone()));
                }
            }
        }
        assert_eq!(
            fits,
            [
                (ColumnType::Int, Literal::Int(1)),
                (ColumnType::Numeric { scale: 0 }, Literal::Int(1)),
                (ColumnType::Bool, Literal::Bool(true)),
                (ColumnType::Text, Literal::Text("1".into())),
            ]
        );
    }

    #[test]
    fn types_display() {
        let shown = [
            ColumnType::Int,
            ColumnType::Numeric { scale: 2 },
            ColumnType::Bool,
            ColumnType::Text,
            ColumnType::Time,
        ]
        .map(|t| t.to_string());
        assert_eq!(
            shown,
            [
                "an integer",
                "a numeric of scale 2",
                "a boolean",
                "text",
                "a timestamptz"
            ]
        );
        let scales = [ColumnType::Numeric { scale: 3 }, ColumnType::Int].map(ColumnType::scale);
        assert_eq!(scales, [3, 0]);
    }
}
