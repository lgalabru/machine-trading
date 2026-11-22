//! Raydium CLMM's integer math, ported from the program at
//! `raydium-io/raydium-clmm@6d7a0f82d2f293c553c69aaf943377367c6c4cc6`,
//! `programs/amm/src/libraries/`: `tick_math.rs`, `sqrt_price_math.rs`,
//! `liquidity_math.rs`, `full_math.rs` and `swap_math.rs`.
//!
//! Prices are `sqrt(token_1 / token_0)` in Q64.64. The program computes
//! with the `uint` crate's `U128`, `U256` and `U512`; every operation it
//! uses is exact (products, quotients rounded down or up, shifts), so
//! `U256` (`protocols::wide`) reproduces it bit for bit. Where the program panics (a `uint`
//! overflow, an inherent `as_u128()` or `as_u64()` of a value too large)
//! the transaction fails, and these functions return an error.

use super::ClmmError;
use crate::protocols::wide::U256;

/// The lowest tick (`tick_math::MIN_TICK`).
pub const MIN_TICK: i32 = -443636;
/// The highest tick (`tick_math::MAX_TICK`).
pub const MAX_TICK: i32 = 443636;
/// `sqrt_price_at_tick(MIN_TICK)`.
pub const MIN_SQRT_PRICE_X64: u128 = 4295048016;
/// `sqrt_price_at_tick(MAX_TICK)`. Raydium's differs from Uniswap's and
/// Orca's by its rounding of the last magic factor.
pub const MAX_SQRT_PRICE_X64: u128 = 79226673521066979257578248091;
/// Fee rates are millionths (`config::FEE_RATE_DENOMINATOR_VALUE`).
pub const FEE_RATE_DENOMINATOR: u32 = 1_000_000;
/// 1.0 in Q64.64.
pub const Q64: u128 = 1 << 64;

/// Bits of the fractional log2 that `get_tick_at_sqrt_price` refines.
const BIT_PRECISION: u32 = 16;

/// `get_sqrt_price_at_tick`: `1.0001^(tick / 2)` in Q64.64.
pub fn sqrt_price_at_tick(tick: i32) -> Result<u128, ClmmError> {
    if !(MIN_TICK..=MAX_TICK).contains(&tick) {
        return Err(ClmmError::InvalidTick("tick outside [MIN_TICK, MAX_TICK]"));
    }
    // Each factor is 2^64 / 1.0001^(2^(i - 1)); `ratio` stays at or below
    // 2^64, so every product fits a u128 before its shift.
    const FACTORS: [u128; 18] = [
        0xfff97272373d4000,
        0xfff2e50f5f657000,
        0xffe5caca7e10f000,
        0xffcb9843d60f7000,
        0xff973b41fa98e800,
        0xff2ea16466c9b000,
        0xfe5dee046a9a3800,
        0xfcbe86c7900bb000,
        0xf987a7253ac65800,
        0xf3392b0822bb6000,
        0xe7159475a2caf000,
        0xd097f3bdfd2f2000,
        0xa9f746462d9f8000,
        0x70d869a156f31c00,
        0x31be135f97ed3200,
        0x9aa508b5b85a500,
        0x5d6af8dedc582c,
        0x2216e584f5fa,
    ];
    let abs_tick = tick.unsigned_abs();
    let mut ratio: u128 = if abs_tick & 1 != 0 {
        0xfffcb933bd6fb800
    } else {
        Q64
    };
    for (bit, factor) in FACTORS.iter().enumerate() {
        if abs_tick & (2 << bit) != 0 {
            ratio = (ratio * factor) >> 64;
        }
    }
    if tick > 0 {
        ratio = u128::MAX / ratio;
    }
    Ok(ratio)
}

