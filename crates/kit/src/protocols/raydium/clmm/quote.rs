//! An exact swap quote: `swap_v2` and its `swap_internal`, step by step,
//! from `programs/amm/src/instructions/{swap.rs,swap_v2.rs}` and
//! `states/{tick_array.rs,pool_fee.rs}` at
//! `raydium-io/raydium-clmm@6d7a0f82d2f293c553c69aaf943377367c6c4cc6`.
//!
//! The loop walks initialized ticks in the swap's direction. Between two of
//! them liquidity is constant and [`compute_swap`] moves the price; at one,
//! limit orders resting there fill first, then the tick is crossed and its
//! `liquidity_net` added. A pool with a dynamic fee moves one tick spacing
//! at a time instead, its fee rising with how far the price has gone.
//!
//! The quote works on copies of the tick arrays, since limit orders filled
//! early in a swap change what later steps see, as on chain.

use solana_pubkey::Pubkey;

use super::{
    ClmmError, STEPS_MAX, TICK_ARRAYS_MAX,
    ix::tick_array_address,
    math::{self, FEE_RATE_DENOMINATOR, Q64, StepMode, SwapStep, compute_swap},
    state::{
        AmmConfig, ArrayNavigator, BitmapExtension, DynamicFee, Pool, PoolToken, Tick, TickArray,
        array_start_index, is_in_extension,
    },
};

/// Which way a swap trades.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapDirection {
    /// Sells token 0 for token 1: the price falls.
    ZeroForOne,
    /// Sells token 1 for token 0: the price rises.
    OneForZero,
}

impl SwapDirection {
    pub fn is_zero_for_one(self) -> bool {
        self == Self::ZeroForOne
    }
}

/// Which amount a swap fixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapMode {
    /// `amount` is what the trader pays.
    ExactIn,
    /// `amount` is what the trader receives.
    ExactOut,
}

/// A swap to quote, as `swap_v2` takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SwapRequest {
    pub direction: SwapDirection,
    pub mode: SwapMode,
    /// In atoms of the input token (exact in) or the output token (exact
    /// out).
    pub amount: u64,
    /// Where the swap stops, Q64.64; 0 for no limit, in which case
    /// `swap_v2` refuses a partial fill.
    pub sqrt_price_limit_x64: u128,
    /// The cluster's clock, in Unix seconds: the pool's open time and a
    /// dynamic fee's decay depend on it.
    pub block_timestamp_s: u64,
}

/// What a swap does, exactly as the program would do it on the same state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapQuote {
    /// What the trader pays, in atoms of the input token: the input
    /// vault's increase.
    pub amount_in: u64,
    /// What the trader receives, in atoms of the output token: the output
    /// vault's decrease.
    pub amount_out: u64,
    /// The trading fee (liquidity providers', protocol's and fund's), in
    /// atoms of `fee_token`.
    pub fee_amount: u64,
    pub fee_token: PoolToken,
    pub sqrt_price_x64_end: u128,
    pub tick_end: i32,
    pub liquidity_end: u128,
    /// Initialized ticks whose liquidity the swap crossed.
    pub ticks_crossed: u32,
    /// Atoms of the output token taken from limit orders, before any fee
    /// on the output.
    pub limit_order_amount_out: u64,
    /// The tick arrays the swap reads, in the order `swap_v2` needs them.
    pub tick_arrays: Vec<Pubkey>,
    /// Their start tick indexes, in the same order.
    pub tick_array_start_indexes: Vec<i32>,
    /// Whether the swap reads the pool's tick array bitmap extension.
    pub needs_bitmap_extension: bool,
}

/// Quotes `request` on `pool` (at `pool_address`) with its `config`, the
/// tick arrays the caller fetched, and the pool's bitmap extension if it
/// has one. Token-2022 transfer fees are not modelled: a quote on a mint
/// with a transfer fee is wrong by that fee.
pub fn quote(
    pool_address: &Pubkey,
    pool: &Pool,
    config: &AmmConfig,
    tick_arrays: &[TickArray],
    extension: Option<&BitmapExtension>,
    request: &SwapRequest,
) -> Result<SwapQuote, ClmmError> {
    if request.block_timestamp_s <= pool.open_time_s {
        return Err(ClmmError::PoolNotOpen {
            open_time_s: pool.open_time_s,
            block_timestamp_s: request.block_timestamp_s,
        });
    }
    let zero_for_one = request.direction.is_zero_for_one();
    let is_base_input = request.mode == SwapMode::ExactIn;
    let sqrt_price_limit_x64 = match (request.sqrt_price_limit_x64, zero_for_one) {
        (0, true) => math::MIN_SQRT_PRICE_X64 + 1,
        (0, false) => math::MAX_SQRT_PRICE_X64 - 1,
        (limit, _) => limit,
    };
    let mut swap = Swap::new(pool_address, pool, config, tick_arrays, extension, request)?;
    swap.run(sqrt_price_limit_x64)?;
    let result = swap.settle()?;
    let (amount_in, amount_out) = if zero_for_one {
        (result.amount_0, result.amount_1)
    } else {
        (result.amount_1, result.amount_0)
    };
    if amount_in == 0 || amount_out == 0 {
        return Err(ClmmError::TooSmallAmount);
    }
    if request.sqrt_price_limit_x64 == 0 {
        let filled = if is_base_input { amount_in } else { amount_out };
        if filled != request.amount {
            return Err(ClmmError::PartialFill {
                amount: request.amount,
                filled,
            });
        }
    }
    assert!(if zero_for_one {
        swap.sqrt_price_x64 <= pool.sqrt_price_x64
    } else {
        swap.sqrt_price_x64 >= pool.sqrt_price_x64
    });
    Ok(SwapQuote {
        amount_in,
        amount_out,
        fee_amount: result.fee_amount,
        fee_token: pool.fee_token(zero_for_one),
        sqrt_price_x64_end: swap.sqrt_price_x64,
        tick_end: swap.tick,
        liquidity_end: swap.liquidity,
        ticks_crossed: swap.ticks_crossed,
        limit_order_amount_out: swap.limit_order_amount_out,
        tick_arrays: swap
            .visited
            .iter()
            .map(|&start| tick_array_address(pool_address, start))
            .collect(),
        tick_array_start_indexes: swap.visited,
        needs_bitmap_extension: swap.navigator.used_extension,
    })
}

