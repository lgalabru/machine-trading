//! A Whirlpool swap, quoted exactly as the program executes it: the swap
//! loop of `orca-so/whirlpools@cc683ec92acb5e7606caa3d8e514d9383e9c7c11`,
//! `programs/whirlpool/src/manager/swap_manager.rs` (`swap`), over the tick
//! sequence of `util/sparse_swap.rs` and `util/swap_tick_sequence.rs`,
//! with `instructions/v2/swap.rs`'s handling of the amounts.
//!
//! The program walks at most three tick arrays: the one holding the
//! current tick and the next two in the swap's direction. A swap that
//! would leave the third fails, so a quote that would fails too.

use solana_pubkey::Pubkey;

use super::{
    STEPS_MAX,
    error::{ProgramFailure, WhirlpoolError},
    fee::FeeRate,
    ix::tick_array_address,
    math::{
        MAX_SQRT_PRICE_X64, MAX_TICK_INDEX, MIN_SQRT_PRICE_X64, MIN_TICK_INDEX,
        PROTOCOL_FEE_RATE_MUL_VALUE, compute_swap, sqrt_price_from_tick_index,
        tick_index_from_sqrt_price,
    },
    state::{Oracle, TICK_ARRAY_SIZE, TickArray, Whirlpool},
};

/// Tick arrays one swap walks (`sparse_swap.rs`, `get_start_tick_indexes`).
pub const SWAP_TICK_ARRAYS_MAX: usize = 3;

/// Which way a swap trades.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapDirection {
    /// Token A in, token B out: the price falls.
    AToB,
    /// Token B in, token A out: the price rises.
    BToA,
}

impl SwapDirection {
    pub fn is_a_to_b(self) -> bool {
        self == Self::AToB
    }
}

/// Which side of a swap its amount fixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapMode {
    /// The amount is what goes in, fee included.
    ExactIn,
    /// The amount is what comes out.
    ExactOut,
}

/// A swap to quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapRequest {
    pub direction: SwapDirection,
    pub mode: SwapMode,
    /// Atoms in (exact in) or out (exact out).
    pub amount: u64,
    /// Where the price must stop, Q64.64; 0 for no limit, in which case an
    /// exact-out swap must fill entirely.
    pub sqrt_price_limit_x64: u128,
    /// The block's time, in Unix seconds: an adaptive fee depends on it.
    pub timestamp_s: u64,
}

/// What a swap does, to the atom.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapQuote {
    /// Atoms the trader pays into the input vault, fee included.
    pub amount_in: u64,
    /// Atoms the trader receives from the output vault.
    pub amount_out: u64,
    /// Atoms of the input charged as fee (liquidity providers' and
    /// protocol's).
    pub fee_amount: u64,
    /// The protocol's part of `fee_amount`.
    pub protocol_fee_amount: u64,
    /// The pool's price after the swap, Q64.64.
    pub sqrt_price_x64_end: u128,
    /// The pool's tick after the swap.
    pub tick_end: i32,
    /// The pool's liquidity after the swap.
    pub liquidity_end: u128,
    /// Initialized ticks the swap crossed.
    pub ticks_crossed: u32,
    /// Tick arrays the swap reached, 1 to 3.
    pub tick_arrays_entered: u8,
    /// The `tick_array_0..2` accounts `swap_v2` takes, in order.
    pub tick_arrays: [Pubkey; SWAP_TICK_ARRAYS_MAX],
}

/// The start ticks of the arrays a swap in `direction` walks, in order
/// (`sparse_swap.rs`, `get_start_tick_indexes`): fetch these.
pub fn tick_array_start_indexes(pool: &Whirlpool, direction: SwapDirection) -> Vec<i32> {
    let tick_spacing = i32::from(pool.tick_spacing);
    let ticks_in_array = pool.ticks_in_array();
    let start_base = pool.tick_current_index.div_euclid(ticks_in_array) * ticks_in_array;
    let offsets: [i32; SWAP_TICK_ARRAYS_MAX] = match direction {
        SwapDirection::AToB => [0, -1, -2],
        SwapDirection::BToA => {
            // A price at the array's last tick searches from the next one.
            if pool.tick_current_index + tick_spacing >= start_base + ticks_in_array {
                [1, 2, 3]
            } else {
                [0, 1, 2]
            }
        }
    };
    offsets
        .iter()
        .map(|offset| start_base + offset * ticks_in_array)
        .filter(|&start| is_valid_start_tick(start, pool.tick_spacing))
        .collect()
}

