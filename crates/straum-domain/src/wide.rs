//! A 256-bit unsigned integer, for what a variance needs past a `u128`:
//! a cell's sum of squares, and the products and quotient its answer
//! takes. `std` stops at 128 bits (ADR-0003). Every number here is at
//! least zero, as a sum of squares and `n * sum of squares - sum^2` are,
//! so there is no sign.
//!
//! Add and subtract wrap, as integer sums do: arithmetic modulo 2^256 is
//! exact for any result that fits, whatever the order of adds and
//! subtracts that led to it.

use std::fmt;

/// An unsigned integer of 256 bits, the high half first so the derived
/// order is the numeric one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct U256 {
    high: u128,
    low: u128,
}

/// The low 64 bits of a `u128`.
const LOW_64: u128 = u64::MAX as u128;

/// Digits a `U256` has at most: 2^256 is about 1.2 * 10^77.
pub(crate) const U256_DIGITS: usize = 78;

/// The largest power of ten a `u64` holds, so printing divides by it
/// and takes 19 digits at a time.
const TEN_TO_19: u128 = 10_000_000_000_000_000_000;

impl U256 {
    pub const ZERO: U256 = U256 { high: 0, low: 0 };

    pub const fn from_u128(number: u128) -> U256 {
        U256 {
            high: 0,
            low: number,
        }
    }

    /// The whole product of two `u128`s, which no `u128` holds.
    pub fn product(left: u128, right: u128) -> U256 {
        let (left_high, left_low) = (left >> 64, left & LOW_64);
        let (right_high, right_low) = (right >> 64, right & LOW_64);
        let low_low = left_low * right_low;
        let low_high = left_low * right_high;
        let high_low = left_high * right_low;
        let high_high = left_high * right_high;
        // Three numbers below 2^64 each, so the sum fits.
        let middle = (low_low >> 64) + (low_high & LOW_64) + (high_low & LOW_64);
        U256 {
            high: high_high + (low_high >> 64) + (high_low >> 64) + (middle >> 64),
            low: (middle << 64) | (low_low & LOW_64),
        }
    }

    #[must_use]
    pub fn wrapping_add(self, other: U256) -> U256 {
        let (low, carry) = self.low.overflowing_add(other.low);
        U256 {
            high: self
                .high
                .wrapping_add(other.high)
                .wrapping_add(u128::from(carry)),
            low,
        }
    }

    #[must_use]
    pub fn wrapping_sub(self, other: U256) -> U256 {
        let (low, borrow) = self.low.overflowing_sub(other.low);
        U256 {
            high: self
                .high
                .wrapping_sub(other.high)
                .wrapping_sub(u128::from(borrow)),
            low,
        }
    }

    /// `self * factor` modulo 2^256, and whether it passed 2^256.
    pub fn overflowing_mul(self, factor: u64) -> (U256, bool) {
        let low = U256::product(self.low, factor.into());
        let high = U256::product(self.high, factor.into());
        let (top, carried) = high.low.overflowing_add(low.high);
        let product = U256 {
            high: top,
            low: low.low,
        };
        (product, high.high != 0 || carried)
    }

    /// The number as a `u128`, if it fits.
    pub fn to_u128(self) -> Option<u128> {
        if self.high == 0 { Some(self.low) } else { None }
    }

    /// The quotient and remainder of dividing by `divisor`, which is not 0.
    pub fn div_rem(self, divisor: u128) -> (U256, u128) {
        if self.high == 0 {
            return (U256::from_u128(self.low / divisor), self.low % divisor);
        }
        // Long division a bit at a time: the high half is set only for
        // values near the ends of `bigint`, so speed matters little here.
        let mut quotient = U256::ZERO;
        let mut rest: u128 = 0;
        for bit in (0..256).rev() {
            // The bit shifted out of `rest`: when it is set, `rest` stands
            // for 2^128 more than it holds, which is more than any divisor.
            let carried = rest >> 127 == 1;
            rest = (rest << 1) | self.bit(bit);
            if carried || rest >= divisor {
                rest = rest.wrapping_sub(divisor);
                quotient = quotient.with_bit(bit);
            }
        }
        (quotient, rest)
    }

    /// The largest whole number whose square is at most `self`.
    pub fn sqrt(self) -> u128 {
        if self.high == 0 {
            return self.low.isqrt();
        }
        // A root of a number below 2^256 is below 2^128: set its bits from
        // the top, keeping each whose square still fits under `self`.
        let mut root: u128 = 0;
        for bit in (0..128).rev() {
            let candidate = root | 1 << bit;
            if U256::product(candidate, candidate) <= self {
                root = candidate;
            }
        }
        root
    }

    /// The decimal digits, most significant first, into the end of
    /// `digits`; how many there are.
    pub(crate) fn digits(self, digits: &mut [u8; U256_DIGITS]) -> usize {
        let mut first = digits.len();
        let mut rest = self;
        // 19 digits a division, while the number is past a `u128`.
        while rest.high != 0 {
            let (quotient, mut chunk) = rest.div_rem(TEN_TO_19);
            for _ in 0..19 {
                first -= 1;
                digits[first] = b'0' + (chunk % 10) as u8;
                chunk /= 10;
            }
            rest = quotient;
        }
        let mut low = rest.low;
        loop {
            first -= 1;
            digits[first] = b'0' + (low % 10) as u8;
            low /= 10;
            if low == 0 {
                break;
            }
        }
        digits.len() - first
    }

    fn bit(self, bit: u32) -> u128 {
        if bit >= 128 {
            (self.high >> (bit - 128)) & 1
        } else {
            (self.low >> bit) & 1
        }
    }