struct Settled {
    amount_0: u64,
    amount_1: u64,
    fee_amount: u64,
}

/// `SwapState` and what `swap_internal` keeps beside it.
struct Swap<'a> {
    pool: &'a Pool,
    config: &'a AmmConfig,
    navigator: ArrayNavigator<'a>,
    /// Working copies of the caller's tick arrays.
    arrays: Vec<TickArray>,
    /// Start indexes of the arrays the swap entered, in order.
    visited: Vec<i32>,
    zero_for_one: bool,
    is_base_input: bool,
    is_fee_on_input: bool,
    amount_specified: u64,
    amount_specified_remaining: u64,
    amount_calculated: u64,
    sqrt_price_x64: u128,
    tick: i32,
    liquidity: u128,
    lp_fee: u64,
    protocol_fee: u64,
    fund_fee: u64,
    sqrt_price_next_x64: u128,
    tick_next: i32,
    tick_spacing_index: i32,
    dynamic_fee: Option<DynamicFee>,
    ticks_crossed: u32,
    limit_order_amount_out: u64,
}

impl<'a> Swap<'a> {
    fn new(
        pool_address: &Pubkey,
        pool: &'a Pool,
        config: &'a AmmConfig,
        tick_arrays: &[TickArray],
        extension: Option<&'a BitmapExtension>,
        request: &SwapRequest,
    ) -> Result<Self, ClmmError> {
        if tick_arrays.iter().any(|array| array.pool != *pool_address) {
            return Err(ClmmError::InvalidTick("a tick array of another pool"));
        }
        if extension.is_some_and(|extension| extension.pool != *pool_address) {
            return Err(ClmmError::InvalidTick(
                "the bitmap extension of another pool",
            ));
        }
        if request.amount == 0 {
            return Err(ClmmError::ZeroAmount);
        }
        if !pool.is_swap_enabled() {
            return Err(ClmmError::SwapDisabled);
        }
        let zero_for_one = request.direction.is_zero_for_one();
        let mut swap = Self {
            pool,
            config,
            navigator: ArrayNavigator {
                pool,
                extension,
                used_extension: false,
            },
            arrays: tick_arrays.to_vec(),
            visited: Vec::new(),
            zero_for_one,
            is_base_input: request.mode == SwapMode::ExactIn,
            is_fee_on_input: pool.is_fee_on_input(zero_for_one),
            amount_specified: request.amount,
            amount_specified_remaining: request.amount,
            amount_calculated: 0,
            sqrt_price_x64: pool.sqrt_price_x64,
            tick: pool.tick_current,
            liquidity: pool.liquidity,
            lp_fee: 0,
            protocol_fee: 0,
            fund_fee: 0,
            sqrt_price_next_x64: 0,
            tick_next: 0,
            tick_spacing_index: 0,
            dynamic_fee: pool.dynamic_fee,
            ticks_crossed: 0,
            limit_order_amount_out: 0,
        };
        // The program reads the clock as a u32 (`oracle::block_timestamp`).
        let block_timestamp_s = u64::from(request.block_timestamp_s as u32);
        if let Some(dynamic_fee) = &mut swap.dynamic_fee {
            swap.tick_spacing_index = tick_spacing_index_from_tick(swap.tick, pool.tick_spacing);
            update_reference(dynamic_fee, swap.tick_spacing_index, block_timestamp_s);
        }
        Ok(swap)
    }

    fn enter_array(&mut self, start_tick_index: i32) -> Result<usize, ClmmError> {
        let index = self
            .arrays
            .iter()
            .position(|array| array.start_tick_index == start_tick_index)
            .ok_or(ClmmError::MissingTickArray { start_tick_index })?;
        if self.visited.len() >= TICK_ARRAYS_MAX {
            return Err(ClmmError::Limit("tick arrays a transaction can hold"));
        }
        self.visited.push(start_tick_index);
        Ok(index)
    }

    /// `swap_internal`'s loop.
    fn run(&mut self, sqrt_price_limit_x64: u128) -> Result<(), ClmmError> {
        let is_limit_valid = if self.zero_for_one {
            sqrt_price_limit_x64 < self.sqrt_price_x64
                && sqrt_price_limit_x64 > math::MIN_SQRT_PRICE_X64
        } else {
            sqrt_price_limit_x64 > self.sqrt_price_x64
                && sqrt_price_limit_x64 < math::MAX_SQRT_PRICE_X64
        };
        if !is_limit_valid {
            return Err(ClmmError::SqrtPriceLimit {
                sqrt_price_limit_x64,
            });
        }
        let (mut first_contains_pool_tick, first_start) =
            self.navigator.first(self.zero_for_one)?;
        let mut current_start = first_start;
        let mut current = self.enter_array(first_start)?;
        let mut steps: usize = 0;
        while self.amount_specified_remaining != 0 && self.sqrt_price_x64 != sqrt_price_limit_x64 {
            let tick_spacing = self.pool.tick_spacing;
            let offset = match self.arrays[current].next_initialized_offset(
                self.tick,
                tick_spacing,
                self.zero_for_one,
            ) {
                Some(offset) => offset,
                None if !first_contains_pool_tick => {
                    first_contains_pool_tick = true;
                    self.arrays[current].first_initialized_offset(self.zero_for_one)?
                }
                None => {
                    let next_start = self
                        .navigator
                        .next(current_start, self.zero_for_one)?
                        .ok_or(ClmmError::LiquidityInsufficient)?;
                    current = self.enter_array(next_start)?;
                    current_start = next_start;
                    self.arrays[current].first_initialized_offset(self.zero_for_one)?
                }
            };
            let mut next_tick = self.arrays[current].ticks[offset];
            assert!(next_tick.is_initialized());
            let target_price = self.target_price(next_tick.tick, sqrt_price_limit_x64)?;
            let liquidity_next =
                self.step_to_tick(&mut next_tick, current, target_price, &mut steps)?;
            self.liquidity = liquidity_next;
        }
        Ok(())
    }