/// The addresses of [`tick_array_start_indexes`].
pub fn tick_array_addresses(
    pool_address: &Pubkey,
    pool: &Whirlpool,
    direction: SwapDirection,
) -> Vec<(i32, Pubkey)> {
    tick_array_start_indexes(pool, direction)
        .into_iter()
        .map(|start| (start, tick_array_address(pool_address, start)))
        .collect()
}

/// Quotes a swap through `pool` (at `pool_address`).
///
/// `tick_arrays` must hold every array [`tick_array_start_indexes`] lists,
/// decoded or, where no account exists, [`TickArray::uninitialized`];
/// arrays of other pools are ignored. `oracle` is the pool's oracle
/// account, which an adaptive-fee pool needs. Token-2022 transfer fees are
/// not modelled: the amounts are what enters and leaves the vaults.
pub fn quote(
    pool_address: &Pubkey,
    pool: &Whirlpool,
    tick_arrays: &[TickArray],
    oracle: Option<&Oracle>,
    request: &SwapRequest,
) -> Result<SwapQuote, WhirlpoolError> {
    let a_to_b = request.direction.is_a_to_b();
    let start_indexes = tick_array_start_indexes(pool, request.direction);
    assert!(
        !start_indexes.is_empty(),
        "the current tick's array is always valid"
    );
    let mut sequence = Vec::with_capacity(SWAP_TICK_ARRAYS_MAX);
    for &start in &start_indexes {
        let array = tick_arrays
            .iter()
            .find(|array| array.whirlpool == *pool_address && array.start_tick_index == start)
            .ok_or(WhirlpoolError::MissingTickArray {
                start_tick_index: start,
            })?;
        sequence.push(array);
    }

    let fee_rate = match oracle {
        Some(oracle) => {
            if oracle.whirlpool != *pool_address {
                return Err(crate::protocols::DecodeError::new(
                    "whirlpool oracle",
                    format!("belongs to {}, not {pool_address}", oracle.whirlpool),
                )
                .into());
            }
            if oracle.trade_enable_timestamp_s > request.timestamp_s {
                return Err(ProgramFailure::TradeIsNotEnabled.into());
            }
            FeeRate::new_adaptive(
                a_to_b,
                pool.tick_current_index,
                request.timestamp_s,
                pool.fee_rate,
                oracle,
            )?
        }
        None if pool.is_adaptive_fee_tier() => return Err(WhirlpoolError::MissingOracle),
        None => FeeRate::new_static(pool.fee_rate),
    };

    let swap = swap(pool, &sequence, fee_rate, request)?;
    let mut tick_arrays_out = [Pubkey::default(); SWAP_TICK_ARRAYS_MAX];
    for (slot, account) in tick_arrays_out.iter_mut().enumerate() {
        // Fewer than three arrays exist at the ends of the tick range: the
        // last one stands in for the missing (the program dedups them).
        let start = start_indexes[slot.min(start_indexes.len() - 1)];
        *account = tick_array_address(pool_address, start);
    }
    let (amount_in, amount_out) = (swap.amount_input, swap.amount_output);
    assert!(amount_in >= swap.fee_amount);
    assert!(swap.protocol_fee_amount <= swap.fee_amount);
    Ok(SwapQuote {
        amount_in,
        amount_out,
        fee_amount: swap.fee_amount,
        protocol_fee_amount: swap.protocol_fee_amount,
        sqrt_price_x64_end: swap.sqrt_price_x64,
        tick_end: swap.tick_index,
        liquidity_end: swap.liquidity,
        ticks_crossed: swap.ticks_crossed,
        tick_arrays_entered: swap.tick_arrays_entered,
        tick_arrays: tick_arrays_out,
    })
}

/// What the swap loop ends with.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SwapOutcome {
    amount_input: u64,
    amount_output: u64,
    fee_amount: u64,
    protocol_fee_amount: u64,
    sqrt_price_x64: u128,
    tick_index: i32,
    liquidity: u128,
    ticks_crossed: u32,
    tick_arrays_entered: u8,
}

