//! A swap's amounts, as the program computes them: `swap_v2`'s
//! `exact_internal_v2`, `swap_internal` and `compute_swap` ported from the
//! source line by line, so a quote is what the program transfers, to the
//! atom.
//!
//! The port keeps every computation that changes an amount, a price or a
//! tick: the Q64.64 tick math, each step's rounding in every fee mode, the
//! dynamic fee, limit orders resting on ticks, and liquidity crossing. It
//! leaves out what only the pool's records keep (fee growth, the protocol
//! and fund fee split, rewards, the oracle) and the Token-2022 transfer
//! fee, which the pools' mints don't charge: a quote takes it as zero.
//!
//! | Program | Here |
//! |---------|------|
//! | `swap_v2` / `exact_internal_v2` | [`swap`] |
//! | `swap_internal`, `SwapState` | `swap_internal`, `SwapState` |
//! | `swap_math::compute_swap` | [`compute_swap_step`] |
//! | `tick_math::get_sqrt_price_at_tick`, `get_tick_at_sqrt_price` | [`sqrt_price_at_tick`], [`tick_at_sqrt_price`] |
//! | `TickState::match_limit_order_with_sqrt_price` | `match_limit_order` |

use solana_pubkey::Pubkey;

use super::{
    AmmConfig, DynamicFeeInfo, PoolState, TICK_ARRAY_SIZE, Tick, TickArray,
    math::{MAX_SQRT_PRICE_X64, MAX_TICK, MIN_SQRT_PRICE_X64, MIN_TICK},
    tick_array_start,
};

/// Fee rates are in millionths.
const FEE_RATE_DENOMINATOR: u32 = 1_000_000;
/// The most the trade fee and the dynamic fee may charge together.
const MAX_FEE_RATE: u32 = 100_000;
const VOLATILITY_ACCUMULATOR_SCALE: u64 = 10_000;
const REDUCTION_FACTOR_DENOMINATOR: u64 = 10_000;
const DYNAMIC_FEE_CONTROL_DENOMINATOR: u128 = 100_000;
/// The status bit that disables swaps.
const SWAP_DISABLED: u8 = 1 << 4;
/// The pool's bitmap holds 1024 tick arrays, half on either side of tick 0.
const BITMAP_HALF: i32 = 512;
/// The swap loop's steps before a quote gives up: far beyond what the
/// program's compute budget allows.
const MAX_STEPS: u32 = 1 << 20;

/// What a swap pays and gets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapQuote {
    /// Atoms of the token in that leave the payer, fee included.
    pub amount_in: u64,
    /// Atoms of the token out that the payer receives, net of any fee.
    pub amount_out: u64,
    /// The trade fee, in atoms of the token it was charged in.
    pub fee: u64,
    /// Whether the fee was charged in token 0 (else token 1).
    pub fee_in_token_0: bool,
    /// The pool's price, tick and active liquidity after the swap.
    pub sqrt_price_x64: u128,
    pub tick: i32,
    pub liquidity: u128,
    /// Initialized ticks the price moved through.
    pub ticks_crossed: u32,
    /// The start ticks of the tick arrays the swap walked, in the order
    /// the instruction must pass them (the first is the current one).
    pub tick_arrays: Vec<i32>,
}

/// Why the program would refuse a swap; each names the program's error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SwapError {
    #[error("swaps are disabled on this pool (NotApproved)")]
    NotApproved,
    #[error("the pool isn't open at this block time")]
    NotOpen,
    #[error("the swap amount is zero (ZeroAmountSpecified)")]
    ZeroAmount,
    #[error(
        "the price limit is on the wrong side of the price or out of range (SqrtPriceLimitOverflow)"
    )]
    PriceLimit,
    #[error("tick array {0} is needed but wasn't given (NotEnoughTickArrayAccount)")]
    MissingTickArray(i32),
    #[error(
        "the swap needs the pool's tick array bitmap extension, which a quote doesn't read (MissingTickArrayBitmapExtensionAccount)"
    )]
    BitmapExtension,
    #[error("the pool has no liquidity in this direction (InsufficientLiquidityForDirection)")]
    NoLiquidityForDirection,
    #[error("the pool's liquidity ran out before the swap filled (LiquidityInsufficient)")]
    LiquidityInsufficient,
    #[error("the swap's input or output is zero atoms (TooSmallInputOrOutputAmount)")]
    TooSmall,
    #[error("filled {filled} of {wanted} atoms with no price limit, which the program refuses")]
    PartialFill { filled: u64, wanted: u64 },
    #[error("arithmetic overflow (CalculateOverflow)")]
    Overflow,
    #[error("an amount exceeds u64 (MaxTokenOverflow)")]
    MaxTokenOverflow,
    #[error("a tick array holds no initialized tick (InvalidTickArray)")]
    InvalidTickArray,
    #[error("a tick is out of range (InvalidTickIndex)")]
    TickOutOfRange,
    #[error("a sqrt price is out of range (SqrtPriceX64)")]
    SqrtPriceOutOfRange,
    #[error("liquidity is zero (ZeroLiquidity)")]
    ZeroLiquidity,
    #[error("crossing a tick takes liquidity below zero or past u128 (LiquiditySubValueErr)")]
    LiquidityDelta,
    #[error("a limit order fills more than the tick holds (InvalidLimitOrderAmount)")]
    LimitOrderAmount,
    #[error("the program's check fails: {0}")]
    Check(&'static str),
    #[error("the swap makes no progress; the program would run out of compute")]
    Stalled,
}

/// The program's `swap_v2` on `pool`, as its amounts come out: exact in
/// (`is_base_input`, `amount` is what goes in) or exact out (`amount` is
/// what comes out), from token 0 to token 1 (`zero_for_one`) or back, up
/// to `sqrt_price_limit_x64` (0 for none: the program then refuses a
/// partial fill). `tick_arrays` are those the caller loaded, in any order;
/// `block_timestamp` is the block's Unix time. Token-2022 transfer fees are
/// taken as zero.
#[allow(clippy::too_many_arguments)]
pub fn swap(
    pool: &PoolState,
    config: &AmmConfig,
    tick_arrays: &[TickArray],
    zero_for_one: bool,
    amount: u64,
    is_base_input: bool,
    sqrt_price_limit_x64: u128,
    block_timestamp: u64,
) -> Result<SwapQuote, SwapError> {
    if block_timestamp <= pool.open_time {
        return Err(SwapError::NotOpen);
    }
    let limit = match (sqrt_price_limit_x64, zero_for_one) {
        (0, true) => MIN_SQRT_PRICE_X64 + 1,
        (0, false) => MAX_SQRT_PRICE_X64 - 1,
        (limit, _) => limit,
    };
    // The program hands the swap loop the block time cut to 32 bits.
    let run = swap_internal(
        pool,
        config,
        tick_arrays,
        amount,
        limit,
        zero_for_one,
        is_base_input,
        u64::from(block_timestamp as u32),
    )?;
    if run.amount_0 == 0 || run.amount_1 == 0 {
        return Err(SwapError::TooSmall);
    }
    let moved_right_way = if zero_for_one {
        pool.sqrt_price_x64 >= run.sqrt_price_x64
    } else {
        run.sqrt_price_x64 >= pool.sqrt_price_x64
    };
    if !moved_right_way {
        return Err(SwapError::Check("the price moved against the swap"));
    }
    let (amount_in, amount_out) = if zero_for_one {
        (run.amount_0, run.amount_1)
    } else {
        (run.amount_1, run.amount_0)
    };
    if sqrt_price_limit_x64 == 0 {
        let filled = if is_base_input { amount_in } else { amount_out };
        if filled != amount {
            return Err(SwapError::PartialFill {
                filled,
                wanted: amount,
            });
        }
    }
    Ok(SwapQuote {
        amount_in,
        amount_out,
        fee: run.fee,
        fee_in_token_0: run.fee_in_token_0,
        sqrt_price_x64: run.sqrt_price_x64,
        tick: run.tick,
        liquidity: run.liquidity,
        ticks_crossed: run.ticks_crossed,
        tick_arrays: run.arrays.iter().map(|a| a.start_tick_index).collect(),
    })
}

/// What the swap loop leaves: the amounts each token moves, as the
/// program's `SwapInternalResult` has them, and the tick arrays it walked,
/// as the swap left them.
struct Run {
    amount_0: u64,
    amount_1: u64,
    fee: u64,
    fee_in_token_0: bool,
    sqrt_price_x64: u128,
    tick: i32,
    liquidity: u128,
    ticks_crossed: u32,
    arrays: Vec<TickArray>,
}