    fn with_bit(self, bit: u32) -> U256 {
        if bit >= 128 {
            U256 {
                high: self.high | 1 << (bit - 128),
                low: self.low,
            }
        } else {
            U256 {
                high: self.high,
                low: self.low | 1 << bit,
            }
        }
    }
}

impl From<u128> for U256 {
    fn from(number: u128) -> U256 {
        U256::from_u128(number)
    }
}

impl fmt::Display for U256 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut digits = [0; U256_DIGITS];
        let count = self.digits(&mut digits);
        let text = std::str::from_utf8(&digits[U256_DIGITS - count..]).expect("ASCII digits");
        formatter.write_str(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_rng::Rng;

    /// A `u128` of random width, so small, middling and full-width numbers
    /// all come up.
    fn any_u128(rng: &mut Rng) -> u128 {
        let number = (u128::from(rng.next()) << 64) | u128::from(rng.next());
        let bits = u32::try_from(rng.below(129)).unwrap();
        if bits == 128 {
            number
        } else {
            number & ((1 << bits) - 1)
        }
    }

    fn any_u256(rng: &mut Rng) -> U256 {
        U256 {
            high: any_u128(rng),
            low: any_u128(rng),
        }
    }

    #[test]
    fn a_product_that_fits_a_u128_is_the_u128_product() {
        let mut rng = Rng::new(1);
        for _ in 0..10_000 {
            let (left, right) = (u128::from(rng.next()), u128::from(rng.next()));
            assert_eq!(U256::product(left, right), U256::from(left * right));
        }
    }

    /// Dividing `quotient * divisor + remainder` by `divisor` gives back
    /// both, at every width: the product, the division and the order agree.
    #[test]
    fn dividing_a_product_gives_back_its_factor_and_remainder() {
        let mut rng = Rng::new(2);
        for _ in 0..10_000 {
            let divisor = any_u128(&mut rng).max(1);
            let quotient = any_u128(&mut rng);
            let remainder = any_u128(&mut rng) % divisor;
            let number = U256::product(quotient, divisor).wrapping_add(U256::from(remainder));
            assert_eq!(
                number.div_rem(divisor),
                (U256::from(quotient), remainder),
                "{quotient} * {divisor} + {remainder}"
            );
        }
    }

    #[test]
    fn subtracting_undoes_adding_across_the_wrap() {
        let mut rng = Rng::new(3);
        for _ in 0..10_000 {
            let (start, step) = (any_u256(&mut rng), any_u256(&mut rng));
            assert_eq!(start.wrapping_add(step).wrapping_sub(step), start);
        }
        let top = U256 {
            high: u128::MAX,
            low: u128::MAX,
        };
        assert_eq!(top.wrapping_add(U256::from(1)), U256::ZERO);
        assert_eq!(U256::ZERO.wrapping_sub(U256::from(1)), top);
    }

    /// A product by a `u64` is the whole product while it fits, and
    /// says so once it passes 2^256, wrapping as an add does.
    #[test]
    fn multiplying_by_a_u64_says_when_it_passes_the_top() {
        let mut rng = Rng::new(4);
        for _ in 0..10_000 {
            let (number, factor) = (any_u128(&mut rng), rng.next());
            assert_eq!(
                U256::from(number).overflowing_mul(factor),
                (U256::product(number, factor.into()), false)
            );
        }
        let half = U256 {
            high: 1 << 127,
            low: 0,
        };
        assert_eq!(half.overflowing_mul(2), (U256::ZERO, true));
        assert_eq!(half.overflowing_mul(3), (half, true));
        assert_eq!(half.overflowing_mul(1), (half, false));
        let top_bit_low = U256 {
            high: u128::MAX,
            low: 1 << 127,
        };
        assert!(top_bit_low.overflowing_mul(2).1);
    }

    /// The root squared is at most the number, and one more squared is
    /// past it.
    #[test]
    fn a_root_is_the_largest_whose_square_fits() {
        let mut rng = Rng::new(5);
        for _ in 0..2_000 {
            let number = any_u256(&mut rng);
            let root = number.sqrt();
            assert!(U256::product(root, root) <= number, "{number}");
            if let Some(next) = root.checked_add(1) {
                assert!(U256::product(next, next) > number, "{number}");
            }
        }
        let root = u128::MAX;
        assert_eq!(U256::product(root, root).sqrt(), root);
        assert_eq!(
            U256::product(root, root).wrapping_sub(U256::from(1)).sqrt(),
            root - 1
        );
    }

    #[test]
    fn prints_every_digit() {
        let top = U256 {
            high: u128::MAX,
            low: u128::MAX,
        };
        assert_eq!(
            top.to_string(),
            "115792089237316195423570985008687907853269984665640564039457584007913129639935"
        );
        assert_eq!(
            U256::product(1 << 64, 1 << 64).to_string(),
            "340282366920938463463374607431768211456"
        );
        assert_eq!(
            U256::product(u128::MAX, 10).to_string(),
            "3402823669209384634633746074317682114550"
        );
        // Chunks of 19 digits that are all zeros keep them.
        assert_eq!(
            U256::product(10u128.pow(38), 10).to_string(),
            format!("1{}", "0".repeat(39))
        );
        assert_eq!(U256::ZERO.to_string(), "0");
        assert_eq!(U256::from(u128::MAX).to_u128(), Some(u128::MAX));
        assert_eq!(U256::product(1 << 64, 1 << 64).to_u128(), None);
    }
}