/// The program's `swap`, over an explicit sequence of tick arrays.
fn swap(
    pool: &Whirlpool,
    sequence: &[&TickArray],
    mut fee_rate: FeeRate,
    request: &SwapRequest,
) -> Result<SwapOutcome, WhirlpoolError> {
    assert!(!sequence.is_empty() && sequence.len() <= SWAP_TICK_ARRAYS_MAX);
    let a_to_b = request.direction.is_a_to_b();
    let is_input = request.mode == SwapMode::ExactIn;
    let sqrt_price_limit_x64 = match (request.sqrt_price_limit_x64, a_to_b) {
        (0, true) => MIN_SQRT_PRICE_X64,
        (0, false) => MAX_SQRT_PRICE_X64,
        (limit, _) => limit,
    };
    if !(MIN_SQRT_PRICE_X64..=MAX_SQRT_PRICE_X64).contains(&sqrt_price_limit_x64) {
        return Err(ProgramFailure::SqrtPriceOutOfBounds.into());
    }
    if (a_to_b && sqrt_price_limit_x64 >= pool.sqrt_price_x64)
        || (!a_to_b && sqrt_price_limit_x64 <= pool.sqrt_price_x64)
    {
        return Err(ProgramFailure::InvalidSqrtPriceLimitDirection.into());
    }
    if request.amount == 0 {
        return Err(ProgramFailure::ZeroTradableAmount.into());
    }

    let tick_spacing = pool.tick_spacing;
    let mut amount_remaining = request.amount;
    let mut amount_calculated: u64 = 0;
    let mut sqrt_price_x64 = pool.sqrt_price_x64;
    let mut tick_index = pool.tick_current_index;
    let mut liquidity = pool.liquidity;
    let mut fee_sum: u64 = 0;
    let mut protocol_fee_sum: u64 = 0;
    let mut array_index: usize = 0;
    let mut ticks_crossed: u32 = 0;
    let mut array_index_max: usize = 0;
    let mut steps: usize = 0;

    while amount_remaining > 0 && sqrt_price_limit_x64 != sqrt_price_x64 {
        let (next_array_index, next_tick_index) =
            next_initialized_tick_index(sequence, tick_index, tick_spacing, a_to_b, array_index)?;
        array_index_max = array_index_max.max(next_array_index);
        let next_tick_sqrt_price_x64 = sqrt_price_from_tick_index(next_tick_index);
        let sqrt_price_target_x64 = if a_to_b {
            sqrt_price_limit_x64.max(next_tick_sqrt_price_x64)
        } else {
            sqrt_price_limit_x64.min(next_tick_sqrt_price_x64)
        };

        loop {
            steps += 1;
            if steps > STEPS_MAX {
                return Err(WhirlpoolError::TooManySteps(STEPS_MAX));
            }
            fee_rate.update_volatility_accumulator();
            let total_fee_rate = fee_rate.total_fee_rate();
            let (bounded_target_x64, is_skipped) =
                fee_rate.bounded_sqrt_price_target(sqrt_price_target_x64, liquidity);
            let step = compute_swap(
                amount_remaining,
                total_fee_rate,
                liquidity,
                sqrt_price_x64,
                bounded_target_x64,
                is_input,
                a_to_b,
            )?;
            if is_input {
                amount_remaining = amount_remaining
                    .checked_sub(step.amount_in)
                    .and_then(|left| left.checked_sub(step.fee_amount))
                    .ok_or(ProgramFailure::AmountRemainingOverflow)?;
                amount_calculated = amount_calculated
                    .checked_add(step.amount_out)
                    .ok_or(ProgramFailure::AmountCalcOverflow)?;
            } else {
                amount_remaining = amount_remaining
                    .checked_sub(step.amount_out)
                    .ok_or(ProgramFailure::AmountRemainingOverflow)?;
                amount_calculated = amount_calculated
                    .checked_add(step.amount_in)
                    .and_then(|sum| sum.checked_add(step.fee_amount))
                    .ok_or(ProgramFailure::AmountCalcOverflow)?;
            }
            fee_sum = fee_sum
                .checked_add(step.fee_amount)
                .ok_or(ProgramFailure::AmountCalcOverflow)?;
            // `swap_manager.rs`, `calculate_fees`: the protocol's share of
            // each step, rounded down (it wraps on chain; a u64 of fees
            // can't reach that).
            if pool.protocol_fee_rate > 0 {
                let protocol_fee = u128::from(step.fee_amount) * u128::from(pool.protocol_fee_rate)
                    / PROTOCOL_FEE_RATE_MUL_VALUE;
                protocol_fee_sum += protocol_fee as u64;
            }

            if step.next_sqrt_price_x64 == next_tick_sqrt_price_x64 {
                if let Some(liquidity_net) =
                    initialized_tick(sequence, next_array_index, next_tick_index, tick_spacing)
                {
                    let signed_net = if a_to_b {
                        liquidity_net
                            .checked_neg()
                            .ok_or(ProgramFailure::LiquidityOverflow)?
                    } else {
                        liquidity_net
                    };
                    liquidity = add_liquidity_delta(liquidity, signed_net)?;
                    ticks_crossed += 1;
                }
                let tick_offset =
                    tick_offset(sequence, next_array_index, next_tick_index, tick_spacing)?;
                array_index = if (a_to_b && tick_offset == 0)
                    || (!a_to_b && tick_offset == TICK_ARRAY_SIZE as i32 - 1)
                {
                    next_array_index + 1
                } else {
                    next_array_index
                };
                // A search to the left includes its start: step past the
                // tick just crossed.
                tick_index = if a_to_b {
                    next_tick_index - 1
                } else {
                    next_tick_index
                };
            } else if step.next_sqrt_price_x64 != sqrt_price_x64 {
                tick_index = tick_index_from_sqrt_price(step.next_sqrt_price_x64);
            }
            sqrt_price_x64 = step.next_sqrt_price_x64;

            if is_skipped {
                fee_rate.advance_tick_group_after_skip(
                    sqrt_price_x64,
                    next_tick_sqrt_price_x64,
                    next_tick_index,
                );
            } else {
                fee_rate.advance_tick_group();
            }
            if amount_remaining == 0 || sqrt_price_x64 == sqrt_price_target_x64 {
                break;
            }
        }
    }

    if amount_remaining > 0 && !is_input && request.sqrt_price_limit_x64 == 0 {
        return Err(ProgramFailure::PartialFillError.into());
    }
    let amount_specified_used = request.amount - amount_remaining;
    let (amount_input, amount_output) = if is_input {
        (amount_specified_used, amount_calculated)
    } else {
        (amount_calculated, amount_specified_used)
    };
    Ok(SwapOutcome {
        amount_input,
        amount_output,
        fee_amount: fee_sum,
        protocol_fee_amount: protocol_fee_sum,
        sqrt_price_x64,
        tick_index,
        liquidity,
        ticks_crossed,
        tick_arrays_entered: (array_index_max.min(sequence.len() - 1) + 1) as u8,
    })
}

