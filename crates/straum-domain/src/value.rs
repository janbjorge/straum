//! The values a row holds and an answer gives back. A load turns each
//! PostgreSQL value into a [`Datum`], typed by its [`ColumnType`]; cells
//! keep a state per input column, and an ask merges them into a [`Value`] per measure, which
//! [`decimal`] prints in its column's scale.

use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;

use crate::date::parse_date;
use crate::spec::Literal;
use crate::uuid::parse_uuid;
use crate::wide::{U256, U256_DIGITS};

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
    /// Days since 2000-01-01, as PostgreSQL keeps a `date`.
    Date,
    /// A `uuid`'s sixteen bytes.
    Uuid,
}

impl ColumnType {
    /// Digits after the point: a numeric column's scale, else 0.
    pub fn scale(self) -> u32 {
        match self {
            ColumnType::Numeric { scale } => scale,
            _ => 0,
        }
    }

    /// Whether `literal` is of the column's type. An integer fits a numeric
    /// column only at scale 0: at scale 2, `1` would read as 0.01. A date
    /// or a uuid is text spelled as PostgreSQL prints it.
    pub fn fits(self, literal: &Literal) -> bool {
        match literal {
            Literal::Int(_) => {
                matches!(self, ColumnType::Int | ColumnType::Numeric { scale: 0 })
            }
            Literal::Text(text) => match self {
                ColumnType::Text => true,
                ColumnType::Date => parse_date(text).is_some(),
                ColumnType::Uuid => parse_uuid(text).is_some(),
                _ => false,
            },
            Literal::Bool(_) => self == ColumnType::Bool,
            // A key compares by `=`, which a key's scale would make depend
            // on digits the column may not keep.
            Literal::Decimal { .. } => false,
        }
    }

    /// How to write a value of the type, for a type whose text has one
    /// spelling of the several PostgreSQL reads.
    pub fn spelling(self) -> Option<&'static str> {
        match self {
            ColumnType::Date => Some("write a date as YYYY-MM-DD, such as 2024-01-15"),
            ColumnType::Uuid => Some(
                "write a uuid as lowercase hex in groups of 8-4-4-4-12, \
                 such as a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11",
            ),
            _ => None,
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
            ColumnType::Date => f.write_str("a date"),
            ColumnType::Uuid => f.write_str("a uuid"),
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
    /// Days since 2000-01-01; `i32::MAX` and `i32::MIN` are `infinity`
    /// and `-infinity`, which order after and before every date, as in
    /// PostgreSQL.
    Date(i32),
    /// Bytes, not a `u128`: its 16-byte alignment would make every datum
    /// a third larger. Bytes order as PostgreSQL orders uuids.
    Uuid([u8; 16]),
}

/// One measure's answer for one group.
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
    /// A number in the measure's scale past what an `i128` holds: a
    /// variance of values near the ends of `bigint`.
    Wide(U256),
}

/// The most digits after the point a HAVING value may have: `10^38` is
/// the largest power of ten an `i128` holds, which an exact comparison
/// divides by.
pub(crate) const MAX_SCALE: u32 = 38;

/// A HAVING value made ready, once per ask, for the values of a measure
/// answered at one scale: scaled there already, so comparing a group's
/// value takes a multiplication at most.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Threshold {
    /// For a `bool_and` or `bool_or`.
    Boolean(bool),
    /// The value in units of the measure's scale, as a fraction: whole
    /// unless the value has more digits after the point than the measure.
    Number(Ratio),
    /// `units * 10^shift` in units of the measure's scale, past what an
    /// `i128` holds, so only a [`Value::Wide`] can reach it.
    Beyond { units: i128, shift: u32 },
}

impl Threshold {
    /// `wanted` for a measure answered in units of `10^-scale`. `None` for
    /// text, or more digits than [`MAX_SCALE`], which the ask's checks
    /// rule out.
    pub(crate) fn new(scale: u32, wanted: &Literal) -> Option<Threshold> {
        let (units, wanted_scale) = match wanted {
            Literal::Bool(wanted) => return Some(Threshold::Boolean(*wanted)),
            Literal::Int(integer) => (i128::from(*integer), 0),
            Literal::Decimal { units, scale } => (*units, *scale),
            Literal::Text(_) => return None,
        };
        if wanted_scale <= scale {
            let shift = scale - wanted_scale;
            let Some(scaled) = scaled_up(units, shift) else {
                return Some(Threshold::Beyond { units, shift });
            };
            return Some(Threshold::Number(Ratio {
                numerator: scaled,
                denominator: 1,
            }));
        }
        let denominator = 10_i128.checked_pow(wanted_scale - scale)?;
        Some(Threshold::Number(Ratio {
            numerator: units,
            denominator,
        }))
    }
}

