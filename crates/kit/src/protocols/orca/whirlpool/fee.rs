//! The fee a swap step pays: a pool's static fee, or with an oracle its
//! adaptive fee, which rises with how many tick groups the price crossed
//! recently. Ported from `orca-so/whirlpools@cc683ec92acb5e7606caa3d8e514d9383e9c7c11`,
//! `programs/whirlpool/src/manager/fee_rate_manager.rs` (`FeeRateManager`)
//! and `state/oracle.rs` (`AdaptiveFeeVariables`).
//!
//! An adaptive fee changes within a swap, one tick group at a time, so the
//! swap loop bounds each step at the next tick group's edge while the fee
//! can still change, and skips ahead where it can't.

use super::{
    error::ProgramFailure,
    math::{
        MAX_TICK_INDEX, MIN_TICK_INDEX, sqrt_price_from_tick_index, tick_index_from_sqrt_price,
    },
    state::Oracle,
};

/// The total fee rate's cap, in hundredths of a basis point (10%).
const FEE_RATE_HARD_LIMIT: u32 = 100_000;
/// One tick group crossed adds this much volatility.
const VOLATILITY_ACCUMULATOR_SCALE_FACTOR: u32 = 10_000;
const REDUCTION_FACTOR_DENOMINATOR: u64 = 10_000;
const ADAPTIVE_FEE_CONTROL_FACTOR_DENOMINATOR: u128 = 100_000;
/// References older than this are reset (`oracle.rs`, `MAX_REFERENCE_AGE`).
const MAX_REFERENCE_AGE_S: u64 = 3_600;

pub(super) enum FeeRate {
    Static { fee_rate: u16 },
    Adaptive(Box<Adaptive>),
}

pub(super) struct Adaptive {
    a_to_b: bool,
    tick_group_index: i32,
    static_fee_rate: u16,
    oracle: Oracle,
    /// The tick group below which the volatility is always at its maximum,
    /// and that group's lower edge's price.
    core_range_lower_bound: Option<(i32, u128)>,
    /// The tick group above which it is, and that group's upper edge.
    core_range_upper_bound: Option<(i32, u128)>,
}

impl FeeRate {
    pub(super) fn new_static(fee_rate: u16) -> Self {
        Self::Static { fee_rate }
    }

    /// `FeeRateManager::new` for a pool with an oracle.
    pub(super) fn new_adaptive(
        a_to_b: bool,
        current_tick_index: i32,
        timestamp_s: u64,
        static_fee_rate: u16,
        oracle: &Oracle,
    ) -> Result<Self, ProgramFailure> {
        let tick_group_size = i32::from(oracle.tick_group_size);
        assert!(
            tick_group_size > 0,
            "decoding refuses a zero tick group size"
        );
        let tick_group_index = current_tick_index.div_euclid(tick_group_size);
        let mut oracle = *oracle;
        update_reference(&mut oracle, tick_group_index, timestamp_s)?;
        assert!(oracle.volatility_reference <= oracle.max_volatility_accumulator);

        let max_delta = (oracle.max_volatility_accumulator - oracle.volatility_reference)
            .div_ceil(VOLATILITY_ACCUMULATOR_SCALE_FACTOR);
        // In i64: the program's i32 arithmetic can't overflow for the
        // constants it accepts, and the comparisons agree when it doesn't.
        let lower_index = i64::from(oracle.tick_group_index_reference) - i64::from(max_delta);
        let upper_index = i64::from(oracle.tick_group_index_reference) + i64::from(max_delta);
        let lower_tick = lower_index * i64::from(tick_group_size);
        let upper_tick = upper_index * i64::from(tick_group_size) + i64::from(tick_group_size);
        let core_range_lower_bound = (lower_tick > i64::from(MIN_TICK_INDEX)).then(|| {
            (
                lower_index as i32,
                sqrt_price_from_tick_index(lower_tick as i32),
            )
        });
        let core_range_upper_bound = (upper_tick < i64::from(MAX_TICK_INDEX)).then(|| {
            (
                upper_index as i32,
                sqrt_price_from_tick_index(upper_tick as i32),
            )
        });
        Ok(Self::Adaptive(Box::new(Adaptive {
            a_to_b,
            tick_group_index,
            static_fee_rate,
            oracle,
            core_range_lower_bound,
            core_range_upper_bound,
        })))
    }

    pub(super) fn update_volatility_accumulator(&mut self) {
        if let Self::Adaptive(adaptive) = self {
            update_volatility_accumulator(&mut adaptive.oracle, adaptive.tick_group_index);
        }
    }

    /// The fee rate for the next step, in hundredths of a basis point.
    pub(super) fn total_fee_rate(&self) -> u32 {
        match self {
            Self::Static { fee_rate } => u32::from(*fee_rate),
            Self::Adaptive(adaptive) => {
                let total =
                    u32::from(adaptive.static_fee_rate) + adaptive_fee_rate(&adaptive.oracle);
                total.min(FEE_RATE_HARD_LIMIT)
            }
        }
    }