/// `liquidity_math.rs`, `add_liquidity_delta`.
fn add_liquidity_delta(liquidity: u128, delta: i128) -> Result<u128, ProgramFailure> {
    if delta >= 0 {
        liquidity
            .checked_add(delta.unsigned_abs())
            .ok_or(ProgramFailure::LiquidityOverflow)
    } else {
        liquidity
            .checked_sub(delta.unsigned_abs())
            .ok_or(ProgramFailure::LiquidityUnderflow)
    }
}

/// `tick.rs`, `check_is_valid_start_tick`.
fn is_valid_start_tick(tick_index: i32, tick_spacing: u16) -> bool {
    let ticks_in_array = TICK_ARRAY_SIZE as i32 * i32::from(tick_spacing);
    if !(MIN_TICK_INDEX..=MAX_TICK_INDEX).contains(&tick_index) {
        if tick_index > MIN_TICK_INDEX {
            return false;
        }
        let min_array_start_index =
            MIN_TICK_INDEX - (MIN_TICK_INDEX % ticks_in_array + ticks_in_array);
        return tick_index == min_array_start_index;
    }
    tick_index % ticks_in_array == 0
}

/// `tick_array.rs`, `get_offset`: the floor of the distance from the
/// array's start, in tick spacings.
fn offset_in_array(array: &TickArray, tick_index: i32, tick_spacing: u16) -> i32 {
    (tick_index - array.start_tick_index).div_euclid(i32::from(tick_spacing))
}