impl Value {
    /// How the value orders against `threshold`. `None` for null, which no
    /// comparison meets, and for a threshold of another kind, which the
    /// ask's checks rule out.
    pub(crate) fn order_against(&self, threshold: Threshold) -> Option<Ordering> {
        let held = match self {
            Value::Null => return None,
            Value::Bool(held) => {
                let Threshold::Boolean(wanted) = threshold else {
                    return None;
                };
                return Some(held.cmp(&wanted));
            }
            Value::Int(units) => Ratio {
                numerator: *units,
                denominator: 1,
            },
            Value::Avg { sum, count } => Ratio {
                numerator: *sum,
                denominator: i128::from(*count),
            },
            Value::Wide(units) => return wide_order(*units, threshold),
        };
        match threshold {
            Threshold::Number(wanted) => Some(held.order(wanted)),
            Threshold::Beyond { units, .. } if units > 0 => Some(Ordering::Less),
            Threshold::Beyond { .. } => Some(Ordering::Greater),
            Threshold::Boolean(_) => None,
        }
    }
}

/// How a value past an `i128`, never below zero, orders against
/// `threshold`. A `Number` threshold lies within an `i128`, so below it.
fn wide_order(units: U256, threshold: Threshold) -> Option<Ordering> {
    match threshold {
        Threshold::Boolean(_) => None,
        Threshold::Number(_) => Some(Ordering::Greater),
        Threshold::Beyond { units: wanted, .. } if wanted < 0 => Some(Ordering::Greater),
        Threshold::Beyond {
            units: wanted,
            shift,
        } => {
            let mut scaled = U256::from(wanted.unsigned_abs());
            for _ in 0..shift {
                let (product, overflowed) = scaled.overflowing_mul(10);
                if overflowed {
                    return Some(Ordering::Less);
                }
                scaled = product;
            }
            Some(units.cmp(&scaled))
        }
    }
}

/// `units` times `10^places`, if an `i128` holds it.
fn scaled_up(units: i128, places: u32) -> Option<i128> {
    if units == 0 {
        return Some(0);
    }
    units.checked_mul(10_i128.checked_pow(places)?)
}

/// A fraction with a positive denominator.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Ratio {
    numerator: i128,
    denominator: i128,
}

impl Ratio {
    /// Orders the two. Whole numbers compare as they are and fractions by
    /// their cross products; only when a product would overflow does it
    /// take Euclid's steps, which divide.
    // Benchmark: `bench/taxi.py`'s asks by hour and zone with HAVING; an
    // i128 division is a library call, tens of times a multiplication.
    fn order(self, other: Ratio) -> Ordering {
        if self.denominator == 1 && other.denominator == 1 {
            return self.numerator.cmp(&other.numerator);
        }
        let left = self.numerator.checked_mul(other.denominator);
        let right = other.numerator.checked_mul(self.denominator);
        if let (Some(left), Some(right)) = (left, right) {
            return left.cmp(&right);
        }
        self.order_by_euclid(other)
    }

    /// Orders the two by their whole parts, then by the reciprocals of
    /// what remains, as Euclid's algorithm steps, so no product of two
    /// large numbers can overflow.
    fn order_by_euclid(self, other: Ratio) -> Ordering {
        let mut left = self;
        let mut right = other;
        let mut flipped = false;
        loop {
            let whole_left = left.numerator.div_euclid(left.denominator);
            let whole_right = right.numerator.div_euclid(right.denominator);
            let rest_left = left.numerator.rem_euclid(left.denominator);
            let rest_right = right.numerator.rem_euclid(right.denominator);
            let order = match whole_left.cmp(&whole_right) {
                Ordering::Equal => rest_left.cmp(&0).cmp(&rest_right.cmp(&0)),
                unequal => unequal,
            };
            if order != Ordering::Equal || rest_left == 0 {
                return if flipped { order.reverse() } else { order };
            }
            // Both rests lie strictly between 0 and 1; the larger is the
            // one whose reciprocal is smaller.
            left = Ratio {
                numerator: left.denominator,
                denominator: rest_left,
            };
            right = Ratio {
                numerator: right.denominator,
                denominator: rest_right,
            };
            flipped = !flipped;
        }
    }
}

/// `units` in units of `10^-scale`, exactly, as PostgreSQL prints a
/// numeric: `1250` at scale 2 is `12.50`.
pub fn decimal(units: i128, scale: u32) -> String {
    let mut text = String::new();
    push_decimal(&mut text, units, scale);
    text
}

