//! The answer of a variance or standard deviation, from what an ask
//! merged of its cells: the count `n` of values that are not null, their
//! sum `s1` and the sum of their squares `s2`. All three are sums over
//! rows, so cells merge by adding them and a delete subtracts its row's
//! share; integers do both exactly (`research/AGGREGATES.md` §4.5).
//!
//! With `m = s1 / n`, the squared deviations add up to
//! `sum((x - m)^2) = s2 - s1^2 / n`, so
//!
//! - `var_pop = (n * s2 - s1^2) / n^2`
//! - `var_samp = (n * s2 - s1^2) / (n * (n - 1))`
//!
//! and a standard deviation is the root of its variance. `n * s2 - s1^2`
//! is never below zero (Cauchy-Schwarz) and takes up to 254 bits, so it is
//! a [`U256`]. The answer is rounded half to even at [`SPREAD_PLACES`]
//! past the variance's scale, twice the column's, or the deviation's, the
//! column's.

use std::cmp::Ordering;

use crate::value::Value;
use crate::wide::U256;

/// Places past its scale a variance or standard deviation is given to,
/// as an average is.
pub(crate) const SPREAD_PLACES: u32 = 4;

/// `10^SPREAD_PLACES`, the factor a variance's places take.
const VARIANCE_FACTOR: u64 = 10u64.pow(SPREAD_PLACES);

/// `10^(2 * SPREAD_PLACES)`: a root at `SPREAD_PLACES` is taken of a
/// variance at twice as many.
const ROOT_FACTOR: u64 = 10u64.pow(2 * SPREAD_PLACES);

/// Which variance or deviation a measure gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Spread {
    VarSamp,
    VarPop,
    StddevSamp,
    StddevPop,
}

/// What a group's cells merged into for a [`Spread`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Moments {
    /// Values that are not null.
    pub(crate) count: u64,
    pub(crate) sum: i128,
    pub(crate) squares: U256,
}

impl Spread {
    /// The answer for a group, null where PostgreSQL's is: no value, or a
    /// sample of one.
    pub(crate) fn answer(self, moments: Moments) -> Value {
        let count = u128::from(moments.count);
        let divisor = match self {
            Spread::VarSamp | Spread::StddevSamp => count * count.saturating_sub(1),
            Spread::VarPop | Spread::StddevPop => count * count,
        };
        if divisor == 0 {
            return Value::Null;
        }
        let deviation = deviation(moments);
        match self {
            Spread::VarSamp | Spread::VarPop => variance(deviation, divisor),
            Spread::StddevSamp | Spread::StddevPop => root(deviation, divisor),
        }
    }
}

/// `n * s2 - s1^2`. Below zero, or past 2^256, only for cells whose rows
/// went out without coming in; zero then, as PostgreSQL gives for a
/// result at or below zero.
fn deviation(moments: Moments) -> U256 {
    let magnitude = moments.sum.unsigned_abs();
    let squared_sum = U256::product(magnitude, magnitude);
    let (scaled, overflowed) = moments.squares.overflowing_mul(moments.count);
    if overflowed || squared_sum > scaled {
        return U256::ZERO;
    }
    scaled.wrapping_sub(squared_sum)
}

/// `deviation / divisor` at [`SPREAD_PLACES`], rounded half to even.
fn variance(deviation: U256, divisor: u128) -> Value {
    let (whole, rest) = deviation.div_rem(divisor);
    let (places, left) = U256::product(rest, VARIANCE_FACTOR.into()).div_rem(divisor);
    let (shifted, _) = whole.overflowing_mul(VARIANCE_FACTOR);
    let mut units = shifted.wrapping_add(places);
    if rounds_up(left, divisor, units.is_odd()) {
        units = units.wrapping_add(U256::from(1));
    }
    value_of(units)
}

/// The root of `deviation / divisor` at [`SPREAD_PLACES`], rounded half
/// to even. The root of the whole part of a number is the whole part of
/// its root, so the root of `floor(x * 10^8)` has the right digits; what
/// was left of `x` decides only the rounding.
fn root(deviation: U256, divisor: u128) -> Value {
    let (whole, rest) = deviation.div_rem(divisor);
    let (places, left) = U256::product(rest, ROOT_FACTOR.into()).div_rem(divisor);
    let (shifted, _) = whole.overflowing_mul(ROOT_FACTOR);
    // x is `scaled + left / divisor`, and its root rounds up past
    // `(root + 1/2)^2 = root^2 + root + 1/4`.
    let scaled = shifted.wrapping_add(places);
    let root = scaled.sqrt();
    let halfway = U256::product(root, root).wrapping_add(U256::from(root));
    let up = match scaled.cmp(&halfway) {
        Ordering::Greater => true,
        Ordering::Less => false,
        // x lies 1/4 past `halfway` when `left / divisor` is 1/4.
        Ordering::Equal => match U256::product(left, 4).cmp(&U256::from(divisor)) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => root % 2 == 1,
        },
    };
    value_of(U256::from(root).wrapping_add(U256::from(u128::from(up))))
}