/// The program's `swap_internal`: the loop that walks the price from tick
/// to tick, `amount_specified` at a time.
#[allow(clippy::too_many_arguments)]
fn swap_internal(
    pool: &PoolState,
    config: &AmmConfig,
    tick_arrays: &[TickArray],
    amount_specified: u64,
    sqrt_price_limit_x64: u128,
    zero_for_one: bool,
    is_base_input: bool,
    block_timestamp: u64,
) -> Result<Run, SwapError> {
    if amount_specified == 0 {
        return Err(SwapError::ZeroAmount);
    }
    if pool.status & SWAP_DISABLED != 0 {
        return Err(SwapError::NotApproved);
    }
    let limit_ok = if zero_for_one {
        sqrt_price_limit_x64 < pool.sqrt_price_x64 && sqrt_price_limit_x64 > MIN_SQRT_PRICE_X64
    } else {
        sqrt_price_limit_x64 > pool.sqrt_price_x64 && sqrt_price_limit_x64 < MAX_SQRT_PRICE_X64
    };
    if !limit_ok {
        return Err(SwapError::PriceLimit);
    }

    let spacing = pool.tick_spacing;
    let (mut first_array_holds_tick, mut current_start) = first_tick_array(pool, zero_for_one)?;
    let mut arrays = Arrays {
        pool: pool.address,
        given: tick_arrays,
        walked: Vec::new(),
    };
    let mut current = arrays.load(current_start)?;

    let is_fee_on_input = is_fee_on_input(pool.fee_on, zero_for_one);
    let mut state = SwapState::new(
        pool,
        amount_specified,
        config.trade_fee_rate,
        block_timestamp,
    );
    let mut ticks_crossed = 0;
    let mut steps = 0;
    while state.amount_specified_remaining != 0 && state.sqrt_price_x64 != sqrt_price_limit_x64 {
        let offset =
            match next_initialized_tick(&arrays.walked[current], state.tick, spacing, zero_for_one)
            {
                Some(offset) => offset,
                // The pool's own array holds no initialized tick: the swap
                // starts at the first one of the next array, once.
                None if !first_array_holds_tick => {
                    first_array_holds_tick = true;
                    first_initialized_tick(&arrays.walked[current], zero_for_one)?
                }
                None => {
                    let next_start = next_tick_array(pool, current_start, zero_for_one)?
                        .ok_or(SwapError::LiquidityInsufficient)?;
                    current = arrays.load(next_start)?;
                    current_start = next_start;
                    first_initialized_tick(&arrays.walked[current], zero_for_one)?
                }
            };
        let mut next = arrays.walked[current].ticks[offset];

        let target_price = state.target_price(next.tick, zero_for_one, sqrt_price_limit_x64)?;
        let mut liquidity_next = state.liquidity;
        loop {
            steps += 1;
            if steps > MAX_STEPS {
                return Err(SwapError::Stalled);
            }
            state.update_volatility_accumulator();
            let fee_rate = state.total_fee_rate()?;
            let (skipped_tick_spacing, bounded_price, bounded_tick) =
                state.spacing_bounded_price(target_price, zero_for_one)?;

            let step = if state.sqrt_price_x64 != bounded_price {
                let step = compute_swap_step(
                    state.sqrt_price_x64,
                    bounded_price,
                    state.liquidity,
                    state.amount_specified_remaining,
                    fee_rate,
                    is_base_input,
                    zero_for_one,
                    is_fee_on_input,
                )?;
                state.apply(
                    step.amount_in,
                    step.amount_out,
                    step.fee_amount,
                    is_base_input,
                    is_fee_on_input,
                )?;
                step
            } else {
                SwapStep::at(bounded_price)
            };

            let unfilled_before = unfilled(&next)?;
            if state.sqrt_price_next_x64 == step.sqrt_price_next_x64 {
                // At the tick's price: its limit orders fill first, then
                // the price crosses it if none are left.
                let fill = match_limit_order(
                    &mut next,
                    state.amount_specified_remaining,
                    zero_for_one,
                    is_base_input,
                    fee_rate,
                    is_fee_on_input,
                    state.sqrt_price_next_x64,
                )?;
                if fill.amount_in != 0 || fill.amount_out != 0 || fill.fee_amount != 0 {
                    state.apply(
                        fill.amount_in,
                        fill.amount_out,
                        fill.fee_amount,
                        is_base_input,
                        is_fee_on_input,
                    )?;
                }
                let passed = !has_limit_orders(&next);
                if next.liquidity_gross > 0 && passed {
                    let net = if zero_for_one {
                        next.liquidity_net
                            .checked_neg()
                            .ok_or(SwapError::Overflow)?
                    } else {
                        next.liquidity_net
                    };
                    liquidity_next = add_delta(state.liquidity, net)?;
                }
                arrays.walked[current].ticks[offset] = next;
                if passed {
                    ticks_crossed += 1;
                }
                // The price sits on the tick: the pool's tick is the one
                // below the price's side of it.
                state.tick = if zero_for_one == passed {
                    state.tick_next - 1
                } else {
                    state.tick_next
                };
            } else if state.sqrt_price_x64 != step.sqrt_price_next_x64 {
                state.tick = match bounded_tick {
                    Some(tick) if step.sqrt_price_next_x64 == bounded_price => tick,
                    _ => tick_at_sqrt_price(step.sqrt_price_next_x64)?,
                };
            }
            state.sqrt_price_x64 = step.sqrt_price_next_x64;
            state.update_dynamic_fee_index(zero_for_one, skipped_tick_spacing);
            if state.amount_specified_remaining == 0 || state.sqrt_price_x64 == target_price {
                let unfilled_after = unfilled(&next)?;
                if state.amount_specified_remaining != 0
                    && unfilled_after != unfilled_before
                    && unfilled_after != 0
                {
                    return Err(SwapError::Check(
                        "a limit order filled in part while the swap went on",
                    ));
                }
                break;
            }
        }
        state.liquidity = liquidity_next;
    }

    let consumed = amount_specified
        .checked_sub(state.amount_specified_remaining)
        .ok_or(SwapError::Overflow)?;
    let (amount_0, amount_1) = if zero_for_one == is_base_input {
        (consumed, state.amount_calculated)
    } else {
        (state.amount_calculated, consumed)
    };
    Ok(Run {
        amount_0,
        amount_1,
        fee: state.fee,
        fee_in_token_0: is_fee_on_token_0(pool.fee_on, zero_for_one),
        sqrt_price_x64: state.sqrt_price_x64,
        tick: state.tick,
        liquidity: state.liquidity,
        ticks_crossed,
        arrays: arrays.walked,
    })
}

/// The swap loop's state, as the program's `SwapState` keeps it, less the
/// fee growth.
struct SwapState {
    amount_specified_remaining: u64,
    amount_calculated: u64,
    sqrt_price_x64: u128,
    tick: i32,
    /// The trade fee charged so far.
    fee: u64,
    liquidity: u128,
    sqrt_price_next_x64: u128,
    tick_next: i32,
    tick_spacing: u16,
    base_fee_rate: u32,
    /// The group of `tick_spacing` ticks the price is in, for the dynamic
    /// fee.
    tick_spacing_index: i32,
    dynamic_fee_info: Option<DynamicFeeInfo>,
}

impl SwapState {
    fn new(
        pool: &PoolState,
        amount_specified: u64,
        base_fee_rate: u32,
        block_timestamp: u64,
    ) -> Self {
        let mut state = Self {
            amount_specified_remaining: amount_specified,
            amount_calculated: 0,
            sqrt_price_x64: pool.sqrt_price_x64,
            tick: pool.tick_current,
            fee: 0,
            liquidity: pool.liquidity,
            sqrt_price_next_x64: 0,
            tick_next: 0,
            tick_spacing: pool.tick_spacing,
            base_fee_rate,
            tick_spacing_index: 0,
            dynamic_fee_info: pool.dynamic_fee_info,
        };
        if let Some(info) = &mut state.dynamic_fee_info {
            state.tick_spacing_index = tick_spacing_index(state.tick, state.tick_spacing);
            update_reference(info, state.tick_spacing_index, block_timestamp);
        }
        state
    }

    /// Counts a step's amounts against what is left to swap (the program's
    /// `apply_swap_amounts`).
    fn apply(
        &mut self,
        amount_in: u64,
        amount_out: u64,
        fee_amount: u64,
        is_base_input: bool,
        is_fee_on_input: bool,
    ) -> Result<(), SwapError> {
        let consumed = if is_fee_on_input {
            amount_in
                .checked_add(fee_amount)
                .ok_or(SwapError::Overflow)?
        } else {
            amount_in
        };
        let (spent, gained) = if is_base_input {
            (consumed, amount_out)
        } else {
            (amount_out, consumed)
        };
        self.amount_specified_remaining = self
            .amount_specified_remaining
            .checked_sub(spent)
            .ok_or(SwapError::Overflow)?;
        self.amount_calculated = self
            .amount_calculated
            .checked_add(gained)
            .ok_or(SwapError::Overflow)?;
        self.fee = self
            .fee
            .checked_add(fee_amount)
            .ok_or(SwapError::Overflow)?;
        Ok(())
    }

    /// The price the next step aims at: the next tick's, or the limit if
    /// that comes first.
    fn target_price(
        &mut self,
        tick_next: i32,
        zero_for_one: bool,
        limit: u128,
    ) -> Result<u128, SwapError> {
        self.tick_next = tick_next.clamp(MIN_TICK, MAX_TICK);
        self.sqrt_price_next_x64 = sqrt_price_at_tick(self.tick_next)?;
        let next = self.sqrt_price_next_x64;
        let target = if (zero_for_one && next < limit) || (!zero_for_one && next > limit) {
            limit
        } else {
            next
        };
        let ahead = if zero_for_one {
            self.tick >= self.tick_next
                && self.sqrt_price_x64 >= next
                && self.sqrt_price_x64 >= target
        } else {
            self.tick_next > self.tick
                && next >= self.sqrt_price_x64
                && target >= self.sqrt_price_x64
        };
        if !ahead {
            return Err(SwapError::Check("the next tick lies behind the price"));
        }
        Ok(target)
    }

    fn update_volatility_accumulator(&mut self) {
        if let Some(info) = &mut self.dynamic_fee_info {
            update_volatility_accumulator(info, self.tick_spacing_index);
        }
    }

    /// The pool's trade fee plus the dynamic fee, in millionths.
    fn total_fee_rate(&self) -> Result<u32, SwapError> {
        let Some(info) = &self.dynamic_fee_info else {
            return Ok(self.base_fee_rate);
        };
        let crossed = info
            .volatility_accumulator
            .checked_mul(u32::from(self.tick_spacing))
            .ok_or(SwapError::Overflow)?;
        let squared = u64::from(crossed) * u64::from(crossed);
        let denominator = DYNAMIC_FEE_CONTROL_DENOMINATOR
            * u128::from(VOLATILITY_ACCUMULATOR_SCALE)
            * u128::from(VOLATILITY_ACCUMULATOR_SCALE);
        let dynamic = (u128::from(info.dynamic_fee_control) * u128::from(squared))
            .div_ceil(denominator)
            .min(u128::from(MAX_FEE_RATE)) as u32;
        let total = self
            .base_fee_rate
            .checked_add(dynamic)
            .ok_or(SwapError::Overflow)?;
        Ok(total.min(MAX_FEE_RATE))
    }