/// `get_tick_at_sqrt_price`: the greatest tick whose sqrt price is at or
/// below `sqrt_price_x64`.
pub fn tick_at_sqrt_price(sqrt_price_x64: u128) -> Result<i32, ClmmError> {
    if !(MIN_SQRT_PRICE_X64..MAX_SQRT_PRICE_X64).contains(&sqrt_price_x64) {
        return Err(ClmmError::Math("sqrt price outside the program's range"));
    }
    let msb: u32 = 128 - sqrt_price_x64.leading_zeros() - 1;
    let log2p_integer_x32 = (i128::from(msb) - 64) << 32;
    let mut bit: i128 = 0x8000_0000_0000_0000;
    let mut log2p_fraction_x64: i128 = 0;
    let mut r = if msb >= 64 {
        sqrt_price_x64 >> (msb - 63)
    } else {
        sqrt_price_x64 << (63 - msb)
    };
    for _ in 0..BIT_PRECISION {
        r *= r;
        let is_r_more_than_two = r >> 127;
        r >>= 63 + is_r_more_than_two;
        log2p_fraction_x64 += bit * is_r_more_than_two as i128;
        bit >>= 1;
    }
    let log2p_x32 = log2p_integer_x32 + (log2p_fraction_x64 >> 32);
    let log_sqrt_10001_x64 = log2p_x32 * 59543866431248i128;
    let tick_low = ((log_sqrt_10001_x64 - 184467440737095516i128) >> 64) as i32;
    let tick_high = ((log_sqrt_10001_x64 + 15793534762490258745i128) >> 64) as i32;
    if tick_low == tick_high {
        return Ok(tick_low);
    }
    if sqrt_price_at_tick(tick_high)? <= sqrt_price_x64 {
        Ok(tick_high)
    } else {
        Ok(tick_low)
    }
}

/// `get_price_from_sqrt_price`: the price of token 0 in token 1, Q64.64.
pub fn price_from_sqrt_price(sqrt_price_x64: u128, round_up: bool) -> Result<u128, ClmmError> {
    let mut product = U256::mul_u128(sqrt_price_x64, sqrt_price_x64);
    if round_up {
        product = product
            .checked_add(U256::from_u128(Q64 - 1))
            .ok_or(ClmmError::Math("price rounding"))?;
    }
    product
        .shr(64)
        .to_u128()
        .ok_or(ClmmError::Math("price exceeds u128"))
}

/// `u64::mul_div_floor`: `None` if `denominator` is zero or the result
/// exceeds a u64.
pub fn mul_div_floor_u64(value: u64, multiplier: u64, denominator: u64) -> Option<u64> {
    if denominator == 0 {
        return None;
    }
    u64::try_from(u128::from(value) * u128::from(multiplier) / u128::from(denominator)).ok()
}

/// `u64::mul_div_ceil`.
pub fn mul_div_ceil_u64(value: u64, multiplier: u64, denominator: u64) -> Option<u64> {
    if denominator == 0 {
        return None;
    }
    u64::try_from((u128::from(value) * u128::from(multiplier)).div_ceil(u128::from(denominator)))
        .ok()
}

/// `U128::mul_div_floor` (a 256-bit product): `None` if `denominator` is
/// zero or the result exceeds a u128.
pub fn mul_div_floor_u128(value: u128, multiplier: u128, denominator: u128) -> Option<u128> {
    U256::mul_u128(value, multiplier)
        .mul_div_floor(U256::ONE, U256::from_u128(denominator))?
        .to_u128()
}

/// `U128::mul_div_ceil`.
pub fn mul_div_ceil_u128(value: u128, multiplier: u128, denominator: u128) -> Option<u128> {
    U256::mul_u128(value, multiplier)
        .mul_div_ceil(U256::ONE, U256::from_u128(denominator))?
        .to_u128()
}

/// `liquidity_math::add_delta`.
pub fn add_liquidity_delta(liquidity: u128, delta: i128) -> Result<u128, ClmmError> {
    if delta < 0 {
        liquidity
            .checked_sub(delta.unsigned_abs())
            .ok_or(ClmmError::Math("liquidity underflow crossing a tick"))
    } else {
        liquidity
            .checked_add(delta.unsigned_abs())
            .ok_or(ClmmError::Math("liquidity overflow crossing a tick"))
    }
}