    /// The step's target, bounded at the current tick group's edge, and
    /// whether the step skips the tick-group-at-a-time walk
    /// (`get_bounded_sqrt_price_target`).
    pub(super) fn bounded_sqrt_price_target(
        &self,
        sqrt_price_target_x64: u128,
        liquidity: u128,
    ) -> (u128, bool) {
        let Self::Adaptive(adaptive) = self else {
            return (sqrt_price_target_x64, false);
        };
        let a_to_b = adaptive.a_to_b;
        if adaptive.oracle.adaptive_fee_control_factor == 0 || liquidity == 0 {
            return (sqrt_price_target_x64, true);
        }
        // Outside the core range the volatility stays at its maximum, so
        // the fee can't change: skip to the range (or the target).
        if let Some((lower_index, lower_sqrt_price)) = adaptive.core_range_lower_bound
            && adaptive.tick_group_index < lower_index
        {
            if a_to_b {
                return (sqrt_price_target_x64, true);
            }
            return (sqrt_price_target_x64.min(lower_sqrt_price), true);
        }
        if let Some((upper_index, upper_sqrt_price)) = adaptive.core_range_upper_bound
            && adaptive.tick_group_index > upper_index
        {
            if a_to_b {
                return (sqrt_price_target_x64.max(upper_sqrt_price), true);
            }
            return (sqrt_price_target_x64, true);
        }
        let tick_group_size = i32::from(adaptive.oracle.tick_group_size);
        let boundary_tick = if a_to_b {
            adaptive.tick_group_index * tick_group_size
        } else {
            adaptive.tick_group_index * tick_group_size + tick_group_size
        };
        let boundary_sqrt_price =
            sqrt_price_from_tick_index(boundary_tick.clamp(MIN_TICK_INDEX, MAX_TICK_INDEX));
        if a_to_b {
            (sqrt_price_target_x64.max(boundary_sqrt_price), false)
        } else {
            (sqrt_price_target_x64.min(boundary_sqrt_price), false)
        }
    }

    /// Moves to the next tick group after a step that didn't skip.
    pub(super) fn advance_tick_group(&mut self) {
        if let Self::Adaptive(adaptive) = self {
            adaptive.tick_group_index += if adaptive.a_to_b { -1 } else { 1 };
        }
    }

    /// Catches up with the price after a step that skipped
    /// (`advance_tick_group_after_skip`).
    pub(super) fn advance_tick_group_after_skip(
        &mut self,
        sqrt_price_x64: u128,
        next_tick_sqrt_price_x64: u128,
        next_tick_index: i32,
    ) {
        let Self::Adaptive(adaptive) = self else {
            unreachable!("a static fee never skips");
        };
        let tick_group_size = i32::from(adaptive.oracle.tick_group_size);
        let (tick_index, is_on_tick_group_boundary) = if sqrt_price_x64 == next_tick_sqrt_price_x64
        {
            (next_tick_index, next_tick_index % tick_group_size == 0)
        } else {
            let tick_index = tick_index_from_sqrt_price(sqrt_price_x64);
            let is_on_boundary = tick_index % tick_group_size == 0
                && sqrt_price_x64 == sqrt_price_from_tick_index(tick_index);
            (tick_index, is_on_boundary)
        };
        let last_traversed_tick_group_index = if is_on_tick_group_boundary && !adaptive.a_to_b {
            tick_index / tick_group_size - 1
        } else {
            tick_index.div_euclid(tick_group_size)
        };
        if (adaptive.a_to_b && last_traversed_tick_group_index < adaptive.tick_group_index)
            || (!adaptive.a_to_b && last_traversed_tick_group_index > adaptive.tick_group_index)
        {
            adaptive.tick_group_index = last_traversed_tick_group_index;
            update_volatility_accumulator(&mut adaptive.oracle, adaptive.tick_group_index);
        }
        adaptive.tick_group_index += if adaptive.a_to_b { -1 } else { 1 };
    }
}

/// `AdaptiveFeeVariables::update_reference`.
fn update_reference(
    oracle: &mut Oracle,
    tick_group_index: i32,
    timestamp_s: u64,
) -> Result<(), ProgramFailure> {
    let max_timestamp_s = oracle
        .last_reference_update_timestamp_s
        .max(oracle.last_major_swap_timestamp_s);
    if timestamp_s < max_timestamp_s {
        return Err(ProgramFailure::InvalidTimestamp);
    }
    let reference_age_s = timestamp_s - oracle.last_reference_update_timestamp_s;
    if reference_age_s > MAX_REFERENCE_AGE_S {
        oracle.tick_group_index_reference = tick_group_index;
        oracle.volatility_reference = 0;
        oracle.last_reference_update_timestamp_s = timestamp_s;
        return Ok(());
    }
    let elapsed_s = timestamp_s - max_timestamp_s;
    if elapsed_s < u64::from(oracle.filter_period_s) {
        // A burst of swaps: the reference stays.
    } else if elapsed_s < u64::from(oracle.decay_period_s) {
        oracle.tick_group_index_reference = tick_group_index;
        let decayed = u64::from(oracle.volatility_accumulator) * u64::from(oracle.reduction_factor)
            / REDUCTION_FACTOR_DENOMINATOR;
        oracle.volatility_reference =
            u32::try_from(decayed).expect("a u32 times a factor below one fits a u32");
        oracle.last_reference_update_timestamp_s = timestamp_s;
    } else {
        oracle.tick_group_index_reference = tick_group_index;
        oracle.volatility_reference = 0;
        oracle.last_reference_update_timestamp_s = timestamp_s;
    }
    Ok(())
}