    /// With a dynamic fee, a step stops at the edge of the price's group of
    /// ticks, so each group pays its own rate. Returns whether no such edge
    /// applies, the price to step to, and the edge's tick where it does.
    fn spacing_bounded_price(
        &self,
        target_price: u128,
        zero_for_one: bool,
    ) -> Result<(bool, u128, Option<i32>), SwapError> {
        let Some(info) = &self.dynamic_fee_info else {
            return Ok((true, target_price, None));
        };
        if self.liquidity == 0 || info.volatility_accumulator == info.max_volatility_accumulator {
            return Ok((true, target_price, None));
        }
        let spacing = i32::from(self.tick_spacing);
        let bounded_tick = if zero_for_one {
            self.tick_spacing_index.saturating_mul(spacing)
        } else {
            self.tick_spacing_index
                .saturating_add(1)
                .saturating_mul(spacing)
        }
        .clamp(MIN_TICK, MAX_TICK);
        let bounded_price = sqrt_price_at_tick(bounded_tick)?;
        let short_of_edge = if zero_for_one {
            target_price > bounded_price
        } else {
            target_price < bounded_price
        };
        Ok(if short_of_edge {
            (false, target_price, None)
        } else {
            (false, bounded_price, Some(bounded_tick))
        })
    }

    /// Moves the dynamic fee's group of ticks on after a step.
    fn update_dynamic_fee_index(&mut self, zero_for_one: bool, skipped_tick_spacing: bool) {
        let Some(info) = self.dynamic_fee_info else {
            return;
        };
        if skipped_tick_spacing {
            let tick = if self.sqrt_price_x64 == self.sqrt_price_next_x64 {
                self.tick_next
            } else {
                self.tick
            };
            let mut index = tick_spacing_index(tick, self.tick_spacing);
            if !zero_for_one && tick % i32::from(self.tick_spacing) == 0 {
                index -= 1;
            }
            self.tick_spacing_index = index;
            if info.volatility_accumulator != info.max_volatility_accumulator {
                self.update_volatility_accumulator();
            }
        }
        self.tick_spacing_index += if zero_for_one { -1 } else { 1 };
    }
}

/// The group of `tick_spacing` ticks `tick` lies in, rounded down.
fn tick_spacing_index(tick: i32, tick_spacing: u16) -> i32 {
    let spacing = i32::from(tick_spacing);
    if tick % spacing == 0 || tick >= 0 {
        tick / spacing
    } else {
        tick / spacing - 1
    }
}

/// The dynamic fee's reference, moved on at the swap's start: kept within
/// the filter period, decayed within the decay period, reset after it.
fn update_reference(info: &mut DynamicFeeInfo, tick_spacing_index: i32, now: u64) {
    let since = now.saturating_sub(info.last_update_timestamp);
    if since < u64::from(info.filter_period) {
        return;
    }
    info.tick_spacing_index_reference = tick_spacing_index;
    info.volatility_reference = if since < u64::from(info.decay_period) {
        (u64::from(info.volatility_accumulator) * u64::from(info.reduction_factor)
            / REDUCTION_FACTOR_DENOMINATOR) as u32
    } else {
        0
    };
    info.last_update_timestamp = now;
}

/// The volatility so far: the reference plus how many groups of ticks the
/// price is from the reference's, capped.
fn update_volatility_accumulator(info: &mut DynamicFeeInfo, tick_spacing_index: i32) {
    let delta = (info.tick_spacing_index_reference - tick_spacing_index).unsigned_abs();
    let accumulator =
        u64::from(info.volatility_reference) + u64::from(delta) * VOLATILITY_ACCUMULATOR_SCALE;
    info.volatility_accumulator =
        accumulator.min(u64::from(info.max_volatility_accumulator)) as u32;
}

/// Whether the fee comes off the token in (the program's
/// `is_fee_on_input`).
fn is_fee_on_input(fee_on: u8, zero_for_one: bool) -> bool {
    match fee_on {
        1 => zero_for_one,
        2 => !zero_for_one,
        _ => true,
    }
}

fn is_fee_on_token_0(fee_on: u8, zero_for_one: bool) -> bool {
    match fee_on {
        1 => true,
        2 => false,
        _ => zero_for_one,
    }
}

/// One step of a swap within a stretch of constant liquidity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SwapStep {
    /// The price the step reaches, no further than its target.
    pub sqrt_price_next_x64: u128,
    /// Atoms in, the fee left out.
    pub amount_in: u64,
    /// Atoms out: with the fee on the output, net of it.
    pub amount_out: u64,
    pub fee_amount: u64,
}

impl SwapStep {
    fn at(sqrt_price_next_x64: u128) -> Self {
        Self {
            sqrt_price_next_x64,
            ..Self::default()
        }
    }
}

/// The program's `compute_swap`: how far `amount_remaining` (in, or with
/// `is_base_input` off, out) moves the price from `sqrt_price_current_x64`
/// toward `sqrt_price_target_x64` at `liquidity`, and what that costs at
/// `fee_rate` millionths, charged on the input or the output.
#[allow(clippy::too_many_arguments)]
pub fn compute_swap_step(
    sqrt_price_current_x64: u128,
    sqrt_price_target_x64: u128,
    liquidity: u128,
    amount_remaining: u64,
    fee_rate: u32,
    is_base_input: bool,
    zero_for_one: bool,
    is_fee_on_input: bool,
) -> Result<SwapStep, SwapError> {
    let denominator = u64::from(FEE_RATE_DENOMINATOR);
    let complement = u64::from(
        FEE_RATE_DENOMINATOR
            .checked_sub(fee_rate)
            .ok_or(SwapError::Overflow)?,
    );
    let fee_rate_64 = u64::from(fee_rate);

    // What moves the price: exact in less its fee, or exact out grossed up
    // for a fee taken from it.
    let amount_for_price = match (is_base_input, is_fee_on_input) {
        (true, true) => mul_div_u64(amount_remaining, complement, denominator, false)?,
        (false, false) => mul_div_u64(amount_remaining, denominator, complement, true)?,
        _ => amount_remaining,
    };

    // Both amounts at the target; one past u64 means the target is out of
    // reach.
    let reachable = match delta_amounts_for_swap(
        sqrt_price_target_x64,
        sqrt_price_current_x64,
        liquidity,
        zero_for_one,
    ) {
        Ok((amount_in, amount_out)) => {
            let needed = if is_base_input { amount_in } else { amount_out };
            (amount_for_price >= needed).then_some((amount_in, amount_out))
        }
        Err(SwapError::MaxTokenOverflow) => None,
        Err(e) => return Err(e),
    };
    let mut step = match reachable {
        Some((amount_in, amount_out)) => SwapStep {
            sqrt_price_next_x64: sqrt_price_target_x64,
            amount_in,
            amount_out,
            fee_amount: 0,
        },
        None => {
            let sqrt_next = if is_base_input {
                next_sqrt_price_from_input(
                    sqrt_price_current_x64,
                    liquidity,
                    amount_for_price,
                    zero_for_one,
                )?
            } else {
                next_sqrt_price_from_output(
                    sqrt_price_current_x64,
                    liquidity,
                    amount_for_price,
                    zero_for_one,
                )?
            };
            let (amount_in, amount_out) =
                delta_amounts_for_swap(sqrt_next, sqrt_price_current_x64, liquidity, zero_for_one)?;
            SwapStep {
                sqrt_price_next_x64: sqrt_next,
                amount_in,
                amount_out,
                fee_amount: 0,
            }
        }
    };
    let within_target = if zero_for_one {
        step.sqrt_price_next_x64 >= sqrt_price_target_x64
    } else {
        sqrt_price_target_x64 >= step.sqrt_price_next_x64
    };
    if !within_target {
        return Err(SwapError::Check("the step passes its target price"));
    }
    let reached = step.sqrt_price_next_x64 == sqrt_price_target_x64;

    match (is_base_input, is_fee_on_input) {
        (true, true) => {
            // Short of the target, whatever the price didn't take is fee.
            step.fee_amount = if reached {
                mul_div_u64(step.amount_in, fee_rate_64, complement, true)?
            } else {
                amount_remaining
                    .checked_sub(step.amount_in)
                    .ok_or(SwapError::Overflow)?
            };
        }
        (true, false) => {
            step.fee_amount = mul_div_u64(step.amount_out, fee_rate_64, denominator, true)?;
            step.amount_out = step
                .amount_out
                .checked_sub(step.fee_amount)
                .ok_or(SwapError::Overflow)?;
            // Short of the target, the input's rounding dust goes to the
            // pool too: the step takes it all.
            if !reached {
                step.amount_in = amount_remaining;
            }
        }
        (false, true) => {
            step.amount_out = step.amount_out.min(amount_remaining);
            step.fee_amount = mul_div_u64(step.amount_in, fee_rate_64, complement, true)?;
        }
        (false, false) => {
            step.fee_amount = mul_div_u64(step.amount_out, fee_rate_64, denominator, true)?;
            let net = step
                .amount_out
                .checked_sub(step.fee_amount)
                .ok_or(SwapError::Overflow)?;
            if net > amount_remaining {
                step.fee_amount = step
                    .amount_out
                    .checked_sub(amount_remaining)
                    .ok_or(SwapError::Overflow)?;
                step.amount_out = amount_remaining;
            } else {
                step.amount_out = net;
            }
        }
    }
    Ok(step)
}

/// What a limit order fill moves: the taker's input and output, and the
/// trade fee on it.
#[derive(Debug, Default)]
struct LimitOrderFill {
    amount_in: u64,
    amount_out: u64,
    fee_amount: u64,
}