    /// `get_target_price_based_on_next_tick`.
    fn target_price(
        &mut self,
        tick_next: i32,
        sqrt_price_limit_x64: u128,
    ) -> Result<u128, ClmmError> {
        self.tick_next = tick_next.clamp(math::MIN_TICK, math::MAX_TICK);
        self.sqrt_price_next_x64 = math::sqrt_price_at_tick(self.tick_next)?;
        let target = if (self.zero_for_one && self.sqrt_price_next_x64 < sqrt_price_limit_x64)
            || (!self.zero_for_one && self.sqrt_price_next_x64 > sqrt_price_limit_x64)
        {
            sqrt_price_limit_x64
        } else {
            self.sqrt_price_next_x64
        };
        let is_ordered = if self.zero_for_one {
            self.tick >= self.tick_next
                && self.sqrt_price_x64 >= self.sqrt_price_next_x64
                && self.sqrt_price_x64 >= target
        } else {
            self.tick_next > self.tick
                && self.sqrt_price_next_x64 >= self.sqrt_price_x64
                && target >= self.sqrt_price_x64
        };
        if !is_ordered {
            return Err(ClmmError::Math("next tick on the wrong side of the price"));
        }
        Ok(target)
    }

    /// The inner loop of `swap_internal`: moves toward `target_price`, at
    /// most to `next_tick`, filling its limit orders and crossing it if the
    /// price gets there. Returns the liquidity past it.
    fn step_to_tick(
        &mut self,
        next_tick: &mut Tick,
        current: usize,
        target_price: u128,
        steps: &mut usize,
    ) -> Result<u128, ClmmError> {
        let mut liquidity_next = self.liquidity;
        loop {
            *steps += 1;
            if *steps > STEPS_MAX {
                return Err(ClmmError::Limit("swap steps"));
            }
            self.update_volatility_accumulator();
            let total_fee_rate = self.total_fee_rate()?;
            let (is_skipped_tick_spacing, bounded_price, bounded_tick) =
                self.spacing_bounded_price(target_price)?;
            let step = if self.sqrt_price_x64 != bounded_price {
                let step = compute_swap(
                    self.sqrt_price_x64,
                    bounded_price,
                    self.liquidity,
                    self.amount_specified_remaining,
                    total_fee_rate,
                    StepMode {
                        is_base_input: self.is_base_input,
                        zero_for_one: self.zero_for_one,
                        is_fee_on_input: self.is_fee_on_input,
                    },
                )?;
                self.apply_amounts(step.amount_in, step.amount_out, step.fee_amount)?;
                step
            } else {
                SwapStep {
                    sqrt_price_next_x64: bounded_price,
                    ..SwapStep::default()
                }
            };
            let unfilled_before = next_tick.limit_order_unfilled_amount()?;
            if self.sqrt_price_next_x64 == step.sqrt_price_next_x64 {
                liquidity_next =
                    self.reach_tick(next_tick, current, total_fee_rate, liquidity_next)?;
            } else if self.sqrt_price_x64 != step.sqrt_price_next_x64 {
                self.tick = match bounded_tick {
                    Some(tick) if step.sqrt_price_next_x64 == bounded_price => tick,
                    _ => math::tick_at_sqrt_price(step.sqrt_price_next_x64)?,
                };
            }
            self.sqrt_price_x64 = step.sqrt_price_next_x64;
            self.update_dynamic_fee_index(is_skipped_tick_spacing);
            if self.amount_specified_remaining == 0 || self.sqrt_price_x64 == target_price {
                let unfilled_after = next_tick.limit_order_unfilled_amount()?;
                if self.amount_specified_remaining != 0
                    && unfilled_after != unfilled_before
                    && unfilled_after != 0
                {
                    return Err(ClmmError::InvalidLimitOrderAmount);
                }
                return Ok(liquidity_next);
            }
        }
    }

    /// The price reached `next_tick`: its limit orders fill, then its
    /// liquidity is crossed unless orders remain on it.
    fn reach_tick(
        &mut self,
        next_tick: &mut Tick,
        current: usize,
        total_fee_rate: u32,
        mut liquidity_next: u128,
    ) -> Result<u128, ClmmError> {
        let tick_spacing = self.pool.tick_spacing;
        let matched = match_limit_order(
            next_tick,
            self.amount_specified_remaining,
            self.zero_for_one,
            self.is_base_input,
            total_fee_rate,
            self.is_fee_on_input,
            self.sqrt_price_next_x64,
        )?;
        if matched.amount_in != 0 || matched.amount_out != 0 || matched.amm_fee_amount != 0 {
            self.limit_order_amount_out = self
                .limit_order_amount_out
                .checked_add(matched.gross_amount_out)
                .ok_or(ClmmError::Math("limit order output"))?;
            self.apply_amounts(
                matched.amount_in,
                matched.amount_out,
                matched.amm_fee_amount,
            )?;
        }
        if !next_tick.is_initialized() {
            // The tick emptied: the program counts it out of its array, and
            // flips the array's bit when none is left, which needs the
            // extension for an array beyond the pool's own bitmap.
            let array = &mut self.arrays[current];
            array.initialized_tick_count = array
                .initialized_tick_count
                .checked_sub(1)
                .ok_or(ClmmError::Math("initialized tick count"))?;
            if array.initialized_tick_count == 0
                && is_in_extension(array.start_tick_index, tick_spacing)
            {
                self.navigator.used_extension = true;
                if self.navigator.extension.is_none() {
                    return Err(ClmmError::MissingBitmapExtension);
                }
            }
        }
        if next_tick.has_liquidity() && !next_tick.has_limit_orders() {
            let liquidity_net = if self.zero_for_one {
                next_tick
                    .liquidity_net
                    .checked_neg()
                    .ok_or(ClmmError::Math("liquidity net"))?
            } else {
                next_tick.liquidity_net
            };
            liquidity_next = math::add_liquidity_delta(self.liquidity, liquidity_net)?;
            self.ticks_crossed += 1;
        }
        let array = &mut self.arrays[current];
        let offset = array.offset_of(next_tick.tick, tick_spacing)?;
        array.ticks[offset] = *next_tick;
        self.tick = if (self.zero_for_one && !next_tick.has_limit_orders())
            || (!self.zero_for_one && next_tick.has_limit_orders())
        {
            self.tick_next - 1
        } else {
            self.tick_next
        };
        Ok(liquidity_next)
    }

