//! Unsigned 256-bit integers, for the concentrated-liquidity programs'
//! sqrt-price math (Orca Whirlpool's `U256Muldiv`, Raydium CLMM's `uint`
//! `U256` and `U512`).
//!
//! Both programs compute with exact integers: a product of two `u128`s, a
//! quotient rounded down or up, a check that a result fits. Any exact
//! implementation therefore reproduces them bit for bit, so this one is the
//! simplest that is exact: little-endian 64-bit limbs, schoolbook
//! multiplication and binary long division. A swap step makes a handful of
//! these operations, so their speed (well under a microsecond each) is not
//! what bounds a quote.
//!
//! What overflows is reported (`None`), never wrapped: the programs fail the
//! transaction where these return `None`, and a quote must fail with them.

use std::cmp::Ordering;

/// An unsigned 256-bit integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct U256([u64; 4]);

// Each adapter uses a different subset; together they use all of it.
#[cfg_attr(
    not(all(feature = "orca-whirlpool-venue", feature = "raydium-clmm-venue")),
    allow(dead_code)
)]
impl U256 {
    #[cfg(test)]
    pub const ZERO: Self = Self([0; 4]);
    pub const ONE: Self = Self([1, 0, 0, 0]);
    #[cfg(test)]
    pub const MAX: Self = Self([u64::MAX; 4]);

    pub const fn from_u128(value: u128) -> Self {
        Self([value as u64, (value >> 64) as u64, 0, 0])
    }

    pub const fn from_u64(value: u64) -> Self {
        Self([value, 0, 0, 0])
    }

    /// The full product of two `u128`s, which always fits.
    pub fn mul_u128(a: u128, b: u128) -> Self {
        let product = mul_words(&Self::from_u128(a).0, &Self::from_u128(b).0);
        debug_assert!(product[4..].iter().all(|&w| w == 0));
        Self([product[0], product[1], product[2], product[3]])
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0; 4]
    }

    /// The value, if it fits a `u128`.
    pub fn to_u128(self) -> Option<u128> {
        if self.0[2] != 0 || self.0[3] != 0 {
            return None;
        }
        Some(u128::from(self.0[0]) | (u128::from(self.0[1]) << 64))
    }

    /// The value, if it fits a `u64`.
    pub fn to_u64(self) -> Option<u64> {
        if self.0[1..].iter().any(|&w| w != 0) {
            return None;
        }
        Some(self.0[0])
    }

    pub fn checked_add(self, other: Self) -> Option<Self> {
        let (sum, carry) = add_words(&self.0, &other.0);
        (!carry).then_some(Self(sum))
    }

    pub fn checked_sub(self, other: Self) -> Option<Self> {
        let (difference, borrow) = sub_words(&self.0, &other.0);
        (!borrow).then_some(Self(difference))
    }

    /// `self << bits`, if no set bit is shifted out.
    pub fn checked_shl(self, bits: u32) -> Option<Self> {
        assert!(bits < 256, "shift of {bits} bits");
        if bits > 0 && self.leading_zeros() < bits {
            return None;
        }
        Some(self.shl(bits))
    }

    /// `self << bits`; bits shifted past the top are dropped, as the `uint`
    /// crate's `<<` drops them.
    pub fn shl(self, bits: u32) -> Self {
        assert!(bits < 256, "shift of {bits} bits");
        Self(shl_words(&self.0, bits))
    }

    pub fn shr(self, bits: u32) -> Self {
        assert!(bits < 256, "shift of {bits} bits");
        Self(shr_words(&self.0, bits))
    }

    pub fn leading_zeros(&self) -> u32 {
        leading_zeros_words(&self.0)
    }

    /// The quotient and remainder of `self / divisor`.
    pub fn div_rem(self, divisor: Self) -> (Self, Self) {
        assert!(!divisor.is_zero(), "division by zero");
        let (quotient, remainder) = div_rem_words(&self.0, &divisor.0);
        (Self(quotient), Self(remainder))
    }

    /// `self / divisor`, rounded up when `round_up` and inexact.
    pub fn div_round(self, divisor: Self, round_up: bool) -> Option<Self> {
        let (quotient, remainder) = self.div_rem(divisor);
        if round_up && !remainder.is_zero() {
            return quotient.checked_add(Self::ONE);
        }
        Some(quotient)
    }

    /// `self * multiplier / divisor` over the full 512-bit product, rounded
    /// down; `None` if `divisor` is zero or the result needs more than 256
    /// bits.
    pub fn mul_div_floor(self, multiplier: Self, divisor: Self) -> Option<Self> {
        self.mul_div(multiplier, divisor, false)
    }

    /// [`mul_div_floor`](Self::mul_div_floor), rounded up.
    pub fn mul_div_ceil(self, multiplier: Self, divisor: Self) -> Option<Self> {
        self.mul_div(multiplier, divisor, true)
    }

    fn mul_div(self, multiplier: Self, divisor: Self, round_up: bool) -> Option<Self> {
        if divisor.is_zero() {
            return None;
        }
        let product = mul_words(&self.0, &multiplier.0);
        let mut divisor_wide = [0u64; 8];
        divisor_wide[..4].copy_from_slice(&divisor.0);
        let (mut quotient, remainder) = div_rem_words(&product, &divisor_wide);
        if round_up && remainder != [0; 8] {
            let (sum, carry) = add_words(&quotient, &one_words());
            if carry {
                return None;
            }
            quotient = sum;
        }
        if quotient[4..].iter().any(|&w| w != 0) {
            return None;
        }
        Some(Self([quotient[0], quotient[1], quotient[2], quotient[3]]))
    }
}