/// The program's `match_limit_order_with_sqrt_price`: the swap takes the
/// tick's limit orders at the tick's price, the oldest cohort first, and
/// pays the trade fee on them.
fn match_limit_order(
    tick: &mut Tick,
    swap_amount: u64,
    zero_for_one: bool,
    is_base_input: bool,
    fee_rate: u32,
    is_fee_on_input: bool,
    sqrt_price_x64: u128,
) -> Result<LimitOrderFill, SwapError> {
    let mut fill = LimitOrderFill::default();
    let unfilled_total = unfilled(tick)?;
    if swap_amount == 0 || unfilled_total == 0 {
        return Ok(fill);
    }
    let denominator = u64::from(FEE_RATE_DENOMINATOR);
    let complement = u64::from(
        FEE_RATE_DENOMINATOR
            .checked_sub(fee_rate)
            .ok_or(SwapError::Overflow)?,
    );
    let fee_rate_64 = u64::from(fee_rate);
    let price_x64 = price_from_sqrt_price(sqrt_price_x64, !zero_for_one)?;

    if is_base_input {
        if is_fee_on_input {
            fill.fee_amount = mul_div_u64(swap_amount, fee_rate_64, denominator, true)?;
            fill.amount_in = swap_amount
                .checked_sub(fill.fee_amount)
                .ok_or(SwapError::Overflow)?;
        } else {
            fill.amount_in = swap_amount;
        }
        let matched = limit_order_output(fill.amount_in, price_x64, zero_for_one)?;
        if matched > u128::from(unfilled_total) {
            // The orders run out first: the input is what they're worth.
            fill.amount_out = unfilled_total;
            fill.amount_in = limit_order_input(unfilled_total, price_x64, !zero_for_one)?;
            if is_fee_on_input {
                fill.fee_amount = mul_div_u64(fill.amount_in, fee_rate_64, complement, true)?;
            }
        } else {
            fill.amount_out = matched as u64;
        }
    } else {
        let net_output = swap_amount.min(unfilled_total);
        fill.amount_out = if is_fee_on_input {
            net_output
        } else {
            mul_div_u64(net_output, denominator, complement, true)?.min(unfilled_total)
        };
        fill.amount_in = limit_order_input(fill.amount_out, price_x64, !zero_for_one)?;
        if is_fee_on_input {
            fill.fee_amount = mul_div_u64(fill.amount_in, fee_rate_64, complement, true)?;
        }
    }

    // The part-filled cohort fills first, then the untouched orders become
    // the next one.
    let mut from_part_filled = 0;
    if tick.part_filled_orders_remaining > 0 {
        from_part_filled = tick.part_filled_orders_remaining.min(fill.amount_out);
        tick.part_filled_orders_remaining -= from_part_filled;
    }
    let from_orders = fill.amount_out.saturating_sub(from_part_filled);
    if from_orders > 0 {
        if tick.part_filled_orders_remaining != 0 {
            return Err(SwapError::Check(
                "orders fill before the part-filled cohort",
            ));
        }
        if tick.orders_amount < from_orders {
            return Err(SwapError::LimitOrderAmount);
        }
        tick.part_filled_orders_remaining = tick.orders_amount - from_orders;
        tick.orders_amount = 0;
    }

    if !is_fee_on_input {
        fill.fee_amount = mul_div_u64(fill.amount_out, fee_rate_64, denominator, true)?;
        fill.amount_out = fill
            .amount_out
            .checked_sub(fill.fee_amount)
            .ok_or(SwapError::Overflow)?;
    }
    Ok(fill)
}

/// What `amount_in` buys from limit orders at `price_x64` (token 1 per
/// token 0, Q64.64), rounded down.
fn limit_order_output(
    amount_in: u64,
    price_x64: u128,
    zero_for_one: bool,
) -> Result<u128, SwapError> {
    let amount = U512::from_u128(u128::from(amount_in));
    let (num, denom) = if zero_for_one {
        (U512::from_u128(price_x64), U512::pow2(64))
    } else {
        (U512::pow2(64), U512::from_u128(price_x64))
    };
    mul_div(amount, num, denom, false, 128)
        .and_then(U512::to_u128)
        .ok_or(SwapError::Overflow)
}

/// What limit orders selling `amount_out` (in the direction
/// `order_zero_for_one`) take in at `price_x64`, rounded up.
fn limit_order_input(
    amount_out: u64,
    price_x64: u128,
    order_zero_for_one: bool,
) -> Result<u64, SwapError> {
    let amount = U512::from_u128(u128::from(amount_out));
    let (num, denom) = if order_zero_for_one {
        (U512::from_u128(price_x64), U512::pow2(64))
    } else {
        (U512::pow2(64), U512::from_u128(price_x64))
    };
    mul_div(amount, num, denom, true, 128)
        .and_then(U512::to_u64)
        .ok_or(SwapError::Overflow)
}

/// Limit orders on the tick not yet filled.
fn unfilled(tick: &Tick) -> Result<u64, SwapError> {
    tick.orders_amount
        .checked_add(tick.part_filled_orders_remaining)
        .ok_or(SwapError::Overflow)
}

fn has_limit_orders(tick: &Tick) -> bool {
    tick.orders_amount > 0 || tick.part_filled_orders_remaining > 0
}

/// A tick the swap stops at: one with liquidity or limit orders.
fn is_initialized(tick: &Tick) -> bool {
    tick.liquidity_gross > 0 || has_limit_orders(tick)
}

/// The tick arrays a swap loads, as it loads them.
struct Arrays<'a> {
    pool: Pubkey,
    given: &'a [TickArray],
    walked: Vec<TickArray>,
}

impl Arrays<'_> {
    /// Loads the pool's tick array starting at `start`: its index in
    /// `walked`.
    fn load(&mut self, start: i32) -> Result<usize, SwapError> {
        let array = self
            .given
            .iter()
            .find(|a| a.start_tick_index == start && a.pool == self.pool)
            .ok_or(SwapError::MissingTickArray(start))?;
        if array.ticks.len() != TICK_ARRAY_SIZE as usize {
            return Err(SwapError::InvalidTickArray);
        }
        self.walked.push(array.clone());
        Ok(self.walked.len() - 1)
    }
}

/// The offset in `array` of the next initialized tick from `tick`: at or
/// below it going down, above it going up. `None` where `tick` is outside
/// the array or nothing is left in it.
fn next_initialized_tick(
    array: &TickArray,
    tick: i32,
    tick_spacing: u16,
    zero_for_one: bool,
) -> Option<usize> {
    if tick_array_start(tick, tick_spacing) != array.start_tick_index {
        return None;
    }
    let offset = ((tick - array.start_tick_index) / i32::from(tick_spacing)) as usize;
    let initialized = |&i: &usize| is_initialized(&array.ticks[i]);
    if zero_for_one {
        (0..=offset).rev().find(initialized)
    } else {
        (offset + 1..TICK_ARRAY_SIZE as usize).find(initialized)
    }
}

/// The offset of the first initialized tick a swap meets entering
/// `array`: its highest going down, its lowest going up.
fn first_initialized_tick(array: &TickArray, zero_for_one: bool) -> Result<usize, SwapError> {
    let initialized = |&i: &usize| is_initialized(&array.ticks[i]);
    let mut offsets = 0..TICK_ARRAY_SIZE as usize;
    if zero_for_one {
        offsets.rev().find(initialized)
    } else {
        offsets.find(initialized)
    }
    .ok_or(SwapError::InvalidTickArray)
}

/// Ticks in one tick array.
fn array_span(tick_spacing: u16) -> i32 {
    TICK_ARRAY_SIZE * i32::from(tick_spacing)
}

/// The first tick array the swap uses, and whether it is the one holding
/// the pool's tick (else the next initialized one along).
fn first_tick_array(pool: &PoolState, zero_for_one: bool) -> Result<(bool, i32), SwapError> {
    let spacing = pool.tick_spacing;
    if beyond_pool_bitmap(pool.tick_current, spacing) {
        return Err(SwapError::BitmapExtension);
    }
    if !(MIN_TICK..=MAX_TICK).contains(&pool.tick_current) {
        return Err(SwapError::TickOutOfRange);
    }
    let start = tick_array_start(pool.tick_current, spacing);
    let bit = start / array_span(spacing) + BITMAP_HALF;
    if bitmap_bit(&pool.tick_array_bitmap, bit) {
        return Ok((true, start));
    }
    let next =
        next_tick_array(pool, start, zero_for_one)?.ok_or(SwapError::NoLiquidityForDirection)?;
    Ok((false, next))
}

/// Whether the tick array holding `tick` lies beyond what the pool's own
/// bitmap records.
fn beyond_pool_bitmap(tick: i32, tick_spacing: u16) -> bool {
    let span = array_span(tick_spacing);
    let mut max = span * BITMAP_HALF;
    let mut min = -max;
    if max > MAX_TICK {
        max = tick_array_start(MAX_TICK, tick_spacing) + span;
    }
    if min < MIN_TICK {
        min = tick_array_start(MIN_TICK, tick_spacing);
    }
    let start = tick_array_start(tick, tick_spacing);
    start >= max || start < min
}

fn bitmap_bit(bitmap: &[u64; 16], bit: i32) -> bool {
    let bit = bit as usize;
    bitmap[bit / 64] >> (bit % 64) & 1 == 1
}

/// The next initialized tick array past the one starting at `last`, from
/// the pool's own bitmap. Where that bitmap runs out, a pool with a tick
/// spacing under 15 needs its bitmap extension, which a quote doesn't read.
fn next_tick_array(
    pool: &PoolState,
    last: i32,
    zero_for_one: bool,
) -> Result<Option<i32>, SwapError> {
    let spacing = pool.tick_spacing;
    let last = tick_array_start(last, spacing);
    let valid_start = if !(MIN_TICK..=MAX_TICK).contains(&last) {
        last == tick_array_start(MIN_TICK, spacing)
    } else {
        last % array_span(spacing) == 0
    };
    if !valid_start {
        return Err(SwapError::TickOutOfRange);
    }
    let span = array_span(spacing);
    let boundary = span * BITMAP_HALF;
    let next = if zero_for_one {
        last - span
    } else {
        last + span
    };
    if (-boundary..boundary).contains(&next) {
        let from = next.div_euclid(span) + BITMAP_HALF;
        let found = if zero_for_one {
            (0..=from)
                .rev()
                .find(|&b| bitmap_bit(&pool.tick_array_bitmap, b))
        } else {
            (from..2 * BITMAP_HALF).find(|&b| bitmap_bit(&pool.tick_array_bitmap, b))
        };
        if let Some(bit) = found {
            return Ok(Some((bit - BITMAP_HALF) * span));
        }
    }
    if spacing >= 15 {
        Ok(None)
    } else {
        Err(SwapError::BitmapExtension)
    }
}

/// `x + y` for a liquidity and a signed change (the program's `add_delta`).
fn add_delta(x: u128, y: i128) -> Result<u128, SwapError> {
    if y < 0 {
        x.checked_sub(y.unsigned_abs())
    } else {
        x.checked_add(y.unsigned_abs())
    }
    .ok_or(SwapError::LiquidityDelta)
}