/// `swap_tick_sequence.rs`, `get_next_initialized_tick_index`: the next
/// initialized tick from `tick_index`, or the edge of the sequence.
fn next_initialized_tick_index(
    sequence: &[&TickArray],
    tick_index: i32,
    tick_spacing: u16,
    a_to_b: bool,
    start_array_index: usize,
) -> Result<(usize, i32), ProgramFailure> {
    let ticks_in_array = TICK_ARRAY_SIZE as i32 * i32::from(tick_spacing);
    let mut search_index = tick_index;
    // Each pass moves to the next array: at most the sequence's length.
    for array_index in start_array_index..sequence.len() {
        let array = sequence[array_index];
        if let Some(next) = next_init_tick_in_array(array, search_index, tick_spacing, a_to_b)? {
            return Ok((array_index, next));
        }
        if a_to_b && array.start_tick_index <= MIN_TICK_INDEX {
            return Ok((array_index, MIN_TICK_INDEX));
        }
        if !a_to_b && array.start_tick_index + ticks_in_array > MAX_TICK_INDEX {
            return Ok((array_index, MAX_TICK_INDEX));
        }
        if array_index + 1 == sequence.len() {
            let edge = if a_to_b {
                array.start_tick_index
            } else {
                array.start_tick_index + ticks_in_array - 1
            };
            return Ok((array_index, edge));
        }
        search_index = if a_to_b {
            array.start_tick_index - 1
        } else {
            array.start_tick_index + ticks_in_array - 1
        };
    }
    Err(ProgramFailure::TickArraySequenceInvalidIndex)
}

/// `fixed_tick_array.rs` and `dynamic_tick_array.rs`,
/// `get_next_init_tick_index`: to the left the start tick is included, to
/// the right it isn't.
fn next_init_tick_in_array(
    array: &TickArray,
    tick_index: i32,
    tick_spacing: u16,
    a_to_b: bool,
) -> Result<Option<i32>, ProgramFailure> {
    let spacing = i32::from(tick_spacing);
    let shift = if a_to_b { 0 } else { spacing };
    let lower = array.start_tick_index - shift;
    let upper = array.start_tick_index + TICK_ARRAY_SIZE as i32 * spacing - shift;
    if tick_index < lower || tick_index >= upper {
        return Err(ProgramFailure::InvalidTickArraySequence);
    }
    let mut offset = offset_in_array(array, tick_index, tick_spacing);
    if !a_to_b {
        offset += 1;
    }
    while (0..TICK_ARRAY_SIZE as i32).contains(&offset) {
        if array.liquidity_net[offset as usize].is_some() {
            return Ok(Some(offset * spacing + array.start_tick_index));
        }
        offset += if a_to_b { -1 } else { 1 };
    }
    Ok(None)
}

/// The net liquidity of the tick at `tick_index` if it is initialized
/// (`swap_manager.rs` reads a missing tick as uninitialized).
fn initialized_tick(
    sequence: &[&TickArray],
    array_index: usize,
    tick_index: i32,
    tick_spacing: u16,
) -> Option<i128> {
    let array = sequence.get(array_index)?;
    let spacing = i32::from(tick_spacing);
    let in_bounds = tick_index >= array.start_tick_index
        && tick_index < array.start_tick_index + TICK_ARRAY_SIZE as i32 * spacing;
    let is_usable =
        (MIN_TICK_INDEX..=MAX_TICK_INDEX).contains(&tick_index) && tick_index % spacing == 0;
    if !in_bounds || !is_usable {
        return None;
    }
    let offset = offset_in_array(array, tick_index, tick_spacing);
    array.liquidity_net[usize::try_from(offset).ok()?]
}

fn tick_offset(
    sequence: &[&TickArray],
    array_index: usize,
    tick_index: i32,
    tick_spacing: u16,
) -> Result<i32, ProgramFailure> {
    let array = sequence
        .get(array_index)
        .ok_or(ProgramFailure::TickArrayIndexOutofBounds)?;
    Ok(offset_in_array(array, tick_index, tick_spacing))
}

#[cfg(test)]
mod tests {
    use super::*;

    const TS_8: u16 = 8;
    const TS_128: u16 = 128;

    fn pool(tick_spacing: u16, liquidity: u128, tick: i32) -> Whirlpool {
        Whirlpool {
            whirlpools_config: Pubkey::default(),
            tick_spacing,
            fee_tier_index: tick_spacing,
            fee_rate: 0,
            protocol_fee_rate: 0,
            liquidity,
            sqrt_price_x64: sqrt_price_from_tick_index(tick),
            tick_current_index: tick,
            token_mint_a: Pubkey::default(),
            token_vault_a: Pubkey::default(),
            fee_growth_global_a: 0,
            token_mint_b: Pubkey::default(),
            token_vault_b: Pubkey::default(),
            fee_growth_global_b: 0,
            reward_last_updated_timestamp_s: 0,
        }
    }