impl Ord for U256 {
    fn cmp(&self, other: &Self) -> Ordering {
        cmp_words(&self.0, &other.0)
    }
}

impl PartialOrd for U256 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg_attr(
    not(all(feature = "orca-whirlpool-venue", feature = "raydium-clmm-venue")),
    allow(dead_code)
)]
fn one_words<const N: usize>() -> [u64; N] {
    let mut one = [0u64; N];
    one[0] = 1;
    one
}

fn cmp_words<const N: usize>(a: &[u64; N], b: &[u64; N]) -> Ordering {
    for i in (0..N).rev() {
        match a[i].cmp(&b[i]) {
            Ordering::Equal => continue,
            unequal => return unequal,
        }
    }
    Ordering::Equal
}

fn add_words<const N: usize>(a: &[u64; N], b: &[u64; N]) -> ([u64; N], bool) {
    let mut sum = [0u64; N];
    let mut carry = false;
    for i in 0..N {
        let (partial, carry_a) = a[i].overflowing_add(b[i]);
        let (total, carry_b) = partial.overflowing_add(u64::from(carry));
        sum[i] = total;
        carry = carry_a || carry_b;
    }
    (sum, carry)
}

fn sub_words<const N: usize>(a: &[u64; N], b: &[u64; N]) -> ([u64; N], bool) {
    let mut difference = [0u64; N];
    let mut borrow = false;
    for i in 0..N {
        let (partial, borrow_a) = a[i].overflowing_sub(b[i]);
        let (total, borrow_b) = partial.overflowing_sub(u64::from(borrow));
        difference[i] = total;
        borrow = borrow_a || borrow_b;
    }
    (difference, borrow)
}

/// The full 512-bit product of two 256-bit values.
fn mul_words(a: &[u64; 4], b: &[u64; 4]) -> [u64; 8] {
    let mut product = [0u64; 8];
    for i in 0..4 {
        let mut carry: u128 = 0;
        for j in 0..4 {
            let term = u128::from(a[i]) * u128::from(b[j]) + u128::from(product[i + j]) + carry;
            product[i + j] = term as u64;
            carry = term >> 64;
        }
        product[i + 4] = carry as u64;
    }
    product
}

fn leading_zeros_words<const N: usize>(a: &[u64; N]) -> u32 {
    let mut zeros = 0;
    for i in (0..N).rev() {
        if a[i] != 0 {
            return zeros + a[i].leading_zeros();
        }
        zeros += 64;
    }
    zeros
}

fn shl_words<const N: usize>(a: &[u64; N], bits: u32) -> [u64; N] {
    let word_shift = (bits / 64) as usize;
    let bit_shift = bits % 64;
    let mut shifted = [0u64; N];
    for i in (word_shift..N).rev() {
        let source = i - word_shift;
        shifted[i] = a[source] << bit_shift;
        if bit_shift > 0 && source > 0 {
            shifted[i] |= a[source - 1] >> (64 - bit_shift);
        }
    }
    shifted
}