/// The sqrt price at `tick`, in Q64.64, bit for bit as the program
/// computes it.
pub fn sqrt_price_at_tick(tick: i32) -> Result<u128, SwapError> {
    // 2^64 / 1.0001^(2^(i-1)) for each bit i of the tick past the first.
    const FACTORS: [(u32, u128); 18] = [
        (0x2, 0xfff97272373d4000),
        (0x4, 0xfff2e50f5f657000),
        (0x8, 0xffe5caca7e10f000),
        (0x10, 0xffcb9843d60f7000),
        (0x20, 0xff973b41fa98e800),
        (0x40, 0xff2ea16466c9b000),
        (0x80, 0xfe5dee046a9a3800),
        (0x100, 0xfcbe86c7900bb000),
        (0x200, 0xf987a7253ac65800),
        (0x400, 0xf3392b0822bb6000),
        (0x800, 0xe7159475a2caf000),
        (0x1000, 0xd097f3bdfd2f2000),
        (0x2000, 0xa9f746462d9f8000),
        (0x4000, 0x70d869a156f31c00),
        (0x8000, 0x31be135f97ed3200),
        (0x10000, 0x9aa508b5b85a500),
        (0x20000, 0x5d6af8dedc582c),
        (0x40000, 0x2216e584f5fa),
    ];
    if !(MIN_TICK..=MAX_TICK).contains(&tick) {
        return Err(SwapError::TickOutOfRange);
    }
    let abs = tick.unsigned_abs();
    let mut ratio: u128 = if abs & 1 != 0 {
        0xfffcb933bd6fb800
    } else {
        1 << 64
    };
    for (bit, factor) in FACTORS {
        if abs & bit != 0 {
            ratio = (ratio * factor) >> 64;
        }
    }
    if tick > 0 {
        ratio = u128::MAX / ratio;
    }
    Ok(ratio)
}

/// The highest tick whose sqrt price is at or below `sqrt_price_x64`, bit
/// for bit as the program computes it: a 16-bit log2, then a check of the
/// two ticks it can't tell apart.
pub fn tick_at_sqrt_price(sqrt_price_x64: u128) -> Result<i32, SwapError> {
    if !(MIN_SQRT_PRICE_X64..MAX_SQRT_PRICE_X64).contains(&sqrt_price_x64) {
        return Err(SwapError::SqrtPriceOutOfRange);
    }
    let msb = 127 - sqrt_price_x64.leading_zeros();
    let log2p_integer_x32 = (i128::from(msb) - 64) << 32;

    let mut bit: i128 = 0x8000_0000_0000_0000;
    let mut log2p_fraction_x64: i128 = 0;
    let mut r = if msb >= 64 {
        sqrt_price_x64 >> (msb - 63)
    } else {
        sqrt_price_x64 << (63 - msb)
    };
    for _ in 0..16 {
        r *= r;
        let more_than_two = r >> 127;
        r >>= 63 + more_than_two;
        log2p_fraction_x64 += bit * more_than_two as i128;
        bit >>= 1;
    }
    let log2p_x32 = log2p_integer_x32 + (log2p_fraction_x64 >> 32);

    // Change of base: × 2^32 / log2(√1.0001), then ± the 16-bit error.
    let log_sqrt_10001_x64 = log2p_x32 * 59_543_866_431_248;
    let tick_low = ((log_sqrt_10001_x64 - 184_467_440_737_095_516) >> 64) as i32;
    let tick_high = ((log_sqrt_10001_x64 + 15_793_534_762_490_258_745) >> 64) as i32;
    if tick_low == tick_high || sqrt_price_at_tick(tick_high)? > sqrt_price_x64 {
        Ok(tick_low)
    } else {
        Ok(tick_high)
    }
}

/// Token 1 per token 0 in Q64.64 at a sqrt price, rounded down or up.
fn price_from_sqrt_price(sqrt_price_x64: u128, round_up: bool) -> Result<u128, SwapError> {
    let sqrt = U512::from_u128(sqrt_price_x64);
    let mut product = sqrt.mul(sqrt);
    if round_up {
        product = product.add(U512::from_u128(u128::from(u64::MAX)));
    }
    product.shr(64).to_u128().ok_or(SwapError::Overflow)
}

/// A step's amount in (rounded up) and out (rounded down) between two
/// sqrt prices at `liquidity` (the program's `get_delta_amounts_for_swap`).
fn delta_amounts_for_swap(
    sqrt_a: u128,
    sqrt_b: u128,
    liquidity: u128,
    zero_for_one: bool,
) -> Result<(u64, u64), SwapError> {
    let (lower, upper) = if sqrt_a > sqrt_b {
        (sqrt_b, sqrt_a)
    } else {
        (sqrt_a, sqrt_b)
    };
    if lower == 0 {
        return Err(SwapError::SqrtPriceOutOfRange);
    }
    // L·Δ√P is token 1's amount in Q64.64; over √Pa·√Pb, token 0's.
    let amount_1_x64 = U512::from_u128(liquidity).mul(U512::from_u128(upper - lower));
    let product = U512::from_u128(lower).mul(U512::from_u128(upper));
    let amount_0 = |round_up| {
        mul_div(amount_1_x64, U512::pow2(64), product, round_up, 256).ok_or(SwapError::Overflow)
    };
    let (amount_in, amount_out) = if zero_for_one {
        (amount_0(true)?, amount_1_x64.shr(64))
    } else {
        (
            amount_1_x64
                .add(U512::from_u128(u128::from(u64::MAX)))
                .shr(64),
            amount_0(false)?,
        )
    };
    let amount_in = amount_in.to_u64().ok_or(SwapError::MaxTokenOverflow)?;
    let amount_out = amount_out.to_u64().ok_or(SwapError::MaxTokenOverflow)?;
    Ok((amount_in, amount_out))
}

fn next_sqrt_price_from_input(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount_in: u64,
    zero_for_one: bool,
) -> Result<u128, SwapError> {
    check_price_and_liquidity(sqrt_price_x64, liquidity)?;
    if zero_for_one {
        next_sqrt_price_from_amount_0(sqrt_price_x64, liquidity, amount_in, true)
    } else {
        next_sqrt_price_from_amount_1(sqrt_price_x64, liquidity, amount_in, true)
    }
}

fn next_sqrt_price_from_output(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount_out: u64,
    zero_for_one: bool,
) -> Result<u128, SwapError> {
    check_price_and_liquidity(sqrt_price_x64, liquidity)?;
    if zero_for_one {
        next_sqrt_price_from_amount_1(sqrt_price_x64, liquidity, amount_out, false)
    } else {
        next_sqrt_price_from_amount_0(sqrt_price_x64, liquidity, amount_out, false)
    }
}

fn check_price_and_liquidity(sqrt_price_x64: u128, liquidity: u128) -> Result<(), SwapError> {
    if !(MIN_SQRT_PRICE_X64..MAX_SQRT_PRICE_X64).contains(&sqrt_price_x64) {
        return Err(SwapError::SqrtPriceOutOfRange);
    }
    if liquidity == 0 {
        return Err(SwapError::ZeroLiquidity);
    }
    Ok(())
}

/// The sqrt price after `amount` of token 0 goes in (`add`) or comes out:
/// `√P·L / (L ± Δx·√P)`, rounded up.
fn next_sqrt_price_from_amount_0(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount: u64,
    add: bool,
) -> Result<u128, SwapError> {
    if amount == 0 {
        return Ok(sqrt_price_x64);
    }
    let numerator = U512::from_u128(liquidity).shl(64);
    let product = U512::from_u128(u128::from(amount)).mul(U512::from_u128(sqrt_price_x64));
    // Both terms are under 2^193, so the program's fallback for a sum past
    // 256 bits never runs.
    let denominator = if add {
        numerator.add(product)
    } else if product > numerator {
        return Err(SwapError::Overflow);
    } else {
        numerator.sub(product)
    };
    mul_div(
        numerator,
        U512::from_u128(sqrt_price_x64),
        denominator,
        true,
        256,
    )
    .and_then(U512::to_u128)
    .ok_or(SwapError::Overflow)
}

/// The sqrt price after `amount` of token 1 goes in (`add`) or comes out:
/// `√P ± Δy / L`, rounded down.
fn next_sqrt_price_from_amount_1(
    sqrt_price_x64: u128,
    liquidity: u128,
    amount: u64,
    add: bool,
) -> Result<u128, SwapError> {
    if amount == 0 {
        return Ok(sqrt_price_x64);
    }
    let shifted = u128::from(amount) << 64;
    if add {
        sqrt_price_x64.checked_add(shifted / liquidity)
    } else {
        sqrt_price_x64.checked_sub(shifted.div_ceil(liquidity))
    }
    .ok_or(SwapError::Overflow)
}

/// `a·b/d` on u64, rounded down or up, if it fits.
fn mul_div_u64(a: u64, b: u64, d: u64, round_up: bool) -> Result<u64, SwapError> {
    if d == 0 {
        return Err(SwapError::Overflow);
    }
    let product = u128::from(a) * u128::from(b);
    let q = if round_up {
        product.div_ceil(u128::from(d))
    } else {
        product / u128::from(d)
    };
    u64::try_from(q).map_err(|_| SwapError::Overflow)
}

/// `a·b/d` rounded down or up, if it fits in `bits` bits: the program's
/// `mul_div_floor` and `mul_div_ceil` on its U128 (128) and U256 (256),
/// whose products it widens before dividing. `a` and `b` are under 2^256.
fn mul_div(a: U512, b: U512, d: U512, round_up: bool, bits: u32) -> Option<U512> {
    if d == U512::ZERO {
        return None;
    }
    let (mut q, r) = a.mul(b).div_rem(d);
    if round_up && r != U512::ZERO {
        q = q.add(U512::from_u128(1));
    }
    (q.bits() <= bits).then_some(q)
}

/// An unsigned 512-bit integer, its 64-bit limbs least significant first:
/// room for the program's 256-bit products.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct U512([u64; 8]);

impl U512 {
    const ZERO: Self = Self([0; 8]);