    fn array(start: i32, tick_spacing: u16, ticks: &[(i32, i128)]) -> TickArray {
        let mut array = TickArray::uninitialized(Pubkey::default(), start);
        for &(tick, net) in ticks {
            let offset = (tick - start) / i32::from(tick_spacing);
            array.liquidity_net[offset as usize] = Some(net);
        }
        array
    }

    /// `swap_test_fixture.rs`: arrays laid out from `start` in the swap's
    /// direction.
    fn arrays(
        start: i32,
        tick_spacing: u16,
        a_to_b: bool,
        ticks: &[&[(i32, i128)]],
    ) -> Vec<TickArray> {
        let direction = if a_to_b { -1 } else { 1 };
        ticks
            .iter()
            .enumerate()
            .map(|(i, ticks)| {
                let start =
                    start + i32::from(tick_spacing) * TICK_ARRAY_SIZE as i32 * i as i32 * direction;
                array(start, tick_spacing, ticks)
            })
            .collect()
    }

    fn run(
        pool: &Whirlpool,
        arrays: &[TickArray],
        amount: u64,
        limit_tick: i32,
        mode: SwapMode,
        direction: SwapDirection,
    ) -> SwapOutcome {
        let sequence: Vec<&TickArray> = arrays.iter().collect();
        let request = SwapRequest {
            direction,
            mode,
            amount,
            sqrt_price_limit_x64: sqrt_price_from_tick_index(limit_tick),
            timestamp_s: 100,
        };
        swap(
            pool,
            &sequence,
            FeeRate::new_static(pool.fee_rate),
            &request,
        )
        .unwrap()
    }

    /// `amount_a` and `amount_b` as the program's `PostSwapUpdate` has them.
    fn amounts(outcome: &SwapOutcome, direction: SwapDirection) -> (u64, u64) {
        match direction {
            SwapDirection::AToB => (outcome.amount_input, outcome.amount_output),
            SwapDirection::BToA => (outcome.amount_output, outcome.amount_input),
        }
    }

    /// `swap_manager.rs`, `zero_l_after_first_tick_b_to_a`.
    #[test]
    fn a_swap_right_that_runs_out_of_liquidity_matches_the_program() {
        let pool = pool(TS_8, 100_000, 255);
        let arrays = arrays(0, TS_8, false, &[&[(448, -100_000)], &[], &[]]);
        let outcome = run(
            &pool,
            &arrays,
            100_000,
            1720,
            SwapMode::ExactOut,
            SwapDirection::BToA,
        );
        assert_eq!(amounts(&outcome, SwapDirection::BToA), (948, 983));
        assert_eq!((outcome.tick_index, outcome.liquidity), (1720, 0));
        assert_eq!(outcome.ticks_crossed, 1);
    }

    /// `swap_manager.rs`, `zero_l_after_first_tick_a_to_b`.
    #[test]
    fn a_swap_left_that_runs_out_of_liquidity_matches_the_program() {
        let pool = pool(TS_8, 100_000, 1720);
        let arrays = arrays(1408, TS_8, true, &[&[], &[], &[(448, 100_000)]]);
        let outcome = run(
            &pool,
            &arrays,
            100_000,
            0,
            SwapMode::ExactOut,
            SwapDirection::AToB,
        );
        assert_eq!(amounts(&outcome, SwapDirection::AToB), (6026, 6715));
        assert_eq!((outcome.tick_index, outcome.liquidity), (-1, 0));
    }

    /// `swap_manager.rs`, `next_initialized_tick_in_next_array_b_to_a`.
    #[test]
    fn a_swap_right_across_two_arrays_matches_the_program() {
        let pool = pool(TS_128, 11_000_000, 25000);
        let arrays = arrays(
            22528,
            TS_128,
            false,
            &[
                &[(23168, 5_000_000), (28416, 6_000_000)],
                &[(33920, -6_000_000), (37504, -5_000_000)],
            ],
        );
        let outcome = run(
            &pool,
            &arrays,
            11_000_000,
            37000,
            SwapMode::ExactOut,
            SwapDirection::BToA,
        );
        assert_eq!(amounts(&outcome, SwapDirection::BToA), (1770620, 39429146));
        assert_eq!((outcome.tick_index, outcome.liquidity), (37000, 11_000_000));
        assert_eq!((outcome.ticks_crossed, outcome.tick_arrays_entered), (2, 2));
    }