/// `AdaptiveFeeVariables::update_volatility_accumulator`.
fn update_volatility_accumulator(oracle: &mut Oracle, tick_group_index: i32) {
    let index_delta = (oracle.tick_group_index_reference - tick_group_index).unsigned_abs();
    let volatility = u64::from(oracle.volatility_reference)
        + u64::from(index_delta) * u64::from(VOLATILITY_ACCUMULATOR_SCALE_FACTOR);
    oracle.volatility_accumulator =
        volatility.min(u64::from(oracle.max_volatility_accumulator)) as u32;
}

/// `FeeRateManager::compute_adaptive_fee_rate`: quadratic in the tick
/// groups crossed, rounded up.
fn adaptive_fee_rate(oracle: &Oracle) -> u32 {
    let crossed = u64::from(oracle.volatility_accumulator) * u64::from(oracle.tick_group_size);
    let squared = u128::from(crossed) * u128::from(crossed);
    let denominator = ADAPTIVE_FEE_CONTROL_FACTOR_DENOMINATOR
        * u128::from(VOLATILITY_ACCUMULATOR_SCALE_FACTOR)
        * u128::from(VOLATILITY_ACCUMULATOR_SCALE_FACTOR);
    let fee_rate = (u128::from(oracle.adaptive_fee_control_factor) * squared).div_ceil(denominator);
    fee_rate.min(u128::from(FEE_RATE_HARD_LIMIT)) as u32
}

#[cfg(test)]
mod tests {
    use solana_pubkey::Pubkey;

    use super::*;

    fn oracle() -> Oracle {
        Oracle {
            whirlpool: Pubkey::default(),
            trade_enable_timestamp_s: 0,
            filter_period_s: 30,
            decay_period_s: 600,
            reduction_factor: 5_000,
            adaptive_fee_control_factor: 4_000,
            max_volatility_accumulator: 350_000,
            tick_group_size: 64,
            major_swap_threshold_ticks: 64,
            last_reference_update_timestamp_s: 1_000,
            last_major_swap_timestamp_s: 1_000,
            volatility_reference: 0,
            tick_group_index_reference: 0,
            volatility_accumulator: 0,
        }
    }

    #[test]
    fn the_adaptive_fee_grows_with_the_square_of_the_groups_crossed() {
        let mut oracle = oracle();
        assert_eq!(adaptive_fee_rate(&oracle), 0);
        update_volatility_accumulator(&mut oracle, 1);
        assert_eq!(oracle.volatility_accumulator, 10_000);
        // 4,000 * (10,000 * 64)^2 / (100,000 * 10^8), rounded up.
        assert_eq!(adaptive_fee_rate(&oracle), 164);
        update_volatility_accumulator(&mut oracle, -2);
        assert_eq!(adaptive_fee_rate(&oracle), 656);
        update_volatility_accumulator(&mut oracle, 1_000);
        assert_eq!(oracle.volatility_accumulator, 350_000);
        assert_eq!(adaptive_fee_rate(&oracle), FEE_RATE_HARD_LIMIT);
    }

    #[test]
    fn the_reference_decays_between_bursts_and_resets_when_old() {
        let mut burst = oracle();
        burst.volatility_accumulator = 40_000;
        update_reference(&mut burst, 5, 1_010).unwrap();
        assert_eq!(
            (burst.tick_group_index_reference, burst.volatility_reference),
            (0, 0)
        );

        let mut decayed = oracle();
        decayed.volatility_accumulator = 40_000;
        update_reference(&mut decayed, 5, 1_100).unwrap();
        assert_eq!(
            (
                decayed.tick_group_index_reference,
                decayed.volatility_reference
            ),
            (5, 20_000)
        );

        let mut old = oracle();
        old.volatility_accumulator = 40_000;
        update_reference(&mut old, 5, 1_000 + MAX_REFERENCE_AGE_S + 1).unwrap();
        assert_eq!(
            (old.tick_group_index_reference, old.volatility_reference),
            (5, 0)
        );

        assert_eq!(
            update_reference(&mut oracle(), 0, 999),
            Err(ProgramFailure::InvalidTimestamp)
        );
    }
}