/// `liquidity_math::get_delta_amounts_for_swap`: `(amount_in, amount_out)`
/// for moving between two sqrt prices, the input rounded up and the output
/// down. `Ok(None)` is the program's `MaxTokenOverflow`: an amount past a
/// u64, which `compute_swap` reads as "the target is out of reach".
pub fn delta_amounts_for_swap(
    sqrt_price_a_x64: u128,
    sqrt_price_b_x64: u128,
    liquidity: u128,
    zero_for_one: bool,
) -> Result<Option<(u64, u64)>, ClmmError> {
    let (lower, upper) = if sqrt_price_a_x64 > sqrt_price_b_x64 {
        (sqrt_price_b_x64, sqrt_price_a_x64)
    } else {
        (sqrt_price_a_x64, sqrt_price_b_x64)
    };
    if lower == 0 {
        return Err(ClmmError::Math("zero sqrt price"));
    }
    let amount_1_x64 = U256::mul_u128(liquidity, upper - lower);
    let sqrt_price_product = U256::mul_u128(lower, upper);
    let two_pow_64 = U256::from_u128(Q64);
    let overflow = ClmmError::Math("swap delta");
    let (amount_in, amount_out) = if zero_for_one {
        let amount_in = amount_1_x64
            .mul_div_ceil(two_pow_64, sqrt_price_product)
            .ok_or(overflow.clone())?;
        (amount_in, amount_1_x64.shr(64))
    } else {
        let amount_in = amount_1_x64
            .checked_add(U256::from_u64(u64::MAX))
            .ok_or(overflow.clone())?
            .shr(64);
        let amount_out = amount_1_x64
            .mul_div_floor(two_pow_64, sqrt_price_product)
            .ok_or(overflow)?;
        (amount_in, amount_out)
    };
    match (amount_in.to_u64(), amount_out.to_u64()) {
        (Some(amount_in), Some(amount_out)) => Ok(Some((amount_in, amount_out))),
        _ => Ok(None),
    }
}

/// `get_next_sqrt_price_from_amount_0_rounding_up`.
fn next_sqrt_price_from_amount_0(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount: u64,
    add: bool,
) -> Result<u128, ClmmError> {
    if amount == 0 {
        return Ok(sqrt_price_x64);
    }
    let overflow = ClmmError::Math("next sqrt price from token 0");
    let numerator = U256::from_u128(liquidity).shl(64);
    let product = U256::mul_u128(u128::from(amount), sqrt_price_x64);
    // With `add`, the denominator can't overflow 256 bits (both terms are
    // under 2^193), so the program's fallback branch is never taken.
    let denominator = if add {
        numerator.checked_add(product).ok_or(overflow.clone())?
    } else {
        numerator.checked_sub(product).ok_or(overflow.clone())?
    };
    numerator
        .mul_div_ceil(U256::from_u128(sqrt_price_x64), denominator)
        .and_then(U256::to_u128)
        .ok_or(overflow)
}

/// `get_next_sqrt_price_from_amount_1_rounding_down`.
fn next_sqrt_price_from_amount_1(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount: u64,
    add: bool,
) -> Result<u128, ClmmError> {
    if amount == 0 {
        return Ok(sqrt_price_x64);
    }
    assert!(liquidity > 0, "the callers check liquidity");
    let amount_x64 = u128::from(amount) << 64;
    let overflow = ClmmError::Math("next sqrt price from token 1");
    if add {
        sqrt_price_x64
            .checked_add(amount_x64 / liquidity)
            .ok_or(overflow)
    } else {
        sqrt_price_x64
            .checked_sub(amount_x64.div_ceil(liquidity))
            .ok_or(overflow)
    }
}

fn check_next_sqrt_price_inputs(sqrt_price_x64: u128, liquidity: u128) -> Result<(), ClmmError> {
    if !(MIN_SQRT_PRICE_X64..MAX_SQRT_PRICE_X64).contains(&sqrt_price_x64) {
        return Err(ClmmError::Math("sqrt price outside the program's range"));
    }
    if liquidity == 0 {
        return Err(ClmmError::Math("zero liquidity"));
    }
    Ok(())
}

