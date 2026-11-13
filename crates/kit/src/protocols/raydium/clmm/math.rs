//! Concentrated-liquidity math, Uniswap v3's as Raydium runs it: a price is
//! token 1 atoms per token 0 atom, a tick `t` is the price 1.0001^t, and a
//! position of liquidity `L` between two ticks holds what the price's path
//! through its range leaves it.
//!
//! What the grader reads (a position's amounts, a price, a tick for a
//! price) is in `f64`: it values and reports, it doesn't settle. A
//! position's fees are counted exactly, as the program counts them.

use super::{PersonalPosition, PoolState, Tick};

/// The lowest and highest ticks the program allows.
pub const MIN_TICK: i32 = -443_636;
pub const MAX_TICK: i32 = 443_636;
/// The sqrt prices of [`MIN_TICK`] and [`MAX_TICK`], in Q64.64: a swap's
/// price limit lies strictly between them.
pub const MIN_SQRT_PRICE_X64: u128 = 4_295_048_016;
pub const MAX_SQRT_PRICE_X64: u128 = 79_226_673_521_066_979_257_578_248_091;

const Q64: f64 = 18_446_744_073_709_551_616.0;

/// Token 1 per token 0, in whole units, at a sqrt price.
pub fn price_from_sqrt_x64(sqrt_price_x64: u128, decimals_0: u8, decimals_1: u8) -> f64 {
    let sqrt = sqrt_price_x64 as f64 / Q64;
    sqrt * sqrt * 10f64.powi(i32::from(decimals_0) - i32::from(decimals_1))
}

/// The sqrt price, in Q64.64, of `price` token 1 per token 0 in whole units,
/// held strictly inside the program's bounds.
pub fn sqrt_x64_from_price(price: f64, decimals_0: u8, decimals_1: u8) -> u128 {
    let atoms = price * 10f64.powi(i32::from(decimals_1) - i32::from(decimals_0));
    let sqrt = (atoms.max(0.0).sqrt() * Q64) as u128;
    sqrt.clamp(MIN_SQRT_PRICE_X64 + 1, MAX_SQRT_PRICE_X64 - 1)
}

/// The sqrt price at `tick`, as a plain number of √(atoms per atom).
pub fn sqrt_at_tick(tick: i32) -> f64 {
    1.0001f64.powf(f64::from(tick) / 2.0)
}

/// The highest tick at or below `price` token 1 per token 0 (whole units),
/// on the `tick_spacing` grid and within the program's bounds.
pub fn tick_at_price(price: f64, decimals_0: u8, decimals_1: u8, tick_spacing: u16) -> i32 {
    let atoms = price * 10f64.powi(i32::from(decimals_1) - i32::from(decimals_0));
    let spacing = i32::from(tick_spacing);
    let tick = if atoms > 0.0 {
        (atoms.ln() / 1.0001f64.ln()).floor() as i32
    } else {
        MIN_TICK
    };
    let lowest = MIN_TICK.div_euclid(spacing) * spacing + spacing;
    let highest = MAX_TICK.div_euclid(spacing) * spacing;
    (tick.div_euclid(spacing) * spacing).clamp(lowest, highest)
}

/// What liquidity `liquidity` between sqrt prices `lower` and `upper` holds
/// at `current`, in atoms of token 0 and token 1.
pub fn amounts_for_liquidity(liquidity: u128, current: f64, lower: f64, upper: f64) -> (f64, f64) {
    let l = liquidity as f64;
    if current <= lower {
        (l * (upper - lower) / (lower * upper), 0.0)
    } else if current >= upper {
        (0.0, l * (upper - lower))
    } else {
        (
            l * (upper - current) / (current * upper),
            l * (current - lower),
        )
    }
}

/// The liquidity `amount_0` and `amount_1` atoms each buy between sqrt
/// prices `lower` and `upper` at `current`: `None` for a token the range
/// doesn't take at that price.
pub fn liquidity_for_amounts(
    amount_0: f64,
    amount_1: f64,
    current: f64,
    lower: f64,
    upper: f64,
) -> (Option<f64>, Option<f64>) {
    let from_0 = |from: f64| amount_0 * from * upper / (upper - from);
    let from_1 = |to: f64| amount_1 / (to - lower);
    if current <= lower {
        (Some(from_0(lower)), None)
    } else if current >= upper {
        (None, Some(from_1(upper)))
    } else {
        (Some(from_0(current)), Some(from_1(current)))
    }
}

