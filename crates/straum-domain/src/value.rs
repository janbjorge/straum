//! Values in rows, rollup keys and answers.

use std::sync::Arc;

use crate::spec::Literal;

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

/// One measure of one answer row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// sum, min, max or avg over no values.
    Null,
    /// count, count distinct, sum, min or max. Sums keep the column's scale.
    Int(i128),
    /// avg, left as sum and count so the caller picks the rounding.
    Avg { sum: i128, count: u64 },
}