/// Whether a quotient rounds up for remainder `rest` of `divisor`: past
/// half, or at half when the quotient is odd.
fn rounds_up(rest: u128, divisor: u128, odd: bool) -> bool {
    let other = divisor - rest;
    rest > other || (rest == other && odd)
}

/// `units` as an `i128` where it fits, as nearly every answer does; wider
/// only for a variance of values near the ends of `bigint`.
fn value_of(units: U256) -> Value {
    let Some(low) = units.to_u128() else {
        return Value::Wide(units);
    };
    match i128::try_from(low) {
        Ok(narrow) => Value::Int(narrow),
        Err(_) => Value::Wide(units),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_rng::Rng;

    fn moments(values: &[i64]) -> Moments {
        let mut moments = Moments {
            count: 0,
            sum: 0,
            squares: U256::ZERO,
        };
        for &value in values {
            moments = merged(moments, single(value));
        }
        moments
    }

    fn single(value: i64) -> Moments {
        let magnitude = u128::from(value.unsigned_abs());
        Moments {
            count: 1,
            sum: value.into(),
            squares: U256::from(magnitude * magnitude),
        }
    }

    fn merged(left: Moments, right: Moments) -> Moments {
        Moments {
            count: left.count + right.count,
            sum: left.sum + right.sum,
            squares: left.squares.wrapping_add(right.squares),
        }
    }

    fn removed(from: Moments, value: i64) -> Moments {
        let row = single(value);
        Moments {
            count: from.count - 1,
            sum: from.sum - row.sum,
            squares: from.squares.wrapping_sub(row.squares),
        }
    }

    /// The squared deviations from the mean times `n^2`, by their
    /// definition and in a pass of their own: `sum((n * x - s1)^2)`.
    fn squared_by_definition(values: &[i64]) -> i128 {
        let count = values.len() as i128;
        let mut sum = 0;
        for &value in values {
            sum += i128::from(value);
        }
        let mut squared = 0;
        for &value in values {
            let apart = count * i128::from(value) - sum;
            squared += apart * apart;
        }
        squared
    }

    /// `numerator / denominator` rounded half to even, as a reference.
    fn rounded(numerator: i128, denominator: i128) -> i128 {
        let (whole, rest) = (numerator / denominator, numerator % denominator);
        match (2 * rest).cmp(&denominator) {
            Ordering::Greater => whole + 1,
            Ordering::Equal if whole % 2 == 1 => whole + 1,
            _ => whole,
        }
    }

    /// The whole `u` nearest `sqrt(numerator / denominator)`, ties to
    /// even: the least `u` whose halfway square `(u + 1/2)^2` is not below
    /// the quotient, found by halving.
    fn rounded_root(numerator: i128, denominator: i128) -> i128 {
        let (mut low, mut high) = (0, 1);
        while !halfway_reaches(high, numerator, denominator) {
            high *= 2;
        }
        while low < high {
            let middle = low.midpoint(high);
            if halfway_reaches(middle, numerator, denominator) {
                high = middle;
            } else {
                low = middle + 1;
            }
        }
        let tie = (2 * low + 1) * (2 * low + 1) * denominator == 4 * numerator;
        if tie && low % 2 == 1 { low + 1 } else { low }
    }

    fn halfway_reaches(root: i128, numerator: i128, denominator: i128) -> bool {
        (2 * root + 1) * (2 * root + 1) * denominator >= 4 * numerator
    }

    fn expected(spread: Spread, values: &[i64]) -> Value {
        let count = values.len() as i128;
        let divisor = match spread {
            Spread::VarSamp | Spread::StddevSamp => count * count * (count - 1),
            Spread::VarPop | Spread::StddevPop => count * count * count,
        };
        if divisor <= 0 {
            return Value::Null;
        }
        let squared = squared_by_definition(values);
        let factor = i128::from(VARIANCE_FACTOR);
        match spread {
            Spread::VarSamp | Spread::VarPop => Value::Int(rounded(squared * factor, divisor)),
            Spread::StddevSamp | Spread::StddevPop => {
                Value::Int(rounded_root(squared * factor * factor, divisor))
            }
        }
    }

    const SPREADS: [Spread; 4] = [
        Spread::VarSamp,
        Spread::VarPop,
        Spread::StddevSamp,
        Spread::StddevPop,
    ];

    /// Values split into cells, merged in any order, some removed: the
    /// answer is the definition's over the values left, at every spread.
    #[test]
    fn merged_cells_answer_as_the_definition_over_the_values() {
        let mut rng = Rng::new(11);
        for _ in 0..3_000 {
            let mut values = Vec::new();
            let magnitude: u64 = [10, 1_000, 100_000][rng.position_below(3)];
            for _ in 0..rng.below(12) {
                let drawn = i64::try_from(rng.below(2 * magnitude)).unwrap();
                values.push(drawn - i64::try_from(magnitude).unwrap());
            }
            let mut total = moments(&[]);
            let mut start = 0;
            while start < values.len() {
                let end = start + 1 + rng.position_below(values.len() - start);
                total = merged(moments(&values[start..end]), total);
                start = end;
            }
            let mut kept = values.clone();
            for _ in 0..rng.position_below(values.len() + 1) {
                let value = kept.swap_remove(rng.position_below(kept.len()));
                total = removed(total, value);
            }
            for spread in SPREADS {
                assert_eq!(
                    spread.answer(total),
                    expected(spread, &kept),
                    "{spread:?} {kept:?}"
                );
            }
        }
    }

    #[test]
    fn answers_known_values() {
        let one_two = moments(&[1, 2]);
        assert_eq!(Spread::VarSamp.answer(one_two), Value::Int(5_000));
        assert_eq!(Spread::VarPop.answer(one_two), Value::Int(2_500));
        // 0.70710678...
        assert_eq!(Spread::StddevSamp.answer(one_two), Value::Int(7_071));
        assert_eq!(Spread::StddevPop.answer(one_two), Value::Int(5_000));
        let same = moments(&[7, 7, 7]);
        for spread in SPREADS {
            assert_eq!(spread.answer(same), Value::Int(0), "{spread:?}");
        }
    }

    /// No value, and for a sample one value, give null; a population of
    /// one has no spread.
    #[test]
    fn too_few_values_give_null() {
        for spread in SPREADS {
            assert_eq!(spread.answer(moments(&[])), Value::Null, "{spread:?}");
        }
        let one = moments(&[5]);
        assert_eq!(Spread::VarSamp.answer(one), Value::Null);
        assert_eq!(Spread::StddevSamp.answer(one), Value::Null);
        assert_eq!(Spread::VarPop.answer(one), Value::Int(0));
        assert_eq!(Spread::StddevPop.answer(one), Value::Int(0));
    }

    /// Exact ties round to the even neighbour: a variance of 0.00005 and a
    /// deviation of 0.00005 both round down to 0.0000, 0.00015 up to 0.0002.
    #[test]
    fn ties_round_to_even() {
        assert_eq!(variance(U256::from(5), 100_000), Value::Int(0));
        assert_eq!(variance(U256::from(15), 100_000), Value::Int(2));
        assert_eq!(variance(U256::from(25), 100_000), Value::Int(2));
        // sqrt(0.0000000025) = 0.00005, and sqrt(0.0000000225) = 0.00015.
        assert_eq!(root(U256::from(25), 10u128.pow(10)), Value::Int(0));
        assert_eq!(root(U256::from(225), 10u128.pow(10)), Value::Int(2));
    }

    /// The widest spread there is: two values at the ends of `bigint`.
    /// Their variance passes an `i128` at four places, and their deviation
    /// does not.
    #[test]
    fn values_at_the_ends_of_bigint_answer_exactly() {
        let ends = moments(&[i64::MIN, i64::MAX]);
        let Value::Wide(units) = Spread::VarSamp.answer(ends) else {
            panic!("a variance past an i128");
        };
        // (2^64 - 1)^2 / 2 at four places, and its root, by python.
        assert_eq!(
            units.to_string(),
            "1701411834604692317132405596421745541125000"
        );
        assert_eq!(
            Spread::StddevSamp.answer(ends),
            Value::Int(130_438_178_253_327_822_116_425)
        );
    }
}