    /// `apply_swap_amounts` and `spilt_fees`.
    fn apply_amounts(
        &mut self,
        amount_in: u64,
        amount_out: u64,
        fee_amount: u64,
    ) -> Result<(), ClmmError> {
        let overflow = ClmmError::Math("swap amounts");
        let amount_in_consumed = if self.is_fee_on_input {
            amount_in.checked_add(fee_amount).ok_or(overflow.clone())?
        } else {
            amount_in
        };
        let (spent, gained) = if self.is_base_input {
            (amount_in_consumed, amount_out)
        } else {
            (amount_out, amount_in_consumed)
        };
        self.amount_specified_remaining = self
            .amount_specified_remaining
            .checked_sub(spent)
            .ok_or(overflow.clone())?;
        self.amount_calculated = self
            .amount_calculated
            .checked_add(gained)
            .ok_or(overflow.clone())?;
        let share = |rate: u32| {
            (u128::from(fee_amount) * u128::from(rate) / u128::from(FEE_RATE_DENOMINATOR)) as u64
        };
        let protocol_fee = share(self.config.protocol_fee_rate);
        let fund_fee = share(self.config.fund_fee_rate);
        self.protocol_fee = self
            .protocol_fee
            .checked_add(protocol_fee)
            .ok_or(overflow.clone())?;
        self.fund_fee = self
            .fund_fee
            .checked_add(fund_fee)
            .ok_or(overflow.clone())?;
        let remaining_fee = fee_amount
            .checked_sub(protocol_fee)
            .and_then(|fee| fee.checked_sub(fund_fee))
            .ok_or(overflow.clone())?;
        if self.liquidity > 0 {
            self.lp_fee = self.lp_fee.checked_add(remaining_fee).ok_or(overflow)?;
        } else {
            self.protocol_fee = self
                .protocol_fee
                .checked_add(remaining_fee)
                .ok_or(overflow)?;
        }
        Ok(())
    }

    /// `settle_amounts`.
    fn settle(&self) -> Result<Settled, ClmmError> {
        let overflow = ClmmError::Math("settle");
        let consumed = self
            .amount_specified
            .checked_sub(self.amount_specified_remaining)
            .ok_or(overflow.clone())?;
        let (amount_0, amount_1) = if self.zero_for_one == self.is_base_input {
            (consumed, self.amount_calculated)
        } else {
            (self.amount_calculated, consumed)
        };
        let fee_amount = self
            .lp_fee
            .checked_add(self.protocol_fee)
            .and_then(|fee| fee.checked_add(self.fund_fee))
            .ok_or(overflow)?;
        Ok(Settled {
            amount_0,
            amount_1,
            fee_amount,
        })
    }

    fn update_volatility_accumulator(&mut self) {
        if let Some(dynamic_fee) = &mut self.dynamic_fee {
            update_volatility_accumulator(dynamic_fee, self.tick_spacing_index);
        }
    }

    /// `get_total_fee_rate`.
    fn total_fee_rate(&self) -> Result<u32, ClmmError> {
        let base = self.config.trade_fee_rate;
        match &self.dynamic_fee {
            None => Ok(base),
            Some(dynamic_fee) => {
                let dynamic = dynamic_fee_rate(dynamic_fee, self.pool.tick_spacing)?;
                Ok(base.saturating_add(dynamic).min(MAX_FEE_RATE))
            }
        }
    }

    /// `get_spacing_bounded_price`: with a dynamic fee, a step goes at most
    /// to the end of the current tick spacing group.
    fn spacing_bounded_price(
        &self,
        target_price: u128,
    ) -> Result<(bool, u128, Option<i32>), ClmmError> {
        let Some(dynamic_fee) = &self.dynamic_fee else {
            return Ok((true, target_price, None));
        };
        if self.liquidity == 0
            || dynamic_fee.volatility_accumulator == dynamic_fee.max_volatility_accumulator
        {
            return Ok((true, target_price, None));
        }
        let tick_spacing = i32::from(self.pool.tick_spacing);
        let bounded_tick = if self.zero_for_one {
            self.tick_spacing_index.saturating_mul(tick_spacing)
        } else {
            self.tick_spacing_index
                .saturating_add(1)
                .saturating_mul(tick_spacing)
        };
        let bounded_tick = bounded_tick.clamp(math::MIN_TICK, math::MAX_TICK);
        let bounded_price = math::sqrt_price_at_tick(bounded_tick)?;
        let is_target_nearer = if self.zero_for_one {
            target_price > bounded_price
        } else {
            target_price < bounded_price
        };
        if is_target_nearer {
            Ok((false, target_price, None))
        } else {
            Ok((false, bounded_price, Some(bounded_tick)))
        }
    }