    /// `swap_manager.rs`, `next_initialized_tick_in_next_array_a_to_b`.
    #[test]
    fn a_swap_left_across_two_arrays_matches_the_program() {
        let pool = pool(TS_128, 11_000_000, 37000);
        let arrays = arrays(
            29824,
            TS_128,
            true,
            &[
                &[(30720, -6_000_000), (37504, -5_000_000)],
                &[(23168, 5_000_000), (28416, 6_000_000)],
            ],
        );
        let outcome = run(
            &pool,
            &arrays,
            110_000_000,
            25000,
            SwapMode::ExactOut,
            SwapDirection::AToB,
        );
        assert_eq!(amounts(&outcome, SwapDirection::AToB), (1579669, 34593019));
        assert_eq!((outcome.tick_index, outcome.liquidity), (25000, 11_000_000));
    }

    #[test]
    fn start_indexes_follow_the_program() {
        // `sparse_swap.rs`, `test_get_start_tick_indexes`, tick spacing 1.
        let starts = |tick, direction| tick_array_start_indexes(&pool(1, 0, tick), direction);
        assert_eq!(starts(0, SwapDirection::AToB), vec![0, -88, -176]);
        assert_eq!(starts(-1, SwapDirection::AToB), vec![-88, -176, -264]);
        assert_eq!(starts(-443608, SwapDirection::AToB), vec![-443608, -443696]);
        assert_eq!(starts(-443635, SwapDirection::AToB), vec![-443696]);
        assert_eq!(starts(86, SwapDirection::BToA), vec![0, 88, 176]);
        assert_eq!(starts(87, SwapDirection::BToA), vec![88, 176, 264]);
        assert_eq!(starts(443600, SwapDirection::BToA), vec![443520, 443608]);
        assert_eq!(starts(443608, SwapDirection::BToA), vec![443608]);
        let starts_64 = |tick, direction| tick_array_start_indexes(&pool(64, 0, tick), direction);
        assert_eq!(
            starts_64(-64, SwapDirection::AToB),
            vec![-5632, -11264, -16896]
        );
        assert_eq!(starts_64(-443635, SwapDirection::AToB), vec![-444928]);
        assert_eq!(starts_64(5567, SwapDirection::BToA), vec![0, 5632, 11264]);
    }

    #[test]
    fn a_swap_that_needs_a_fourth_array_fails_as_the_program_does() {
        let pool = pool(TS_8, 1_000_000_000, 0);
        let arrays = arrays(0, TS_8, false, &[&[], &[], &[]]);
        let sequence: Vec<&TickArray> = arrays.iter().collect();
        let request = SwapRequest {
            direction: SwapDirection::BToA,
            mode: SwapMode::ExactIn,
            amount: u64::MAX / 2,
            sqrt_price_limit_x64: 0,
            timestamp_s: 0,
        };
        let result = swap(&pool, &sequence, FeeRate::new_static(0), &request);
        assert_eq!(
            result,
            Err(WhirlpoolError::Program(
                ProgramFailure::TickArraySequenceInvalidIndex
            ))
        );
    }

    #[test]
    fn requests_the_program_refuses_are_refused() {
        let pool = pool(TS_8, 1_000_000, 0);
        let arrays = arrays(0, TS_8, true, &[&[], &[], &[]]);
        let sequence: Vec<&TickArray> = arrays.iter().collect();
        let mut request = SwapRequest {
            direction: SwapDirection::AToB,
            mode: SwapMode::ExactIn,
            amount: 0,
            sqrt_price_limit_x64: 0,
            timestamp_s: 0,
        };
        let failure =
            |request: &SwapRequest| match swap(&pool, &sequence, FeeRate::new_static(0), request) {
                Err(WhirlpoolError::Program(failure)) => failure,
                other => panic!("expected a program failure, got {other:?}"),
            };
        assert_eq!(failure(&request), ProgramFailure::ZeroTradableAmount);
        request.amount = 1;
        request.sqrt_price_limit_x64 = sqrt_price_from_tick_index(10);
        assert_eq!(
            failure(&request),
            ProgramFailure::InvalidSqrtPriceLimitDirection
        );
        request.sqrt_price_limit_x64 = MIN_SQRT_PRICE_X64 - 1;
        assert_eq!(failure(&request), ProgramFailure::SqrtPriceOutOfBounds);
    }
}