    fn from_u128(x: u128) -> Self {
        let mut limbs = [0; 8];
        limbs[0] = x as u64;
        limbs[1] = (x >> 64) as u64;
        Self(limbs)
    }

    /// 2^n, for n under 512.
    fn pow2(n: u32) -> Self {
        let mut limbs = [0; 8];
        limbs[n as usize / 64] = 1 << (n % 64);
        Self(limbs)
    }

    fn to_u128(self) -> Option<u128> {
        (self.bits() <= 128).then(|| u128::from(self.0[0]) | u128::from(self.0[1]) << 64)
    }

    fn to_u64(self) -> Option<u64> {
        (self.bits() <= 64).then_some(self.0[0])
    }

    /// The bits it takes: 0 for zero.
    fn bits(&self) -> u32 {
        (0..8)
            .rev()
            .find(|&i| self.0[i] != 0)
            .map_or(0, |i| 64 * i as u32 + 64 - self.0[i].leading_zeros())
    }

    fn bit(&self, n: u32) -> bool {
        self.0[n as usize / 64] >> (n % 64) & 1 == 1
    }

    /// The product, cut to 512 bits.
    fn mul(self, rhs: Self) -> Self {
        let mut out = [0u64; 8];
        for (i, &a) in self.0.iter().enumerate() {
            let mut carry = 0u128;
            for (j, &b) in rhs.0.iter().take(8 - i).enumerate() {
                let t = u128::from(a) * u128::from(b) + u128::from(out[i + j]) + carry;
                out[i + j] = t as u64;
                carry = t >> 64;
            }
        }
        Self(out)
    }

    /// The sum, wrapping at 512 bits.
    fn add(self, rhs: Self) -> Self {
        let mut out = [0u64; 8];
        let mut carry = false;
        for (o, (&a, &b)) in out.iter_mut().zip(self.0.iter().zip(&rhs.0)) {
            let (sum, c1) = a.overflowing_add(b);
            let (sum, c2) = sum.overflowing_add(u64::from(carry));
            *o = sum;
            carry = c1 || c2;
        }
        Self(out)
    }

    /// The difference, wrapping at 512 bits.
    fn sub(self, rhs: Self) -> Self {
        let mut out = [0u64; 8];
        let mut borrow = false;
        for (o, (&a, &b)) in out.iter_mut().zip(self.0.iter().zip(&rhs.0)) {
            let (diff, b1) = a.overflowing_sub(b);
            let (diff, b2) = diff.overflowing_sub(u64::from(borrow));
            *o = diff;
            borrow = b1 || b2;
        }
        Self(out)
    }

    fn shl(self, n: u32) -> Self {
        let (limbs, bits) = (n as usize / 64, n % 64);
        let mut out = [0u64; 8];
        for (i, o) in out.iter_mut().enumerate().skip(limbs) {
            *o = self.0[i - limbs] << bits;
            if bits > 0 && i > limbs {
                *o |= self.0[i - limbs - 1] >> (64 - bits);
            }
        }
        Self(out)
    }

    fn shr(self, n: u32) -> Self {
        let (limbs, bits) = (n as usize / 64, n % 64);
        let mut out = [0u64; 8];
        for (i, o) in out.iter_mut().enumerate().take(8 - limbs) {
            *o = self.0[i + limbs] >> bits;
            if bits > 0 && i + limbs + 1 < 8 {
                *o |= self.0[i + limbs + 1] << (64 - bits);
            }
        }
        Self(out)
    }

    /// The quotient and remainder, by long division; `d` is not zero and
    /// under 2^511.
    fn div_rem(self, d: Self) -> (Self, Self) {
        let mut q = Self::ZERO;
        let mut r = Self::ZERO;
        for n in (0..self.bits()).rev() {
            r = r.shl(1);
            r.0[0] |= u64::from(self.bit(n));
            if r >= d {
                r = r.sub(d);
                q.0[n as usize / 64] |= 1 << (n % 64);
            }
        }
        (q, r)
    }
}

impl Ord for U512 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.iter().rev().cmp(other.0.iter().rev())
    }
}