/// Fees per unit of liquidity earned inside `[lower, upper]`, for each
/// token, as the program counts them (wrapping, in Q64.64).
pub fn fee_growth_inside(pool: &PoolState, lower: &Tick, upper: &Tick) -> (u128, u128) {
    let side = |global: u128, lower_out: u128, upper_out: u128| {
        let below = if pool.tick_current >= lower.tick {
            lower_out
        } else {
            global.wrapping_sub(lower_out)
        };
        let above = if pool.tick_current < upper.tick {
            upper_out
        } else {
            global.wrapping_sub(upper_out)
        };
        global.wrapping_sub(below).wrapping_sub(above)
    };
    (
        side(
            pool.fee_growth_global_0_x64,
            lower.fee_growth_outside_0_x64,
            upper.fee_growth_outside_0_x64,
        ),
        side(
            pool.fee_growth_global_1_x64,
            lower.fee_growth_outside_1_x64,
            upper.fee_growth_outside_1_x64,
        ),
    )
}

/// A position's uncollected fees, in atoms of token 0 and token 1: those
/// counted on it, and those earned since.
pub fn fees_owed(
    position: &PersonalPosition,
    pool: &PoolState,
    lower: &Tick,
    upper: &Tick,
) -> (u64, u64) {
    let (inside_0, inside_1) = fee_growth_inside(pool, lower, upper);
    let earned = |inside: u128, last: u128| {
        let growth = inside.wrapping_sub(last);
        mul_shift_64(growth, position.liquidity)
    };
    (
        position
            .fees_owed_0
            .saturating_add(earned(inside_0, position.fee_growth_inside_0_last_x64)),
        position
            .fees_owed_1
            .saturating_add(earned(inside_1, position.fee_growth_inside_1_last_x64)),
    )
}

/// `a × b >> 64`, saturating at `u64::MAX`.
fn mul_shift_64(a: u128, b: u128) -> u64 {
    let (a_hi, a_lo) = (a >> 64, a & u128::from(u64::MAX));
    let (b_hi, b_lo) = (b >> 64, b & u128::from(u64::MAX));
    // (a_hi·2^64 + a_lo)(b_hi·2^64 + b_lo) >> 64.
    let lo = (a_lo * b_lo) >> 64;
    let mid = a_hi
        .checked_mul(b_lo)
        .and_then(|x| x.checked_add(a_lo.checked_mul(b_hi)?))
        .and_then(|x| x.checked_add(lo));
    let high = a_hi.checked_mul(b_hi);
    match (mid, high) {
        (Some(mid), Some(0)) => u64::try_from(mid).unwrap_or(u64::MAX),
        _ => u64::MAX,
    }
}

/// What a position holds at the pool's price, in atoms of token 0 and
/// token 1, its fees left out.
pub fn position_amounts(position: &PersonalPosition, pool: &PoolState) -> (f64, f64) {
    amounts_for_liquidity(
        position.liquidity,
        pool.sqrt_price_x64 as f64 / Q64,
        sqrt_at_tick(position.tick_lower),
        sqrt_at_tick(position.tick_upper),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prices_ticks_and_sqrt_prices_agree() {
        let sqrt = sqrt_x64_from_price(140.27, 6, 6);
        assert!((price_from_sqrt_x64(sqrt, 6, 6) - 140.27).abs() < 1e-9);
        // Tick 49438 is the live SPCX/USDC pool's at 140.27.
        assert_eq!(tick_at_price(140.27, 6, 6, 1), 49438);
        assert_eq!(tick_at_price(140.27, 6, 6, 60), 49380);
        assert_eq!(tick_at_price(1e-30, 6, 6, 60), -443_580);
        assert_eq!(sqrt_x64_from_price(0.0, 6, 6), MIN_SQRT_PRICE_X64 + 1);
        assert!((sqrt_at_tick(49438).powi(2) - 140.27).abs() < 0.02);
    }

    #[test]
    fn a_position_holds_both_tokens_only_inside_its_range() {
        let (lower, upper) = (sqrt_at_tick(-600), sqrt_at_tick(600));
        let l = 1_000_000_000u128;
        let (below_0, below_1) = amounts_for_liquidity(l, sqrt_at_tick(-1200), lower, upper);
        assert!(below_0 > 0.0 && below_1 == 0.0);
        let (above_0, above_1) = amounts_for_liquidity(l, sqrt_at_tick(1200), lower, upper);
        assert!(above_0 == 0.0 && above_1 > 0.0);
        let (in_0, in_1) = amounts_for_liquidity(l, 1.0, lower, upper);
        assert!(in_0 > 0.0 && in_1 > 0.0);
        // The liquidity back from those amounts.
        let (from_0, from_1) = liquidity_for_amounts(in_0, in_1, 1.0, lower, upper);
        assert!((from_0.unwrap() - l as f64).abs() < 1.0);
        assert!((from_1.unwrap() - l as f64).abs() < 1.0);
        assert_eq!(liquidity_for_amounts(1.0, 1.0, 0.5, lower, upper).1, None);
    }

    #[test]
    fn fees_are_the_growth_inside_times_the_liquidity() {
        assert_eq!(mul_shift_64(1 << 64, 5), 5);
        assert_eq!(mul_shift_64(3 << 63, 4), 6);
        assert_eq!(mul_shift_64(u128::MAX, u128::MAX), u64::MAX);
    }
}