/// Digits an `i128` has at most.
const I128_DIGITS: usize = 39;

/// [`decimal`] onto the end of `out`, without a string of its own.
// Benchmark: `bench/taxi.py`'s ask by hour and zone over six months,
// 509,200 rows: written in place, with no string a value, its JSON took
// 55 ms where it took 252.
pub fn push_decimal(out: &mut String, units: i128, scale: u32) {
    let mut digits = [0; I128_DIGITS];
    let mut first = digits.len();
    let magnitude = units.unsigned_abs();
    // Dividing a u128 is a call; most magnitudes fit a u64.
    if let Ok(mut rest) = u64::try_from(magnitude) {
        loop {
            first -= 1;
            digits[first] = b'0' + (rest % 10) as u8;
            rest /= 10;
            if rest == 0 {
                break;
            }
        }
    } else {
        let mut rest = magnitude;
        while rest > 0 {
            first -= 1;
            digits[first] = b'0' + (rest % 10) as u8;
            rest /= 10;
        }
    }
    if units < 0 {
        out.push('-');
    }
    push_placed(out, &digits[first..], scale);
}

/// A [`U256`] in units of `10^-scale`, as [`push_decimal`] prints an
/// `i128`.
pub fn push_wide_decimal(out: &mut String, units: U256, scale: u32) {
    let mut digits = [0; U256_DIGITS];
    let count = units.digits(&mut digits);
    push_placed(out, &digits[U256_DIGITS - count..], scale);
}

/// `digits`, most significant first, with a point `scale` places from
/// the right.
fn push_placed(out: &mut String, digits: &[u8], scale: u32) {
    let scale = scale as usize;
    if scale == 0 {
        push_digits(out, digits);
        return;
    }
    if digits.len() <= scale {
        out.push_str("0.");
        for _ in digits.len()..scale {
            out.push('0');
        }
        push_digits(out, digits);
        return;
    }
    let (whole, fraction) = digits.split_at(digits.len() - scale);
    push_digits(out, whole);
    out.push('.');
    push_digits(out, fraction);
}