    /// `update_dynamic_fee_index`.
    fn update_dynamic_fee_index(&mut self, is_skipped_tick_spacing: bool) {
        if self.dynamic_fee.is_none() {
            return;
        }
        if is_skipped_tick_spacing {
            let tick = if self.sqrt_price_x64 == self.sqrt_price_next_x64 {
                self.tick_next
            } else {
                self.tick
            };
            let mut index = tick_spacing_index_from_tick(tick, self.pool.tick_spacing);
            if !self.zero_for_one && tick % i32::from(self.pool.tick_spacing) == 0 {
                index -= 1;
            }
            self.tick_spacing_index = index;
            if let Some(dynamic_fee) = &mut self.dynamic_fee
                && dynamic_fee.volatility_accumulator != dynamic_fee.max_volatility_accumulator
            {
                update_volatility_accumulator(dynamic_fee, self.tick_spacing_index);
            }
        }
        self.tick_spacing_index += if self.zero_for_one { -1 } else { 1 };
    }
}

/// `MAX_FEE_RATE_NUMERATOR`: a dynamic fee is capped at 10%.
const MAX_FEE_RATE: u32 = 100_000;
/// `VOLATILITY_ACCUMULATOR_SCALE`.
const VOLATILITY_ACCUMULATOR_SCALE: u64 = 10_000;
/// `REDUCTION_FACTOR_DENOMINATOR`.
const REDUCTION_FACTOR_DENOMINATOR: u64 = 10_000;
/// `DYNAMIC_FEE_CONTROL_DENOMINATOR`.
const DYNAMIC_FEE_CONTROL_DENOMINATOR: u128 = 100_000;

/// `tick_spacing_index_from_tick`: the tick spacing group a tick is in.
fn tick_spacing_index_from_tick(tick: i32, tick_spacing: u16) -> i32 {
    tick.div_euclid(i32::from(tick_spacing))
}

/// `DynamicFeeInfo::update_reference`.
fn update_reference(dynamic_fee: &mut DynamicFee, tick_spacing_index: i32, timestamp_s: u64) {
    let elapsed_s = timestamp_s.saturating_sub(dynamic_fee.last_update_timestamp_s);
    if elapsed_s < u64::from(dynamic_fee.filter_period_s) {
        return;
    }
    dynamic_fee.tick_spacing_index_reference = tick_spacing_index;
    dynamic_fee.last_update_timestamp_s = timestamp_s;
    dynamic_fee.volatility_reference = if elapsed_s < u64::from(dynamic_fee.decay_period_s) {
        (u64::from(dynamic_fee.volatility_accumulator) * u64::from(dynamic_fee.reduction_factor)
            / REDUCTION_FACTOR_DENOMINATOR) as u32
    } else {
        0
    };
}

/// `DynamicFeeInfo::update_volatility_accumulator`.
fn update_volatility_accumulator(dynamic_fee: &mut DynamicFee, tick_spacing_index: i32) {
    let index_delta =
        (dynamic_fee.tick_spacing_index_reference - tick_spacing_index).unsigned_abs();
    let accumulator = u64::from(dynamic_fee.volatility_reference)
        + u64::from(index_delta) * VOLATILITY_ACCUMULATOR_SCALE;
    dynamic_fee.volatility_accumulator =
        accumulator.min(u64::from(dynamic_fee.max_volatility_accumulator)) as u32;
}

/// `compute_dynamic_fee_rate`: quadratic in the volatility accumulator.
fn dynamic_fee_rate(dynamic_fee: &DynamicFee, tick_spacing: u16) -> Result<u32, ClmmError> {
    let crossed = dynamic_fee
        .volatility_accumulator
        .checked_mul(u32::from(tick_spacing))
        .ok_or(ClmmError::Math("dynamic fee volatility"))?;
    let squared = u128::from(crossed) * u128::from(crossed);
    let denominator = DYNAMIC_FEE_CONTROL_DENOMINATOR
        * u128::from(VOLATILITY_ACCUMULATOR_SCALE)
        * u128::from(VOLATILITY_ACCUMULATOR_SCALE);
    let fee_rate = math::mul_div_ceil_u128(
        u128::from(dynamic_fee.dynamic_fee_control),
        squared,
        denominator,
    )
    .ok_or(ClmmError::Math("dynamic fee rate"))?;
    Ok(fee_rate.min(u128::from(MAX_FEE_RATE)) as u32)
}

/// `LimitOrderMatchResult`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct LimitOrderMatch {
    amount_in: u64,
    amount_out: u64,
    amm_fee_amount: u64,
    /// What the orders gave up, before a fee on the output.
    gross_amount_out: u64,
}

/// `get_limit_order_output_with_price`, which rounds down.
fn limit_order_output(
    amount_in: u64,
    token_0_price_x64: u128,
    zero_for_one: bool,
) -> Result<u128, ClmmError> {
    let (multiplier, denominator) = if zero_for_one {
        (token_0_price_x64, Q64)
    } else {
        (Q64, token_0_price_x64)
    };
    math::mul_div_floor_u128(u128::from(amount_in), multiplier, denominator)
        .ok_or(ClmmError::Math("limit order output"))
}

/// `get_limit_order_input_with_price`, which rounds up.
fn limit_order_input(
    amount_out: u64,
    token_0_price_x64: u128,
    zero_for_one: bool,
) -> Result<u64, ClmmError> {
    let (multiplier, denominator) = if zero_for_one {
        (token_0_price_x64, Q64)
    } else {
        (Q64, token_0_price_x64)
    };
    math::mul_div_ceil_u128(u128::from(amount_out), multiplier, denominator)
        .and_then(|amount| u64::try_from(amount).ok())
        .ok_or(ClmmError::Math("limit order input"))
}