impl PartialOrd for U512 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_763_000_000;

    /// A pool at `tick` and `sqrt_price_x64` whose bitmap records the tick
    /// arrays starting at `starts`.
    fn pool(
        tick: i32,
        tick_spacing: u16,
        sqrt_price_x64: u128,
        liquidity: u128,
        starts: &[i32],
    ) -> PoolState {
        let mut tick_array_bitmap = [0u64; 16];
        for start in starts {
            let bit = (start / array_span(tick_spacing) + BITMAP_HALF) as usize;
            tick_array_bitmap[bit / 64] |= 1 << (bit % 64);
        }
        PoolState {
            address: Pubkey::new_from_array([7; 32]),
            amm_config: Pubkey::default(),
            mint_0: Pubkey::default(),
            mint_1: Pubkey::default(),
            vault_0: Pubkey::default(),
            vault_1: Pubkey::default(),
            observation: Pubkey::default(),
            decimals_0: 6,
            decimals_1: 6,
            tick_spacing,
            liquidity,
            sqrt_price_x64,
            tick_current: tick,
            fee_growth_global_0_x64: 0,
            fee_growth_global_1_x64: 0,
            status: 0,
            fee_on: 0,
            open_time: 0,
            dynamic_fee_info: None,
            tick_array_bitmap,
            rewards: Vec::new(),
        }
    }

    /// A tick with liquidity: `(tick, liquidity_gross, liquidity_net)`.
    fn tick(tick: i32, liquidity_gross: u128, liquidity_net: i128) -> Tick {
        Tick {
            tick,
            liquidity_gross,
            liquidity_net,
            ..Tick::default()
        }
    }

    fn array(pool: &PoolState, start: i32, ticks: &[Tick]) -> TickArray {
        let mut all = vec![Tick::default(); TICK_ARRAY_SIZE as usize];
        for t in ticks {
            all[((t.tick - start) / i32::from(pool.tick_spacing)) as usize] = *t;
        }
        TickArray {
            pool: pool.address,
            start_tick_index: start,
            ticks: all,
        }
    }

    fn config(trade_fee_rate: u32) -> AmmConfig {
        AmmConfig {
            trade_fee_rate,
            tick_spacing: 1,
        }
    }

    /// The pool and its tick arrays as a swap leaves them.
    fn after(pool: &PoolState, arrays: &mut [TickArray], run: &Run) -> PoolState {
        for walked in &run.arrays {
            if let Some(a) = arrays
                .iter_mut()
                .find(|a| a.start_tick_index == walked.start_tick_index)
            {
                *a = walked.clone();
            }
        }
        PoolState {
            sqrt_price_x64: run.sqrt_price_x64,
            tick_current: run.tick,
            liquidity: run.liquidity,
            ..pool.clone()
        }
    }

    fn unfilled_at(arrays: &[TickArray], at: i32) -> u64 {
        let t = arrays
            .iter()
            .flat_map(|a| &a.ticks)
            .find(|t| t.tick == at)
            .unwrap();
        unfilled(t).unwrap()
    }

    /// The live SPCX/USDC pool (2026-11-13 snapshot) near its price: its
    /// price, tick, liquidity, fee and the ticks from 48600 to 51000 with
    /// their liquidity_net (liquidity_gross stands in as its size).
    fn live() -> (PoolState, AmmConfig, Vec<TickArray>) {
        const TICKS_46800: [(i32, i128); 30] = [
            (48600, 38_666_264_977),
            (48660, 17_851_675_127),
            (48720, 17_342_339_267),
            (48780, 11_195_781_040),
            (48840, 190_724_593_668),
            (48900, 193_791_142_829),
            (48960, 30_012_235_613),
            (49020, 22_728_674_772),
            (49080, 711_110_617_752),
            (49140, 37_367_337_710),
            (49200, 25_196_025_687),
            (49260, 20_265_756_707),
            (49320, 118_652_978_606),
            (49380, 892_907_697_971),
            (49440, 566_479_975_242),
            (49500, -1_299_359_049_556),
            (49560, 187_407_238_641),
            (49620, -210_761_302_780),
            (49680, -294_576_534_263),
            (49740, -567_122_576_422),
            (49800, -197_542_089_807),
            (49860, -27_613_402_736),
            (49920, -44_325_576_443),
            (49980, -43_360_462_208),
            (50040, -90_161_693_842),
            (50100, -7_710_580_119),
            (50160, -168_212_678_711),
            (50220, -14_275_848_630),
            (50280, -222_257_024),
            (50340, -2_769_213_345),
        ];
        const TICKS_50400: [(i32, i128); 11] = [
            (50400, -22_882_541_842),
            (50460, -15_323_212_975),
            (50520, -9_425_429_954),
            (50580, 7_583_359_116),
            (50640, -10_928_854_934),
            (50700, -12_958_428_699),
            (50760, -6_973_229_503),
            (50820, -10_175_409_702),
            (50880, -29_115_202_713),
            (50940, -67_510_698_197),
            (51000, -17_397_023_727),
        ];
        let mut p = pool(
            49456,
            60,
            218_674_880_416_311_159_630,
            3_606_678_480_861,
            &[46800, 50400],
        );
        p.fee_on = 2;
        let ticks = |list: &[(i32, i128)]| -> Vec<Tick> {
            list.iter()
                .map(|&(t, net)| tick(t, net.unsigned_abs(), net))
                .collect()
        };
        let arrays = vec![
            array(&p, 46800, &ticks(&TICKS_46800)),
            array(&p, 50400, &ticks(&TICKS_50400)),
        ];
        let config = AmmConfig {
            trade_fee_rate: 2500,
            tick_spacing: 60,
        };
        (p, config, arrays)
    }

    #[test]
    fn tick_math_matches_the_program() {
        assert_eq!(sqrt_price_at_tick(MIN_TICK), Ok(MIN_SQRT_PRICE_X64));
        assert_eq!(sqrt_price_at_tick(MAX_TICK), Ok(MAX_SQRT_PRICE_X64));
        assert_eq!(sqrt_price_at_tick(0), Ok(1 << 64));
        assert_eq!(
            sqrt_price_at_tick(MAX_TICK + 1),
            Err(SwapError::TickOutOfRange)
        );
        assert_eq!(tick_at_sqrt_price(MIN_SQRT_PRICE_X64), Ok(MIN_TICK));
        assert_eq!(tick_at_sqrt_price(MAX_SQRT_PRICE_X64 - 1), Ok(MAX_TICK - 1));
        assert_eq!(
            tick_at_sqrt_price(MAX_SQRT_PRICE_X64),
            Err(SwapError::SqrtPriceOutOfRange)
        );

        // The source's tick_round_down.
        let at = sqrt_price_at_tick(-28861).unwrap();
        assert_eq!(tick_at_sqrt_price(at), Ok(-28861));
        assert_eq!(tick_at_sqrt_price(at + 1), Ok(-28861));
        assert_eq!(
            tick_at_sqrt_price(sqrt_price_at_tick(-28860).unwrap() - 1),
            Ok(-28861)
        );
        assert_eq!(tick_at_sqrt_price(at - 1), Ok(-28862));
        let at = sqrt_price_at_tick(28861).unwrap();
        assert_eq!(tick_at_sqrt_price(at), Ok(28861));
        assert_eq!(tick_at_sqrt_price(at + 1), Ok(28861));
        assert_eq!(
            tick_at_sqrt_price(sqrt_price_at_tick(28862).unwrap() - 1),
            Ok(28861)
        );
        assert_eq!(tick_at_sqrt_price(at - 1), Ok(28860));

        // The live pool's tick for its price, as the program stored it.
        assert_eq!(tick_at_sqrt_price(218_674_880_416_311_159_630), Ok(49456));

        // Every 97th tick round-trips, and the prices rise.
        let mut last = 0;
        for t in (MIN_TICK..MAX_TICK).step_by(97) {
            let sqrt = sqrt_price_at_tick(t).unwrap();
            assert!(sqrt > last);
            assert_eq!(tick_at_sqrt_price(sqrt), Ok(t));
            if t > MIN_TICK {
                assert_eq!(tick_at_sqrt_price(sqrt - 1), Ok(t - 1));
            }
            last = sqrt;
        }
    }

    #[test]
    fn wide_arithmetic_divides_exactly() {
        let a = U512::from_u128(u128::MAX);
        let (q, r) = a.mul(a).div_rem(U512::from_u128(u128::MAX - 1));
        assert_eq!(q, U512::from_u128(u128::MAX).add(U512::from_u128(1)));
        assert_eq!(r, U512::from_u128(1));
        assert_eq!(
            mul_div(a, a, U512::from_u128(1), false, 256),
            Some(a.mul(a))
        );
        assert_eq!(mul_div(a, a, U512::from_u128(1), false, 255), None);
        assert_eq!(
            mul_div(
                U512::from_u128(7),
                U512::from_u128(3),
                U512::from_u128(2),
                true,
                128
            ),
            Some(U512::from_u128(11))
        );
        assert_eq!(U512::pow2(300).shr(236).shl(10), U512::pow2(74));
        assert!(U512::pow2(129) > U512::from_u128(u128::MAX));
    }

    /// The source's `basic_fee_mode_tests`: every way a fee is charged, at
    /// tick 0 toward tick ±100.
    #[test]
    fn a_step_charges_its_fee_where_the_pool_says() {
        let current = sqrt_price_at_tick(0).unwrap();
        let (liquidity, fee_rate) = (1_000_000_000_000u128, 2500u32);
        for is_base_input in [true, false] {
            for is_fee_on_input in [true, false] {
                for zero_for_one in [true, false] {
                    let amount = if is_base_input { 1_000_000 } else { 500_000 };
                    let target = sqrt_price_at_tick(if zero_for_one { -100 } else { 100 }).unwrap();
                    let step = compute_swap_step(
                        current,
                        target,
                        liquidity,
                        amount,
                        fee_rate,
                        is_base_input,
                        zero_for_one,
                        is_fee_on_input,
                    )
                    .unwrap();
                    let on_input = mul_div_u64(step.amount_in, 2500, 997_500, true).unwrap();
                    let on_output = |gross: u64| mul_div_u64(gross, 2500, 1_000_000, true).unwrap();
                    match (is_base_input, is_fee_on_input) {
                        (true, true) => {
                            assert_eq!(step.amount_in + step.fee_amount, amount);
                            assert_eq!(step.fee_amount, on_input);
                        }
                        (true, false) => {
                            assert_eq!(step.amount_in, amount);
                            assert_eq!(
                                step.fee_amount,
                                on_output(step.amount_out + step.fee_amount)
                            );
                        }
                        (false, true) => {
                            assert_eq!(step.amount_out, amount);
                            assert_eq!(step.fee_amount, on_input);
                        }
                        (false, false) => {
                            assert_eq!(step.amount_out, amount);
                            assert_eq!(
                                step.fee_amount,
                                on_output(step.amount_out + step.fee_amount)
                            );
                        }
                    }
                    assert!(step.amount_in > 0 && step.amount_out > 0 && step.fee_amount > 0);
                }
            }
        }
    }

    /// The source's `compute_swap_step_test` invariants, at a few points:
    /// short of the target the step uses all it has; at it, no more. (Exact
    /// out with the fee on the output delivers the net amount asked.)
    #[test]
    fn a_step_uses_all_it_has_short_of_its_target() {
        let cases = [
            (
                sqrt_price_at_tick(1000).unwrap(),
                sqrt_price_at_tick(-2000).unwrap(),
                4_000_000_000u128,
                5_000_000u64,
            ),
            (
                sqrt_price_at_tick(-50_000).unwrap(),
                sqrt_price_at_tick(60_000).unwrap(),
                77u128,
                1_234_567u64,
            ),
            (
                sqrt_price_at_tick(200_000).unwrap(),
                sqrt_price_at_tick(199_000).unwrap(),
                3_000_000_000u128,
                u64::MAX / 2_000_000,
            ),
            (
                sqrt_price_at_tick(10).unwrap(),
                sqrt_price_at_tick(11).unwrap(),
                1u128 << 90,
                1u64,
            ),
        ];
        for (current, target, liquidity, amount) in cases {
            for is_base_input in [true, false] {
                for is_fee_on_input in [true, false] {
                    let zero_for_one = current > target;
                    let step = compute_swap_step(
                        current,
                        target,
                        liquidity,
                        amount,
                        3000,
                        is_base_input,
                        zero_for_one,
                        is_fee_on_input,
                    )
                    .unwrap();
                    let used = match (is_base_input, is_fee_on_input) {
                        (true, true) => step.amount_in + step.fee_amount,
                        (true, false) => step.amount_in,
                        (false, true) => step.amount_out,
                        (false, false) => step.amount_out,
                    };
                    if step.sqrt_price_next_x64 == target {
                        assert!(used <= amount);
                    } else {
                        assert_eq!(used, amount);
                    }
                    assert!(step.sqrt_price_next_x64 >= current.min(target));
                    assert!(step.sqrt_price_next_x64 <= current.max(target));
                }
            }
        }
    }

    /// The source's `exact_input_fee_from_output_dust_is_consumed_*`: with
    /// the fee on the output, a partial step's dust goes to the pool and
    /// the loop ends.
    #[test]
    fn dust_short_of_a_tick_goes_to_the_pool() {
        let liquidity = 100u128 << 64;
        let mut p = pool(0, 1, sqrt_price_at_tick(0).unwrap(), liquidity, &[0]);
        p.fee_on = 1;
        let arrays = [array(&p, 0, &[tick(59, 1, 0)])];
        let run = swap_internal(
            &p,
            &config(0),
            &arrays,
            150,
            MAX_SQRT_PRICE_X64 - 1,
            false,
            true,
            NOW,
        )
        .unwrap();
        assert_eq!(run.amount_1, 150);

        let mut p = pool(59, 1, sqrt_price_at_tick(59).unwrap(), liquidity, &[0]);
        p.fee_on = 2;
        let arrays = [array(&p, 0, &[tick(0, 1, 0)])];
        let run = swap_internal(
            &p,
            &config(0),
            &arrays,
            150,
            MIN_SQRT_PRICE_X64 + 1,
            true,
            true,
            NOW,
        )
        .unwrap();
        assert_eq!(run.amount_0, 150);
    }

    /// The source's limit order tests' pool: tick spacing 1 at tick 0, a
    /// trade fee of 1000, limit orders of 1,000,000 on ticks -10 (with
    /// liquidity) and 10 (without).
    fn limit_order_pool() -> (PoolState, Vec<TickArray>) {
        let p = pool(
            0,
            1,
            sqrt_price_at_tick(0).unwrap(),
            5_124_165_121_219,
            &[0, -60],
        );
        let mut at_minus_10 = tick(-10, 6_408_486_554, -6_408_486_554);
        at_minus_10.orders_amount = 1_000_000;
        let mut at_10 = tick(10, 0, 0);
        at_10.orders_amount = 1_000_000;
        let arrays = vec![
            array(&p, 0, &[at_10, tick(20, 790_917_615_645, 0)]),
            array(
                &p,
                -60,
                &[tick(-20, 1_330_680_689, -1_330_680_689), at_minus_10],
            ),
        ];
        (p, arrays)
    }

    /// The source's `test_zero_for_one_tick_assignment_with_limit_order`:
    /// swaps that reach a limit order's tick and stop leave it untouched.
    #[test]
    fn reaching_a_limit_order_tick_leaves_its_orders() {
        let (p, mut arrays) = limit_order_pool();
        let liquidity = p.liquidity;
        let run = swap_internal(
            &p,
            &config(1000),
            &arrays,
            2_565_160_190,
            sqrt_price_at_tick(-10).unwrap(),
            true,
            true,
            NOW,
        )
        .unwrap();
        let p = after(&p, &mut arrays, &run);
        assert_eq!(
            (p.tick_current, p.sqrt_price_x64, p.liquidity),
            (-10, sqrt_price_at_tick(-10).unwrap(), liquidity)
        );
        assert_eq!(unfilled_at(&arrays, -10), 1_000_000);
        assert_eq!(run.ticks_crossed, 0);

        let run = swap_internal(
            &p,
            &config(1000),
            &arrays,
            5_129_038_183,
            MAX_SQRT_PRICE_X64 - 1,
            false,
            true,
            NOW,
        )
        .unwrap();
        assert_eq!(run.amount_1, 5_129_038_183);
        let p = after(&p, &mut arrays, &run);
        assert_eq!(
            (p.tick_current, p.sqrt_price_x64, p.liquidity),
            (9, sqrt_price_at_tick(10).unwrap(), liquidity)
        );
        assert_eq!(unfilled_at(&arrays, 10), 1_000_000);

        let run = swap_internal(
            &p,
            &config(1000),
            &arrays,
            5_129_038_183,
            MIN_SQRT_PRICE_X64 + 1,
            true,
            true,
            NOW,
        )
        .unwrap();
        assert_eq!(run.amount_0, 5_129_038_183);
        let p = after(&p, &mut arrays, &run);
        assert_eq!(
            (p.tick_current, p.sqrt_price_x64, p.liquidity),
            (-10, sqrt_price_at_tick(-10).unwrap(), liquidity)
        );
        assert_eq!(unfilled_at(&arrays, -10), 1_000_000);
    }

    /// The source's `test_base_output_both_directions_partial_then_full_execution`:
    /// exact out, a limit order fills in part, then in full and the price
    /// crosses its tick, both ways.
    #[test]
    fn limit_orders_fill_in_part_then_in_full() {
        let (p, mut arrays) = limit_order_pool();
        let liquidity = p.liquidity;
        let config = config(1000);

        let run = swap_internal(
            &p,
            &config,
            &arrays,
            2_561_314_115 + 1000,
            MIN_SQRT_PRICE_X64 + 1,
            true,
            false,
            NOW,
        )
        .unwrap();
        assert_eq!(run.amount_1, 2_561_314_115 + 1000);
        let p = after(&p, &mut arrays, &run);
        assert_eq!(
            (p.tick_current, p.sqrt_price_x64, p.liquidity),
            (-10, sqrt_price_at_tick(-10).unwrap(), liquidity)
        );
        let unfilled = unfilled_at(&arrays, -10);
        assert_eq!(unfilled, 1_000_000 - 1000);

        let run = swap_internal(
            &p,
            &config,
            &arrays,
            unfilled,
            MIN_SQRT_PRICE_X64 + 1,
            true,
            false,
            NOW,
        )
        .unwrap();
        assert_eq!(run.amount_1, unfilled);
        assert_eq!(run.ticks_crossed, 1);
        let p = after(&p, &mut arrays, &run);
        assert_eq!(
            (p.tick_current, p.sqrt_price_x64, p.liquidity),
            (
                -11,
                sqrt_price_at_tick(-10).unwrap(),
                liquidity + 6_408_486_554
            )
        );

        let run = swap_internal(
            &p,
            &config,
            &arrays,
            5_123_909_143 + 1000,
            MAX_SQRT_PRICE_X64 - 1,
            false,
            false,
            NOW,
        )
        .unwrap();
        assert_eq!(run.amount_0, 5_123_909_143 + 1000);
        let p = after(&p, &mut arrays, &run);
        assert_eq!(
            (p.tick_current, p.sqrt_price_x64, p.liquidity),
            (9, sqrt_price_at_tick(10).unwrap(), liquidity)
        );
        let unfilled = unfilled_at(&arrays, 10);
        assert_eq!(unfilled, 1_000_000 - 1000);

        let run = swap_internal(
            &p,
            &config,
            &arrays,
            unfilled,
            MAX_SQRT_PRICE_X64 - 1,
            false,
            false,
            NOW,
        )
        .unwrap();
        assert_eq!(run.amount_0, unfilled);
        let p = after(&p, &mut arrays, &run);
        assert_eq!(
            (p.tick_current, p.sqrt_price_x64, p.liquidity),
            (10, sqrt_price_at_tick(10).unwrap(), liquidity)
        );
        assert_eq!(unfilled_at(&arrays, 10), 0);
    }

    /// A swap through a limit order's tick fills it, pays it the tick's
    /// price, and goes on past it.
    #[test]
    fn a_swap_through_a_limit_order_tick_fills_it() {
        let (p, arrays) = limit_order_pool();
        let without = {
            let (q, mut bare) = limit_order_pool();
            bare[1].ticks.iter_mut().for_each(|t| t.orders_amount = 0);
            swap(&q, &config(1000), &bare, true, 4_000_000_000, true, 0, NOW).unwrap()
        };
        let with = swap(
            &p,
            &config(1000),
            &arrays,
            true,
            4_000_000_000,
            true,
            0,
            NOW,
        )
        .unwrap();
        assert_eq!(with.amount_in, 4_000_000_000);
        assert_eq!(with.ticks_crossed, 1);
        assert_eq!(with.tick_arrays, [0, -60]);
        // The orders sell a million of token 1 at tick -10, which is more
        // than the curve gives there, and leave the price higher.
        assert!(with.amount_out > without.amount_out);
        assert!(with.amount_out - without.amount_out < 1_000_000);
        assert!(with.sqrt_price_x64 > without.sqrt_price_x64);
        assert!(with.tick < -10 && with.tick >= without.tick);
    }

    #[test]
    fn the_live_pool_quotes_both_ways() {
        let (p, config, arrays) = live();
        // Token 1 in: the fee comes off the input, in token 1.
        let up = swap(&p, &config, &arrays, false, 1_000_000_000_000, true, 0, NOW).unwrap();
        assert_eq!(up.amount_in, 1_000_000_000_000);
        assert!(!up.fee_in_token_0);
        // Each step rounds its fee up.
        let fee = mul_div_u64(1_000_000_000_000, 2500, 1_000_000, true).unwrap();
        assert!(up.fee >= fee && up.fee <= fee + u64::from(up.ticks_crossed) + 1);
        assert_eq!(up.tick_arrays, [46800, 50400]);
        assert!(up.tick > 50400 && up.ticks_crossed > 10);
        assert_eq!(tick_at_sqrt_price(up.sqrt_price_x64), Ok(up.tick));

        // Token 0 in: the fee comes off the output, still in token 1.
        let down = swap(&p, &config, &arrays, true, 2_000_000_000, true, 0, NOW).unwrap();
        assert!(!down.fee_in_token_0);
        assert_eq!(down.tick_arrays, [46800]);
        let gross = down.amount_out + down.fee;
        assert!(
            down.fee >= gross / 400 && down.fee <= gross / 400 + u64::from(down.ticks_crossed) + 1
        );

        // Exact out for what exact in gave costs no more, and nearly as much.
        for zero_for_one in [true, false] {
            let amount = if zero_for_one {
                2_000_000_000
            } else {
                300_000_000_000
            };
            let exact_in = swap(&p, &config, &arrays, zero_for_one, amount, true, 0, NOW).unwrap();
            let exact_out = swap(
                &p,
                &config,
                &arrays,
                zero_for_one,
                exact_in.amount_out,
                false,
                0,
                NOW,
            )
            .unwrap();
            assert_eq!(exact_out.amount_out, exact_in.amount_out);
            assert!(exact_out.amount_in <= exact_in.amount_in);
            assert!(exact_out.amount_in * 10_000 >= exact_in.amount_in * 9_999);
        }
    }

    #[test]
    fn a_dynamic_fee_charges_more_as_the_price_moves() {
        let (mut p, config, arrays) = live();
        let flat = swap(&p, &config, &arrays, false, 500_000_000_000, true, 0, NOW).unwrap();
        p.dynamic_fee_info = Some(DynamicFeeInfo {
            filter_period: 10,
            decay_period: 600,
            reduction_factor: 5000,
            dynamic_fee_control: 50_000,
            max_volatility_accumulator: 200_000,
            tick_spacing_index_reference: 0,
            volatility_reference: 0,
            volatility_accumulator: 0,
            last_update_timestamp: 0,
        });
        let dynamic = swap(&p, &config, &arrays, false, 500_000_000_000, true, 0, NOW).unwrap();
        assert_eq!(dynamic.amount_in, flat.amount_in);
        assert!(dynamic.fee > flat.fee);
        assert!(dynamic.amount_out < flat.amount_out);
        // The fee is capped at 10%.
        assert!(dynamic.fee <= dynamic.amount_in / 10);

        assert_eq!(tick_spacing_index(-30, 60), -1);
        assert_eq!(tick_spacing_index(-120, 60), -2);
        assert_eq!(tick_spacing_index(-121, 60), -3);
        assert_eq!(tick_spacing_index(121, 60), 2);
    }

    #[test]
    fn the_program_refuses_what_it_refuses() {
        let (p, config, arrays) = live();
        let quote = |p: &PoolState, arrays: &[TickArray], zero_for_one, amount, limit| {
            swap(p, &config, arrays, zero_for_one, amount, true, limit, NOW)
        };
        assert_eq!(quote(&p, &arrays, true, 0, 0), Err(SwapError::ZeroAmount));
        assert_eq!(quote(&p, &arrays, false, 1, 0), Err(SwapError::TooSmall));
        assert_eq!(
            quote(&p, &arrays, true, 1_000, p.sqrt_price_x64 + 1),
            Err(SwapError::PriceLimit)
        );
        assert_eq!(
            quote(&p, &arrays[..1], false, 1_000_000_000_000, 0),
            Err(SwapError::MissingTickArray(50400))
        );
        assert_eq!(
            quote(&p, &arrays, true, u64::MAX / 4, 0),
            Err(SwapError::LiquidityInsufficient)
        );
        let mut closed = p.clone();
        closed.status = 1 << 4;
        assert_eq!(
            quote(&closed, &arrays, true, 1_000, 0),
            Err(SwapError::NotApproved)
        );
        closed.status = 0;
        closed.open_time = NOW;
        assert_eq!(
            quote(&closed, &arrays, true, 1_000, 0),
            Err(SwapError::NotOpen)
        );

        // With a limit, a swap fills what it can up to it.
        let limit = sqrt_price_at_tick(49380).unwrap();
        let part = quote(&p, &arrays, true, 1_000_000_000_000, limit).unwrap();
        assert!(part.amount_in < 1_000_000_000_000);
        assert_eq!(part.sqrt_price_x64, limit);
        assert!(
            SwapError::PartialFill {
                filled: 1,
                wanted: 2
            }
            .to_string()
            .contains("1 of 2")
        );
    }
}