/// `get_next_sqrt_price_from_input`.
pub fn next_sqrt_price_from_input(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount_in: u64,
    zero_for_one: bool,
) -> Result<u128, ClmmError> {
    check_next_sqrt_price_inputs(sqrt_price_x64, liquidity)?;
    if zero_for_one {
        next_sqrt_price_from_amount_0(sqrt_price_x64, liquidity, amount_in, true)
    } else {
        next_sqrt_price_from_amount_1(sqrt_price_x64, liquidity, amount_in, true)
    }
}

/// `get_next_sqrt_price_from_output`.
pub fn next_sqrt_price_from_output(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount_out: u64,
    zero_for_one: bool,
) -> Result<u128, ClmmError> {
    check_next_sqrt_price_inputs(sqrt_price_x64, liquidity)?;
    if zero_for_one {
        next_sqrt_price_from_amount_1(sqrt_price_x64, liquidity, amount_out, false)
    } else {
        next_sqrt_price_from_amount_0(sqrt_price_x64, liquidity, amount_out, false)
    }
}

/// One step of a swap within a price range of constant liquidity
/// (`swap_math::SwapComputationResult`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SwapStep {
    pub sqrt_price_next_x64: u128,
    /// What enters the pool, before the fee when the fee is on the input.
    pub amount_in: u64,
    /// What leaves the pool, after the fee when the fee is on the output.
    pub amount_out: u64,
    pub fee_amount: u64,
}

/// How a swap step is driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StepMode {
    pub is_base_input: bool,
    pub zero_for_one: bool,
    pub is_fee_on_input: bool,
}

/// `swap_math::compute_swap`: swaps from the current price toward the
/// target, as far as `amount_remaining` goes.
pub fn compute_swap(
    sqrt_price_current_x64: u128,
    sqrt_price_target_x64: u128,
    liquidity: u128,
    amount_remaining: u64,
    fee_rate: u32,
    mode: StepMode,
) -> Result<SwapStep, ClmmError> {
    assert!(fee_rate < FEE_RATE_DENOMINATOR, "fee rate {fee_rate}");
    let overflow = ClmmError::Math("swap step");
    let fee_denominator = u64::from(FEE_RATE_DENOMINATOR);
    let fee_complement = u64::from(FEE_RATE_DENOMINATOR - fee_rate);
    let amount_for_price = match (mode.is_base_input, mode.is_fee_on_input) {
        (true, true) => mul_div_floor_u64(amount_remaining, fee_complement, fee_denominator)
            .ok_or(overflow.clone())?,
        (false, false) => mul_div_ceil_u64(amount_remaining, fee_denominator, fee_complement)
            .ok_or(overflow.clone())?,
        _ => amount_remaining,
    };
    let at_target = delta_amounts_for_swap(
        sqrt_price_target_x64,
        sqrt_price_current_x64,
        liquidity,
        mode.zero_for_one,
    )?
    .filter(|&(amount_in, amount_out)| {
        let user_limit = if mode.is_base_input {
            amount_in
        } else {
            amount_out
        };
        amount_for_price >= user_limit
    });
    let mut step = match at_target {
        Some((amount_in, amount_out)) => SwapStep {
            sqrt_price_next_x64: sqrt_price_target_x64,
            amount_in,
            amount_out,
            fee_amount: 0,
        },
        None => {
            let sqrt_next = if mode.is_base_input {
                next_sqrt_price_from_input(
                    sqrt_price_current_x64,
                    liquidity,
                    amount_for_price,
                    mode.zero_for_one,
                )?
            } else {
                next_sqrt_price_from_output(
                    sqrt_price_current_x64,
                    liquidity,
                    amount_for_price,
                    mode.zero_for_one,
                )?
            };
            let (amount_in, amount_out) = delta_amounts_for_swap(
                sqrt_next,
                sqrt_price_current_x64,
                liquidity,
                mode.zero_for_one,
            )?
            .ok_or(ClmmError::Math("swap step amounts exceed u64"))?;
            SwapStep {
                sqrt_price_next_x64: sqrt_next,
                amount_in,
                amount_out,
                fee_amount: 0,
            }
        }
    };
    let is_within_target = if mode.zero_for_one {
        step.sqrt_price_next_x64 >= sqrt_price_target_x64
    } else {
        sqrt_price_target_x64 >= step.sqrt_price_next_x64
    };
    if !is_within_target {
        return Err(ClmmError::Math("swap step passed its target"));
    }
    apply_step_fee(
        &mut step,
        sqrt_price_target_x64,
        amount_remaining,
        fee_rate,
        mode,
    )?;
    Ok(step)
}