/// `match_limit_order_with_sqrt_price`: fills the limit orders resting on
/// `tick` at its price, oldest cohort first. The order phase and unfilled
/// ratio it also updates only matter to the orders' owners, so they are
/// left out.
fn match_limit_order(
    tick: &mut Tick,
    swap_amount: u64,
    zero_for_one: bool,
    is_base_input: bool,
    fee_rate: u32,
    is_fee_on_input: bool,
    sqrt_price_x64: u128,
) -> Result<LimitOrderMatch, ClmmError> {
    let mut result = LimitOrderMatch::default();
    let total_unfilled = tick.limit_order_unfilled_amount()?;
    if swap_amount == 0 || total_unfilled == 0 {
        return Ok(result);
    }
    let overflow = ClmmError::Math("limit order fee");
    let fee_rate_u64 = u64::from(fee_rate);
    let fee_denominator = u64::from(FEE_RATE_DENOMINATOR);
    let fee_complement = u64::from(FEE_RATE_DENOMINATOR - fee_rate);
    let token_0_price_x64 = math::price_from_sqrt_price(sqrt_price_x64, !zero_for_one)?;
    if is_base_input {
        if is_fee_on_input {
            result.amm_fee_amount =
                math::mul_div_ceil_u64(swap_amount, fee_rate_u64, fee_denominator)
                    .ok_or(overflow.clone())?;
            result.amount_in = swap_amount - result.amm_fee_amount;
        } else {
            result.amount_in = swap_amount;
        }
        let matched_output = limit_order_output(result.amount_in, token_0_price_x64, zero_for_one)?;
        if matched_output > u128::from(total_unfilled) {
            result.amount_out = total_unfilled;
            result.amount_in = limit_order_input(total_unfilled, token_0_price_x64, !zero_for_one)?;
            if is_fee_on_input {
                result.amm_fee_amount =
                    math::mul_div_ceil_u64(result.amount_in, fee_rate_u64, fee_complement)
                        .ok_or(overflow.clone())?;
            }
        } else {
            result.amount_out = matched_output as u64;
        }
    } else {
        let net_output = swap_amount.min(total_unfilled);
        result.amount_out = if is_fee_on_input {
            net_output
        } else {
            math::mul_div_ceil_u64(net_output, fee_denominator, fee_complement)
                .ok_or(overflow.clone())?
                .min(total_unfilled)
        };
        result.amount_in = limit_order_input(result.amount_out, token_0_price_x64, !zero_for_one)?;
        if is_fee_on_input {
            result.amm_fee_amount =
                math::mul_div_ceil_u64(result.amount_in, fee_rate_u64, fee_complement)
                    .ok_or(overflow.clone())?;
        }
    }
    consume_limit_orders(tick, result.amount_out)?;
    result.gross_amount_out = result.amount_out;
    if !is_fee_on_input {
        result.amm_fee_amount =
            math::mul_div_ceil_u64(result.amount_out, fee_rate_u64, fee_denominator)
                .ok_or(overflow.clone())?;
        result.amount_out = result
            .amount_out
            .checked_sub(result.amm_fee_amount)
            .ok_or(overflow)?;
    }
    Ok(result)
}

/// The order book half of `match_limit_order_with_sqrt_price`: the partly
/// filled cohort first, then the untouched orders, which become the new
/// partly filled cohort.
fn consume_limit_orders(tick: &mut Tick, amount_out: u64) -> Result<(), ClmmError> {
    let from_part_filled = tick.part_filled_orders_remaining.min(amount_out);
    tick.part_filled_orders_remaining -= from_part_filled;
    let from_orders = amount_out - from_part_filled;
    if from_orders > 0 {
        if tick.part_filled_orders_remaining != 0 {
            return Err(ClmmError::InvalidLimitOrderAmount);
        }
        if tick.orders_amount < from_orders {
            return Err(ClmmError::InvalidLimitOrderAmount);
        }
        tick.part_filled_orders_remaining = tick.orders_amount - from_orders;
        tick.orders_amount = 0;
    }
    Ok(())
}

