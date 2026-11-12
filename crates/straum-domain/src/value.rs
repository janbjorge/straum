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
    /// Unix seconds. A `timestamptz` is an instant. A `timestamp` has no
    /// zone, so it is kept as the seconds its wall-clock reading would be
    /// in UTC, and buckets and asks over it are wall clock too.
    Time {
        zoned: bool,
    },
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
    pub fn fits(self, literal: &Literal) -> bool {
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
            ColumnType::Time { zoned: true } => f.write_str("a timestamptz"),
            ColumnType::Time { zoned: false } => f.write_str("a timestamp"),
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
    Bool(bool),
    /// avg, left as sum and count so the caller picks the rounding.
    Avg {
        sum: i128,
        count: u64,
    },
}

/// `units` in units of `10^-scale`, exactly, as PostgreSQL prints a
/// numeric: `1250` at scale 2 is `12.50`.
pub fn decimal(units: i128, scale: u32) -> String {
    let digits = units.unsigned_abs().to_string();
    let sign = if units < 0 { "-" } else { "" };
    let scale = scale as usize;
    if scale == 0 {
        return format!("{sign}{digits}");
    }
    let padded = format!("{digits:0>width$}", width = scale + 1);
    let (whole, fraction) = padded.split_at(padded.len() - scale);
    format!("{sign}{whole}.{fraction}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_print_every_digit() {
        let cases = [
            (0, 0, "0"),
            (-7, 0, "-7"),
            (-1, 2, "-0.01"),
            (1250, 2, "12.50"),
            (5, 2, "0.05"),
            (-1250, 3, "-1.250"),
            (0, 3, "0.000"),
            (i128::MIN, 0, "-170141183460469231731687303715884105728"),
            (i128::MAX, 38, "1.70141183460469231731687303715884105727"),
        ];
        for (units, scale, text) in cases {
            assert_eq!(decimal(units, scale), text, "{units} at {scale}");
        }
    }

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
            ColumnType::Time { zoned: true },
            ColumnType::Time { zoned: false },
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
            ColumnType::Time { zoned: true },
            ColumnType::Time { zoned: false },
        ]
        .map(|t| t.to_string());
        assert_eq!(
            shown,
            [
                "an integer",
                "a numeric of scale 2",
                "a boolean",
                "text",
                "a timestamptz",
                "a timestamp"
            ]
        );
        let scales = [ColumnType::Numeric { scale: 3 }, ColumnType::Int].map(ColumnType::scale);
        assert_eq!(scales, [3, 0]);
    }
}