/// The fee half of `compute_swap`.
fn apply_step_fee(
    step: &mut SwapStep,
    sqrt_price_target_x64: u128,
    amount_remaining: u64,
    fee_rate: u32,
    mode: StepMode,
) -> Result<(), ClmmError> {
    let overflow = ClmmError::Math("swap step fee");
    let fee_rate_u64 = u64::from(fee_rate);
    let fee_denominator = u64::from(FEE_RATE_DENOMINATOR);
    let fee_complement = u64::from(FEE_RATE_DENOMINATOR - fee_rate);
    let is_target_reached = step.sqrt_price_next_x64 == sqrt_price_target_x64;
    match (mode.is_base_input, mode.is_fee_on_input) {
        (true, true) => {
            step.fee_amount = if is_target_reached {
                mul_div_ceil_u64(step.amount_in, fee_rate_u64, fee_complement).ok_or(overflow)?
            } else {
                amount_remaining
                    .checked_sub(step.amount_in)
                    .ok_or(overflow)?
            };
        }
        (true, false) => {
            step.fee_amount = mul_div_ceil_u64(step.amount_out, fee_rate_u64, fee_denominator)
                .ok_or(overflow.clone())?;
            step.amount_out = step
                .amount_out
                .checked_sub(step.fee_amount)
                .ok_or(overflow)?;
            // A partial step charges the whole input: the dust its price
            // can't express goes to the pool (the program's fix for an
            // exact-input loop that made no progress).
            if !is_target_reached {
                step.amount_in = amount_remaining;
            }
        }
        (false, true) => {
            step.amount_out = step.amount_out.min(amount_remaining);
            step.fee_amount =
                mul_div_ceil_u64(step.amount_in, fee_rate_u64, fee_complement).ok_or(overflow)?;
        }
        (false, false) => {
            step.fee_amount = mul_div_ceil_u64(step.amount_out, fee_rate_u64, fee_denominator)
                .ok_or(overflow.clone())?;
            let net_output = step
                .amount_out
                .checked_sub(step.fee_amount)
                .ok_or(overflow.clone())?;
            if net_output > amount_remaining {
                step.fee_amount = step
                    .amount_out
                    .checked_sub(amount_remaining)
                    .ok_or(overflow)?;
                step.amount_out = amount_remaining;
            } else {
                step.amount_out = net_output;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqrt_prices_at_the_tick_bounds_are_the_programs_constants() {
        assert_eq!(sqrt_price_at_tick(MIN_TICK), Ok(MIN_SQRT_PRICE_X64));
        assert_eq!(sqrt_price_at_tick(MAX_TICK), Ok(MAX_SQRT_PRICE_X64));
        assert_eq!(sqrt_price_at_tick(0), Ok(Q64));
        assert!(sqrt_price_at_tick(MAX_TICK + 1).is_err());
        assert_eq!(tick_at_sqrt_price(MIN_SQRT_PRICE_X64), Ok(MIN_TICK));
        // The program's own test: MAX_SQRT_PRICE_X64 can't be reached.
        assert_eq!(tick_at_sqrt_price(MAX_SQRT_PRICE_X64 - 1), Ok(MAX_TICK - 1));
        assert!(tick_at_sqrt_price(MAX_SQRT_PRICE_X64).is_err());
    }

    /// `tick_math.rs`'s `tick_round_down`.
    #[test]
    fn tick_at_sqrt_price_rounds_down() {
        for tick in [-28861, 28861] {
            let sqrt_price = sqrt_price_at_tick(tick).unwrap();
            assert_eq!(tick_at_sqrt_price(sqrt_price), Ok(tick));
            assert_eq!(tick_at_sqrt_price(sqrt_price + 1), Ok(tick));
            assert_eq!(
                tick_at_sqrt_price(sqrt_price_at_tick(tick + 1).unwrap() - 1),
                Ok(tick)
            );
            assert_eq!(tick_at_sqrt_price(sqrt_price - 1), Ok(tick - 1));
        }
    }

    /// A sweep of `tick_sqrt_price_roundtrip_exhaustive`, which walks every
    /// tick: every 97th, and both ends.
    #[test]
    fn tick_and_sqrt_price_round_trip() {
        let mut previous = 0;
        for tick in (MIN_TICK..MAX_TICK).step_by(97).chain([MAX_TICK - 1]) {
            let sqrt_price = sqrt_price_at_tick(tick).unwrap();
            assert!(sqrt_price > previous);
            previous = sqrt_price;
            assert_eq!(tick_at_sqrt_price(sqrt_price), Ok(tick), "tick {tick}");
        }
    }

    #[test]
    fn delta_amounts_round_toward_the_pool() {
        let lower = sqrt_price_at_tick(-10).unwrap();
        let upper = sqrt_price_at_tick(10).unwrap();
        let liquidity = 5_124_165_121_219u128;
        let (in_0, out_1) = delta_amounts_for_swap(lower, upper, liquidity, true)
            .unwrap()
            .unwrap();
        let (in_1, out_0) = delta_amounts_for_swap(lower, upper, liquidity, false)
            .unwrap()
            .unwrap();
        // Both directions over the same range: the input rounds up, the
        // output down, so each side's input exceeds the other's output by
        // at most one atom.
        assert!(in_0 >= out_0 && in_0 - out_0 <= 1);
        assert!(in_1 >= out_1 && in_1 - out_1 <= 1);
        // Liquidity this large over the whole range needs more than a u64.
        assert_eq!(
            delta_amounts_for_swap(
                MIN_SQRT_PRICE_X64,
                MAX_SQRT_PRICE_X64,
                u128::from(u64::MAX),
                true
            ),
            Ok(None)
        );
    }

    #[test]
    fn an_exact_input_step_spends_the_input_exactly() {
        let current = sqrt_price_at_tick(0).unwrap();
        let target = sqrt_price_at_tick(-100).unwrap();
        let liquidity = 5_124_165_121_219u128;
        for is_fee_on_input in [true, false] {
            let mode = StepMode {
                is_base_input: true,
                zero_for_one: true,
                is_fee_on_input,
            };
            let step = compute_swap(current, target, liquidity, 1_000_000, 1000, mode).unwrap();
            assert!(step.sqrt_price_next_x64 > target && step.sqrt_price_next_x64 < current);
            let spent = if is_fee_on_input {
                step.amount_in + step.fee_amount
            } else {
                step.amount_in
            };
            assert_eq!(spent, 1_000_000);
            // 0.1% of the gross amount, rounded up.
            assert!(
                step.fee_amount >= 999 && step.fee_amount <= 1001,
                "{step:?}"
            );
        }
    }

    #[test]
    fn an_exact_output_step_delivers_the_output_exactly() {
        let current = sqrt_price_at_tick(0).unwrap();
        let target = sqrt_price_at_tick(100).unwrap();
        let liquidity = 5_124_165_121_219u128;
        for is_fee_on_input in [true, false] {
            let mode = StepMode {
                is_base_input: false,
                zero_for_one: false,
                is_fee_on_input,
            };
            let step = compute_swap(current, target, liquidity, 1_000_000, 1000, mode).unwrap();
            assert_eq!(step.amount_out, 1_000_000);
            assert!(step.amount_in > 1_000_000);
        }
    }

    #[test]
    fn mul_div_reports_overflow_and_zero_denominators() {
        assert_eq!(mul_div_floor_u64(u64::MAX, 2, 1), None);
        assert_eq!(mul_div_ceil_u64(7, 1, 2), Some(4));
        assert_eq!(mul_div_floor_u64(7, 1, 0), None);
        assert_eq!(
            mul_div_floor_u128(u128::MAX, u128::MAX, u128::MAX),
            Some(u128::MAX)
        );
        assert_eq!(mul_div_ceil_u128(u128::MAX, 2, 1), None);
        assert_eq!(
            add_liquidity_delta(1, -2),
            Err(ClmmError::Math("liquidity underflow crossing a tick"))
        );
    }
}