/// The tick arrays a swap from the pool's current price in `direction`
/// would enter, in order, up to `count_max` of them: what to fetch before
/// quoting. The extension is needed past the pool's own bitmap.
pub fn tick_arrays_in_direction(
    pool: &Pool,
    extension: Option<&BitmapExtension>,
    direction: SwapDirection,
    count_max: usize,
) -> Result<Vec<i32>, ClmmError> {
    assert!(count_max <= TICK_ARRAYS_MAX);
    let zero_for_one = direction.is_zero_for_one();
    let mut navigator = ArrayNavigator {
        pool,
        extension,
        used_extension: false,
    };
    let mut starts = Vec::with_capacity(count_max);
    let (_, mut start) = navigator.first(zero_for_one)?;
    while starts.len() < count_max {
        starts.push(start);
        match navigator.next(start, zero_for_one)? {
            Some(next) => start = next,
            None => break,
        }
    }
    debug_assert!(
        starts
            .iter()
            .all(|&start| start == array_start_index(start, pool.tick_spacing))
    );
    Ok(starts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocols::raydium::clmm::state::{FeeOn, TICK_ARRAY_SIZE};

    const POOL_ADDRESS: Pubkey = Pubkey::new_from_array([7; 32]);

    fn pool(tick_current: i32, tick_spacing: u16, liquidity: u128) -> Pool {
        Pool {
            amm_config: Pubkey::default(),
            token_mint_0: Pubkey::default(),
            token_mint_1: Pubkey::default(),
            token_vault_0: Pubkey::default(),
            token_vault_1: Pubkey::default(),
            observation: Pubkey::default(),
            mint_decimals_0: 9,
            mint_decimals_1: 6,
            tick_spacing,
            liquidity,
            sqrt_price_x64: math::sqrt_price_at_tick(tick_current).unwrap(),
            tick_current,
            status: 0,
            fee_on: FeeOn::Input,
            tick_array_bitmap: [0; 16],
            open_time_s: 0,
            dynamic_fee: None,
        }
    }

    fn tick(tick: i32, liquidity_gross: u128, liquidity_net: i128, orders_amount: u64) -> Tick {
        Tick {
            tick,
            liquidity_net,
            liquidity_gross,
            orders_amount,
            part_filled_orders_remaining: 0,
        }
    }

    /// `build_swap_param`: arrays of the given ticks, flipped on in the
    /// pool's bitmap.
    fn arrays(pool: &mut Pool, layout: &[(i32, Vec<Tick>)]) -> Vec<TickArray> {
        layout
            .iter()
            .map(|(start, ticks)| {
                let mut array = TickArray {
                    pool: POOL_ADDRESS,
                    start_tick_index: *start,
                    ticks: [Tick::default(); TICK_ARRAY_SIZE],
                    initialized_tick_count: ticks.len() as u8,
                };
                for (offset, tick) in array.ticks.iter_mut().enumerate() {
                    tick.tick = start + offset as i32 * i32::from(pool.tick_spacing);
                }
                for tick in ticks {
                    let offset = array.offset_of(tick.tick, pool.tick_spacing).unwrap();
                    array.ticks[offset] = *tick;
                }
                let bit = (start / pool.ticks_per_array() + 512) as usize;
                pool.tick_array_bitmap[bit / 64] |= 1 << (bit % 64);
                array
            })
            .collect()
    }

    fn config() -> AmmConfig {
        AmmConfig {
            trade_fee_rate: 1000,
            protocol_fee_rate: 0,
            fund_fee_rate: 0,
            tick_spacing: 1,
        }
    }

    fn swap_request(
        direction: SwapDirection,
        mode: SwapMode,
        amount: u64,
        sqrt_price_limit_x64: u128,
    ) -> SwapRequest {
        SwapRequest {
            direction,
            mode,
            amount,
            sqrt_price_limit_x64,
            block_timestamp_s: 1,
        }
    }

    /// The layout of `swap.rs`'s limit order tests: limit orders of
    /// 1,000,000 on ticks -10 and 10 around tick 0.
    fn limit_order_pool() -> (Pool, Vec<TickArray>) {
        let mut pool = pool(0, 1, 5_124_165_121_219);
        let arrays = arrays(
            &mut pool,
            &[
                (
                    0,
                    vec![tick(10, 0, 0, 1_000_000), tick(20, 790_917_615_645, 0, 0)],
                ),
                (
                    -60,
                    vec![
                        tick(-20, 1_330_680_689, -1_330_680_689, 0),
                        tick(-10, 6_408_486_554, -6_408_486_554, 1_000_000),
                    ],
                ),
            ],
        );
        (pool, arrays)
    }

    /// `test_tick_assignment_during_oscillation_without_execution`'s
    /// first two swaps, each from the starting state: exactly the amount
    /// that reaches the limit order's tick, which fills none of it.
    #[test]
    fn a_swap_that_reaches_a_limit_orders_tick_stops_there() {
        let (pool, arrays) = limit_order_pool();
        let limit = math::sqrt_price_at_tick(-10).unwrap();
        let request = swap_request(
            SwapDirection::ZeroForOne,
            SwapMode::ExactIn,
            2_565_160_190,
            limit,
        );
        let quote = quote(&POOL_ADDRESS, &pool, &config(), &arrays, None, &request).unwrap();
        assert_eq!(quote.amount_in, 2_565_160_190);
        assert_eq!(quote.tick_end, -10);
        assert_eq!(quote.sqrt_price_x64_end, limit);
        assert_eq!(quote.liquidity_end, pool.liquidity);
        assert_eq!(quote.limit_order_amount_out, 0);
        assert_eq!(quote.tick_array_start_indexes, vec![0, -60]);

        // Upward, the order on tick 10 sells token 0: what the swap has left
        // at that tick fills all of it, and the emptied tick is passed.
        let limit = math::sqrt_price_at_tick(10).unwrap();
        let request = swap_request(
            SwapDirection::OneForZero,
            SwapMode::ExactIn,
            5_129_038_183,
            limit,
        );
        let quote = super::quote(&POOL_ADDRESS, &pool, &config(), &arrays, None, &request).unwrap();
        assert_eq!(quote.limit_order_amount_out, 1_000_000);
        assert_eq!(quote.tick_end, 10);
        assert_eq!(quote.sqrt_price_x64_end, limit);
        assert!(quote.amount_in < 5_129_038_183);
    }

    /// `test_tick_assignment_after_partial_execution_and_oscillation`'s
    /// first swap: 1,000 atoms of the order on tick -10 fill.
    #[test]
    fn a_limit_order_fills_at_its_tick_before_liquidity_is_crossed() {
        let (pool, arrays) = limit_order_pool();
        let mut order = arrays[1].ticks[50];
        assert_eq!(order.tick, -10);
        let price = math::sqrt_price_at_tick(-10).unwrap();
        let expected = match_limit_order(&mut order, 1000, true, false, 1000, true, price).unwrap();
        let expected_amount_in = expected.amount_in + expected.amm_fee_amount;
        let request = swap_request(
            SwapDirection::ZeroForOne,
            SwapMode::ExactIn,
            2_565_160_190 + expected_amount_in,
            price,
        );
        let quote = quote(&POOL_ADDRESS, &pool, &config(), &arrays, None, &request).unwrap();
        assert_eq!(quote.amount_in, 2_565_160_190 + expected_amount_in);
        assert_eq!(quote.tick_end, -10);
        assert_eq!(quote.sqrt_price_x64_end, price);
        assert_eq!(quote.liquidity_end, pool.liquidity);
        assert_eq!(quote.limit_order_amount_out, 1000);
        assert_eq!(quote.ticks_crossed, 0);
    }

    #[test]
    fn crossing_ticks_adds_their_net_liquidity() {
        let (pool, mut arrays) = limit_order_pool();
        for array in &mut arrays {
            for tick in array.ticks.iter_mut() {
                tick.orders_amount = 0;
            }
        }
        arrays[0].ticks[10].liquidity_gross = 0;
        // The program finds the next initialized tick before it applies the
        // price limit, so one must exist below the limit at tick -30.
        arrays[1].ticks[20].liquidity_gross = 1;
        arrays[1].initialized_tick_count = 3;
        let request = swap_request(
            SwapDirection::ZeroForOne,
            SwapMode::ExactIn,
            10_000_000_000,
            math::sqrt_price_at_tick(-30).unwrap(),
        );
        let quote = quote(&POOL_ADDRESS, &pool, &config(), &arrays, None, &request).unwrap();
        assert_eq!(quote.ticks_crossed, 2);
        // Going down, a tick's net liquidity is subtracted.
        assert_eq!(
            quote.liquidity_end,
            pool.liquidity + 6_408_486_554 + 1_330_680_689
        );
        assert_eq!(quote.tick_end, -30);
        // Exact out of what exact in produced costs no more than it did.
        let back = super::quote(
            &POOL_ADDRESS,
            &pool,
            &config(),
            &arrays,
            None,
            &SwapRequest {
                mode: SwapMode::ExactOut,
                amount: quote.amount_out,
                sqrt_price_limit_x64: 0,
                ..request
            },
        )
        .unwrap();
        assert_eq!(back.amount_out, quote.amount_out);
        assert!(back.amount_in <= quote.amount_in);
    }

    #[test]
    fn a_swap_past_the_supplied_arrays_names_the_one_it_needs() {
        let (pool, arrays) = limit_order_pool();
        let request = swap_request(
            SwapDirection::ZeroForOne,
            SwapMode::ExactIn,
            10_000_000_000,
            0,
        );
        assert_eq!(
            quote(
                &POOL_ADDRESS,
                &pool,
                &config(),
                &arrays[..1],
                None,
                &request
            ),
            Err(ClmmError::MissingTickArray {
                start_tick_index: -60
            })
        );
        // Past -60 there is no array at all. With a tick spacing of 1 the
        // pool's bitmap doesn't span every tick, so only the extension can
        // say so.
        let request = SwapRequest {
            amount: u64::MAX / 2,
            ..request
        };
        assert_eq!(
            quote(&POOL_ADDRESS, &pool, &config(), &arrays, None, &request),
            Err(ClmmError::MissingBitmapExtension)
        );
        let empty = BitmapExtension {
            pool: POOL_ADDRESS,
            positive: [[0; 8]; 14],
            negative: [[0; 8]; 14],
        };
        assert_eq!(
            quote(
                &POOL_ADDRESS,
                &pool,
                &config(),
                &arrays,
                Some(&empty),
                &request
            ),
            Err(ClmmError::LiquidityInsufficient)
        );
    }

    #[test]
    fn refusals_mirror_the_program() {
        let (mut pool, arrays) = limit_order_pool();
        let base = swap_request(SwapDirection::ZeroForOne, SwapMode::ExactIn, 1_000, 0);
        let config = config();
        assert_eq!(
            quote(
                &POOL_ADDRESS,
                &pool,
                &config,
                &arrays,
                None,
                &SwapRequest { amount: 0, ..base }
            ),
            Err(ClmmError::ZeroAmount)
        );
        assert!(matches!(
            quote(
                &POOL_ADDRESS,
                &pool,
                &config,
                &arrays,
                None,
                &SwapRequest {
                    sqrt_price_limit_x64: pool.sqrt_price_x64 + 1,
                    ..base
                }
            ),
            Err(ClmmError::SqrtPriceLimit { .. })
        ));
        pool.open_time_s = 5;
        assert!(matches!(
            quote(&POOL_ADDRESS, &pool, &config, &arrays, None, &base),
            Err(ClmmError::PoolNotOpen { .. })
        ));
        pool.open_time_s = 0;
        pool.status = 1 << 4;
        assert_eq!(
            quote(&POOL_ADDRESS, &pool, &config, &arrays, None, &base),
            Err(ClmmError::SwapDisabled)
        );
    }

    #[test]
    fn dynamic_fees_rise_with_volatility() {
        let dynamic_fee = DynamicFee {
            filter_period_s: 10,
            decay_period_s: 60,
            reduction_factor: 5000,
            dynamic_fee_control: 50_000,
            max_volatility_accumulator: 200_000,
            ..DynamicFee::default()
        };
        assert_eq!(dynamic_fee_rate(&dynamic_fee, 1), Ok(0));
        let mut moved = dynamic_fee;
        update_volatility_accumulator(&mut moved, 5);
        assert_eq!(moved.volatility_accumulator, 50_000);
        // ceil(50,000 * 50,000^2 / 10^13) = 13 millionths.
        assert_eq!(dynamic_fee_rate(&moved, 1), Ok(13));
        update_volatility_accumulator(&mut moved, 1_000);
        assert_eq!(moved.volatility_accumulator, 200_000);
        // Within the filter period nothing moves; within the decay period
        // the reference halves; after it, it resets.
        let mut reference = moved;
        update_reference(&mut reference, 3, 5);
        assert_eq!(reference, moved);
        update_reference(&mut reference, 3, 30);
        assert_eq!(
            (
                reference.volatility_reference,
                reference.tick_spacing_index_reference
            ),
            (100_000, 3)
        );
        update_reference(&mut reference, 4, 1_000);
        assert_eq!(reference.volatility_reference, 0);
        assert_eq!(tick_spacing_index_from_tick(-1, 10), -1);
        assert_eq!(tick_spacing_index_from_tick(-10, 10), -1);
        assert_eq!(tick_spacing_index_from_tick(9, 10), 0);
    }
}