fn shr_words<const N: usize>(a: &[u64; N], bits: u32) -> [u64; N] {
    let word_shift = (bits / 64) as usize;
    let bit_shift = bits % 64;
    let mut shifted = [0u64; N];
    for (i, word) in shifted
        .iter_mut()
        .enumerate()
        .take(N.saturating_sub(word_shift))
    {
        let source = i + word_shift;
        *word = a[source] >> bit_shift;
        if bit_shift > 0 && source + 1 < N {
            *word |= a[source + 1] << (64 - bit_shift);
        }
    }
    shifted
}

/// Binary long division, from the dividend's highest set bit down: at most
/// `64 * N` steps, each a shift, a compare and a subtraction.
fn div_rem_words<const N: usize>(dividend: &[u64; N], divisor: &[u64; N]) -> ([u64; N], [u64; N]) {
    assert!(*divisor != [0; N], "division by zero");
    let mut quotient = [0u64; N];
    let mut remainder = [0u64; N];
    let bits = (64 * N) as u32 - leading_zeros_words(dividend);
    for bit in (0..bits).rev() {
        // The remainder is below the divisor, so doubling it overflows at
        // most into one carry bit, which the subtraction below absorbs.
        let carry = remainder[N - 1] >> 63 == 1;
        remainder = shl_words(&remainder, 1);
        remainder[0] |= (dividend[(bit / 64) as usize] >> (bit % 64)) & 1;
        if carry || cmp_words(&remainder, divisor) != Ordering::Less {
            remainder = sub_words(&remainder, divisor).0;
            quotient[(bit / 64) as usize] |= 1 << (bit % 64);
        }
    }
    debug_assert!(cmp_words(&remainder, divisor) == Ordering::Less);
    (quotient, remainder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn products_and_quotients_of_u128_values_match_native_arithmetic() {
        let values = [
            0u128,
            1,
            3,
            u64::MAX as u128,
            1 << 64,
            u128::MAX / 3,
            u128::MAX,
        ];
        for &a in &values {
            for &b in &values {
                let product = U256::mul_u128(a, b);
                if let Some(native) = a.checked_mul(b) {
                    assert_eq!(product.to_u128(), Some(native));
                }
                if b != 0 {
                    let (quotient, remainder) = product.div_rem(U256::from_u128(b));
                    assert_eq!(
                        quotient,
                        if a == 0 {
                            U256::ZERO
                        } else {
                            U256::from_u128(a)
                        }
                    );
                    assert!(remainder.is_zero());
                    let (quotient, remainder) = U256::from_u128(a).div_rem(U256::from_u128(b));
                    assert_eq!(quotient.to_u128(), Some(a / b));
                    assert_eq!(remainder.to_u128(), Some(a % b));
                }
            }
        }
    }

    #[test]
    fn mul_div_uses_the_full_product_and_rounds_as_asked() {
        let max = U256::MAX;
        // MAX * MAX / MAX needs 512 bits in between.
        assert_eq!(max.mul_div_floor(max, max), Some(max));
        assert_eq!(max.mul_div_floor(max, U256::ONE), None);
        assert_eq!(max.mul_div_floor(U256::ONE, U256::ZERO), None);
        let seven = U256::from_u64(7);
        let two = U256::from_u64(2);
        assert_eq!(seven.mul_div_floor(U256::ONE, two), Some(U256::from_u64(3)));
        assert_eq!(seven.mul_div_ceil(U256::ONE, two), Some(U256::from_u64(4)));
        assert_eq!(max.mul_div_ceil(U256::ONE, U256::ONE), Some(max));
    }

    #[test]
    fn shifts_report_or_drop_lost_bits() {
        let top = U256::ONE.shl(255);
        assert_eq!(top.checked_shl(1), None);
        assert_eq!(top.shl(1), U256::ZERO);
        assert_eq!(top.shr(255), U256::ONE);
        assert_eq!(
            U256::from_u128(u128::MAX)
                .checked_shl(64)
                .map(|v| v.shr(64)),
            Some(U256::from_u128(u128::MAX))
        );
        assert_eq!(U256::from_u128(5).shl(130).shr(130), U256::from_u128(5));
    }

    #[test]
    fn ordering_compares_the_high_limbs_first() {
        let low = U256::from_u128(u128::MAX);
        let high = U256::ONE.shl(128);
        assert!(low < high);
        assert_eq!(high.checked_sub(low), Some(U256::ONE));
        assert_eq!(low.checked_sub(high), None);
        assert_eq!(U256::MAX.checked_add(U256::ONE), None);
    }
}