fn push_digits(out: &mut String, digits: &[u8]) {
    for &digit in digits {
        out.push(char::from(digit));
    }
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
            ColumnType::Date,
            ColumnType::Uuid,
        ];
        let uuid = "a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11";
        let values = [
            Literal::Int(1),
            Literal::Text("1".into()),
            Literal::Bool(true),
            Literal::Text("2024-01-15".into()),
            Literal::Text(uuid.into()),
            Literal::Decimal { units: 1, scale: 0 },
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
                (ColumnType::Text, Literal::Text("2024-01-15".into())),
                (ColumnType::Text, Literal::Text(uuid.into())),
                (ColumnType::Date, Literal::Text("2024-01-15".into())),
                (ColumnType::Uuid, Literal::Text(uuid.into())),
            ]
        );
    }

    /// One HAVING case: a value answered at a scale, what it is compared
    /// with, and how it orders.
    type HavingCase = (Value, u32, Literal, Option<Ordering>);

    fn decimal_literal(units: i128, scale: u32) -> Literal {
        Literal::Decimal { units, scale }
    }

    fn assert_orders(cases: Vec<HavingCase>) {
        for (value, scale, wanted, order) in cases {
            let found = match Threshold::new(scale, &wanted) {
                Some(threshold) => value.order_against(threshold),
                None => None,
            };
            assert_eq!(found, order, "{value:?} at {scale} against {wanted:?}");
        }
    }

    /// Each side as an exact fraction: equal across scales, and an
    /// average against its nearest decimals.
    #[test]
    fn numbers_order_exactly_against_a_having_value() {
        use Ordering::{Equal, Greater, Less};
        let third = decimal_literal(333_333_333_333_333_333, 18);
        assert_orders(vec![
            (Value::Int(2050), 2, decimal_literal(205, 1), Some(Equal)),
            (Value::Int(2051), 2, decimal_literal(205, 1), Some(Greater)),
            (Value::Int(205), 1, decimal_literal(20_501, 3), Some(Less)),
            (
                Value::Avg { sum: 1, count: 3 },
                0,
                third.clone(),
                Some(Greater),
            ),
            (Value::Avg { sum: -1, count: 3 }, 0, third, Some(Less)),
            (
                Value::Avg { sum: 15, count: 10 },
                1,
                decimal_literal(15, 2),
                Some(Equal),
            ),
        ]);
    }

    /// A value past an `i128`, as a variance near the ends of `bigint`
    /// is, orders exactly against a HAVING value past one too, and above
    /// any that fits.
    #[test]
    fn wide_values_order_exactly_against_a_having_value() {
        use Ordering::{Equal, Greater, Less};
        // 10^39 units at scale 40, which is 0.1.
        let tenth = Value::Wide(U256::product(10u128.pow(38), 10));
        assert_orders(vec![
            (tenth.clone(), 40, decimal_literal(1, 1), Some(Equal)),
            (tenth.clone(), 40, decimal_literal(2, 1), Some(Less)),
            (tenth.clone(), 40, decimal_literal(9, 2), Some(Greater)),
            (tenth.clone(), 40, Literal::Int(-1), Some(Greater)),
            (tenth.clone(), 40, decimal_literal(0, 0), Some(Greater)),
            (tenth.clone(), 0, Literal::Int(5), Some(Greater)),
            // Past 2^256 at the measure's scale.
            (tenth.clone(), 70, Literal::Int(i64::MAX), Some(Less)),
            (tenth, 0, Literal::Bool(true), None),
        ]);
    }

    #[test]
    fn wide_decimals_print_every_digit() {
        let tenth = U256::product(10u128.pow(38), 10);
        for (scale, printed) in [
            (0, format!("1{}", "0".repeat(39))),
            (40, format!("0.1{}", "0".repeat(39))),
            (42, format!("0.001{}", "0".repeat(39))),
            (2, format!("1{}.00", "0".repeat(37))),
        ] {
            let mut out = String::new();
            push_wide_decimal(&mut out, tenth, scale);
            assert_eq!(out, printed, "at {scale}");
        }
    }

    /// Past an i128 at either end, and nothing for null or a value of
    /// another kind.
    #[test]
    fn extremes_and_other_kinds_order_against_a_having_value() {
        use Ordering::{Equal, Greater, Less};
        let widest = Value::Avg {
            sum: i128::MAX,
            count: u64::MAX,
        };
        assert_orders(vec![
            (widest, 0, Literal::Int(i64::MAX), Some(Greater)),
            (Value::Int(i128::MIN), 0, Literal::Int(i64::MIN), Some(Less)),
            (Value::Int(1), 38, Literal::Int(1), Some(Less)),
            (Value::Int(5), 40, Literal::Int(1), Some(Less)),
            (Value::Int(5), 40, Literal::Int(-1), Some(Greater)),
            (Value::Int(5), 40, Literal::Int(0), Some(Greater)),
            (Value::Bool(true), 0, Literal::Bool(false), Some(Greater)),
            (Value::Bool(true), 0, Literal::Bool(true), Some(Equal)),
            (Value::Null, 0, Literal::Int(0), None),
            (Value::Bool(true), 0, Literal::Int(1), None),
            (Value::Int(1), 0, Literal::Text("1".into()), None),
            (Value::Int(1), 0, decimal_literal(1, 39), None),
        ]);
        // i128::MAX over 100 is ...057.27: the cross products overflow and
        // the whole parts tie, so only the fractions, .27 against .2 and .5,
        // decide.
        let largest = Value::Avg {
            sum: i128::MAX,
            count: 100,
        };
        let point_two = decimal_literal(17_014_118_346_046_923_173_168_730_371_588_410_572, 1);
        let point_five = decimal_literal(17_014_118_346_046_923_173_168_730_371_588_410_575, 1);
        assert_orders(vec![
            (largest.clone(), 0, point_two, Some(Greater)),
            (largest, 0, point_five, Some(Less)),
        ]);
    }

    #[test]
    fn types_display() {
        let types = [
            ColumnType::Int,
            ColumnType::Numeric { scale: 2 },
            ColumnType::Bool,
            ColumnType::Text,
            ColumnType::Time { zoned: true },
            ColumnType::Time { zoned: false },
            ColumnType::Date,
            ColumnType::Uuid,
        ];
        let mut shown = Vec::new();
        for column_type in types {
            shown.push(column_type.to_string());
        }
        assert_eq!(
            shown,
            [
                "an integer",
                "a numeric of scale 2",
                "a boolean",
                "text",
                "a timestamptz",
                "a timestamp",
                "a date",
                "a uuid"
            ]
        );
        let mut spelled = Vec::new();
        for column_type in types {
            spelled.push(column_type.spelling().is_some());
        }
        assert_eq!(
            spelled,
            [false, false, false, false, false, false, true, true]
        );
        let scales = [ColumnType::Numeric { scale: 3 }, ColumnType::Int].map(ColumnType::scale);
        assert_eq!(scales, [3, 0]);
    }
}
