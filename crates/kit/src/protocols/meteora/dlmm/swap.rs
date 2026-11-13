//! A swap worked out exactly as the program executes `swap2` (exact in) and
//! `swap_exact_out2` (exact out), integer for integer: the variable fee as the
//! clock and the bins crossed move it, each bin's Q64.64 price, its
//! market-making reserves and then its limit orders, the fee mode, and the
//! rounding at every step.
//!
//! It follows the quote in Meteora's `dlmm-sdk` (`commons`, release 0.12.0),
//! which mirrors the deployed program: the one with limit orders and
//! `collect_fee_mode`. What it leaves out: Token-2022 transfer fees (the
//! named pools' mints charge none), and bin arrays past the pool's own bitmap
//! (±512 arrays around index 0), which only its bitmap extension records.

use std::fmt;

use super::{BINS_PER_ARRAY, Bin, BinArray, LbPair, bin_array_index};

/// The most a swap's fee can be, in billionths: 10%.
pub const MAX_FEE_RATE: u128 = 100_000_000;
const FEE_PRECISION: u128 = 1_000_000_000;
const BASIS_POINT_MAX: u128 = 10_000;
/// The share of a limit order's fee its owner keeps, in basis points; the
/// protocol takes the rest.
const LIMIT_ORDER_FEE_SHARE: u128 = 5_000;
/// The bins any pool's prices reach, at a 1 bp step.
const MIN_BIN_ID: i32 = -443_636;
const MAX_BIN_ID: i32 = 443_636;
/// The bin arrays the pool's own bitmap records: -512 to 511.
const BITMAP_HALF: i64 = 512;
/// Past this exponent a Q64.64 price overflows.
const MAX_EXPONENT: u32 = 0x80000;
const ONE: u128 = 1 << 64;

/// A token of the pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Token {
    X,
    Y,
}

/// What a swap does, as the program executes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Swap {
    /// Atoms of the token in that leave the payer, fee included.
    pub amount_in: u64,
    /// Atoms of the token out the payer receives, fee excluded.
    pub amount_out: u64,
    /// The fee, all of it, in atoms of `fee_token`: the token in, or Y
    /// whichever way the swap goes when the pool's `collect_fee_mode` is 1.
    /// `amount_in` already includes it, or `amount_out` already lacks it.
    pub fee: u64,
    pub fee_token: Token,
    /// The protocol's part of `fee`, which the pool keeps apart from its
    /// liquidity; it changes nothing the payer sees.
    pub protocol_fee: u64,
    /// The active bin after the swap, and how many bins it moved.
    pub active_id: i32,
    pub bins_crossed: u32,
    /// The pool's volatility accumulator after the swap.
    pub volatility_accumulator: u32,
    /// The bin arrays the swap walks, in the order its instruction names
    /// them: the active bin's first.
    pub bin_arrays: Vec<i64>,
}

/// Why a swap can't be worked out, or would fail on chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwapError {
    /// The pool's status disables swaps.
    Disabled,
    /// The pool's activation slot or time is still ahead.
    NotActivated {
        activation_point: u64,
    },
    /// A bin array the swap reaches isn't among those given.
    BinArrayNotLoaded(i64),
    /// The swap starts in a bin array only the pool's bitmap extension
    /// records.
    BeyondBitmap(i64),
    /// The bins the pool's bitmap records can't fill it.
    InsufficientLiquidity,
    ZeroAmount,
    Overflow,
}

impl fmt::Display for SwapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Disabled => write!(f, "the pool is disabled"),
            Self::NotActivated { activation_point } => {
                write!(f, "the pool activates at {activation_point}")
            }
            Self::BinArrayNotLoaded(index) => write!(f, "bin array {index} isn't loaded"),
            Self::BeyondBitmap(index) => {
                write!(f, "bin array {index} lies past the pool's own bitmap")
            }
            Self::InsufficientLiquidity => write!(f, "the pool hasn't the liquidity"),
            Self::ZeroAmount => write!(f, "the amount is zero"),
            Self::Overflow => write!(f, "the swap's math overflows"),
        }
    }
}

impl std::error::Error for SwapError {}

/// The swap the program would execute on `pair`: X in (`swap_for_y`) or Y
/// in, `amount` atoms in (`exact_in`) or out, at the clock's
/// `unix_timestamp` and `slot`, through the bins of `bin_arrays` (any order;
/// those the swap doesn't reach may be missing).
pub fn swap(
    pair: &LbPair,
    bin_arrays: &[BinArray],
    swap_for_y: bool,
    amount: u64,
    exact_in: bool,
    unix_timestamp: i64,
    slot: u64,
) -> Result<Swap, SwapError> {
    if amount == 0 {
        return Err(SwapError::ZeroAmount);
    }
    check_active(pair, unix_timestamp, slot)?;
    let mut volatility = Volatility::at_start(pair, unix_timestamp)?;
    let fees = Fees {
        on_input: fee_on_input(pair, swap_for_y),
        limit_orders: supports_limit_orders(pair),
        protocol_share: pair.protocol_share,
        rate: 0,
    };
    let mut active_id = pair.active_id;
    // What is left to send in, or to take out.
    let mut left = amount;
    let (mut amount_in, mut amount_out, mut fee, mut protocol_fee) = (0u64, 0u64, 0u64, 0u64);
    let mut walked = Vec::new();
    while left > 0 {
        let index = next_array_with_liquidity(pair, bin_array_index(active_id), swap_for_y)?;
        let array = bin_arrays
            .iter()
            .find(|a| a.index == index)
            .ok_or(SwapError::BinArrayNotLoaded(index))?;
        walked.push(index);
        // Bin arrays without liquidity are skipped: the swap resumes at the
        // near edge of the next one.
        if bin_array_index(active_id) != index {
            let lower = i32::try_from(index * i64::from(BINS_PER_ARRAY))
                .map_err(|_| SwapError::Overflow)?;
            active_id = if swap_for_y {
                lower + BINS_PER_ARRAY - 1
            } else {
                lower
            };
        }
        while left > 0 {
            let Some(bin) = array.bin(active_id) else {
                break;
            };
            if max_amount_out(bin, swap_for_y, fees.limit_orders) > 0 {
                let rate = fee_rate(pair, volatility.accumulate(pair, active_id)?)?;
                let fees = Fees { rate, ..fees };
                let price = bin_price(bin, active_id, pair.bin_step)?;
                let quote = if exact_in {
                    exact_in_at_bin(bin, price, &fees, left, swap_for_y)?
                } else {
                    exact_out_at_bin(bin, price, &fees, left, swap_for_y)?
                };
                let filled = if exact_in {
                    quote.amount_in
                } else {
                    quote.amount_out
                };
                if filled > 0 {
                    left = left.checked_sub(filled).ok_or(SwapError::Overflow)?;
                    amount_in = add(amount_in, quote.amount_in)?;
                    amount_out = add(amount_out, quote.amount_out)?;
                    fee = add(fee, quote.fee)?;
                    protocol_fee = add(protocol_fee, quote.protocol_fee)?;
                }
            }
            if left > 0 {
                active_id = if swap_for_y {
                    active_id - 1
                } else {
                    active_id + 1
                };
                if !(MIN_BIN_ID..=MAX_BIN_ID).contains(&active_id) {
                    return Err(SwapError::InsufficientLiquidity);
                }
            }
        }
    }
    let fee_token = match (fees.on_input, swap_for_y) {
        (true, true) => Token::X,
        _ => Token::Y,
    };
    Ok(Swap {
        amount_in,
        amount_out,
        fee,
        fee_token,
        protocol_fee,
        active_id,
        bins_crossed: active_id.abs_diff(pair.active_id),
        volatility_accumulator: volatility.accumulator,
        bin_arrays: walked,
    })
}

/// The fee, in billionths, at `volatility_accumulator`: the base fee plus
/// the variable fee, capped at [`MAX_FEE_RATE`].
pub fn fee_rate(pair: &LbPair, volatility_accumulator: u32) -> Result<u128, SwapError> {
    let base = u128::from(pair.base_factor)
        .checked_mul(u128::from(pair.bin_step) * 10)
        .and_then(|b| b.checked_mul(10u128.checked_pow(u32::from(pair.base_fee_power_factor))?))
        .ok_or(SwapError::Overflow)?;
    let variable = if pair.variable_fee_control > 0 {
        let square = (u128::from(volatility_accumulator) * u128::from(pair.bin_step))
            .checked_pow(2)
            .ok_or(SwapError::Overflow)?;
        u128::from(pair.variable_fee_control)
            .checked_mul(square)
            .and_then(|v| v.checked_add(99_999_999_999))
            .ok_or(SwapError::Overflow)?
            / 100_000_000_000
    } else {
        0
    };
    Ok(base
        .checked_add(variable)
        .ok_or(SwapError::Overflow)?
        .min(MAX_FEE_RATE))
}

/// Bin `id`'s price, Y atoms per X atom in Q64.64: `(1 + bin_step / 10⁴)^id`
/// by the program's own squaring, rounding as it rounds. `None` past the
/// prices Q64.64 holds.
pub fn price_from_id(id: i32, bin_step: u16) -> Option<u128> {
    let base = ONE + (u128::from(bin_step) << 64) / BASIS_POINT_MAX;
    pow(base, id)
}

/// `base^exp` in Q64.64. Squaring a base above one would overflow, so it
/// squares the base's inverse and inverts the result.
fn pow(base: u128, exp: i32) -> Option<u128> {
    if exp == 0 {
        return Some(ONE);
    }
    let mut invert = exp < 0;
    let exp = exp.unsigned_abs();
    if exp >= MAX_EXPONENT {
        return None;
    }
    let mut squared = base;
    if squared >= ONE {
        squared = u128::MAX.checked_div(squared)?;
        invert = !invert;
    }
    let mut result = ONE;
    for bit in 0..19 {
        if bit > 0 {
            squared = squared.checked_mul(squared)? >> 64;
        }
        if exp & (1 << bit) != 0 {
            result = result.checked_mul(squared)? >> 64;
        }
    }
    if result == 0 {
        return None;
    }
    if invert {
        result = u128::MAX.checked_div(result)?;
    }
    Some(result)
}

/// The fee a swap of `amount` atoms, fee excluded, adds on top: rounded up.
fn fee_on_amount(amount: u64, rate: u128) -> Result<u64, SwapError> {
    let denominator = FEE_PRECISION - rate;
    to_u64((u128::from(amount) * rate).div_ceil(denominator))
}

/// The fee within `amount` atoms, fee included: rounded up.
fn fee_within_amount(amount: u64, rate: u128) -> Result<u64, SwapError> {
    to_u64((u128::from(amount) * rate).div_ceil(FEE_PRECISION))
}

/// Whether the pool may be swapped now: enabled, and, for the pool types
/// that have one, past its activation point.
fn check_active(pair: &LbPair, unix_timestamp: i64, slot: u64) -> Result<(), SwapError> {
    if pair.status != 0 {
        return Err(SwapError::Disabled);
    }
    // Permissioned and customizable permissionless pools.
    if matches!(pair.pair_type, 1 | 2) {
        let now = match pair.activation_type {
            0 => slot,
            1 => unix_timestamp as u64,
            _ => return Err(SwapError::Disabled),
        };
        if now < pair.activation_point {
            return Err(SwapError::NotActivated {
                activation_point: pair.activation_point,
            });
        }
    }
    Ok(())
}

/// Whether swaps fill the bins' limit orders: pools for limit orders, and
/// undetermined pools without rewards.
fn supports_limit_orders(pair: &LbPair) -> bool {
    match pair.function_type {
        0 => pair.reward_mints.iter().all(|m| *m == Default::default()),
        2 => true,
        _ => false,
    }
}

/// Whether the fee comes off the token in; otherwise off the token out. A
/// pool collecting fees only in Y charges a swap of X in on what it pays
/// out.
fn fee_on_input(pair: &LbPair, swap_for_y: bool) -> bool {
    pair.collect_fee_mode != 1 || !swap_for_y
}

/// The pool's variable-fee state through a swap.
struct Volatility {
    accumulator: u32,
    reference: u32,
    index_reference: i32,
}

impl Volatility {
    /// The state a swap at `now` starts from: past the filter period the
    /// reference moves to the active bin and decays the accumulator, and
    /// past the decay period it's gone.
    fn at_start(pair: &LbPair, now: i64) -> Result<Self, SwapError> {
        let mut state = Self {
            accumulator: pair.volatility_accumulator,
            reference: pair.volatility_reference,
            index_reference: pair.index_reference,
        };
        let elapsed = now
            .checked_sub(pair.last_update_timestamp)
            .ok_or(SwapError::Overflow)?;
        if elapsed >= i64::from(pair.filter_period) {
            state.index_reference = pair.active_id;
            state.reference = if elapsed < i64::from(pair.decay_period) {
                state
                    .accumulator
                    .checked_mul(u32::from(pair.reduction_factor))
                    .ok_or(SwapError::Overflow)?
                    / BASIS_POINT_MAX as u32
            } else {
                0
            };
        }
        Ok(state)
    }

    /// The accumulator with the swap at bin `active_id`: the reference plus
    /// 10⁴ for each bin from the reference's, capped.
    fn accumulate(&mut self, pair: &LbPair, active_id: i32) -> Result<u32, SwapError> {
        let bins = u64::from(self.index_reference.abs_diff(active_id));
        let accumulator = (u64::from(self.reference) + bins * BASIS_POINT_MAX as u64)
            .min(u64::from(pair.max_volatility_accumulator));
        self.accumulator = u32::try_from(accumulator).map_err(|_| SwapError::Overflow)?;
        Ok(self.accumulator)
    }
}

/// The bin array a swap continues into from array `start`: the first with
/// liquidity, by the pool's bitmap, from `start` on in its direction.
fn next_array_with_liquidity(
    pair: &LbPair,
    start: i64,
    swap_for_y: bool,
) -> Result<i64, SwapError> {
    if !(-BITMAP_HALF..BITMAP_HALF).contains(&start) {
        return Err(SwapError::BeyondBitmap(start));
    }
    let has_liquidity = |index: &i64| pair.bin_array_has_liquidity(*index) == Some(true);
    let found = if swap_for_y {
        (-BITMAP_HALF..=start).rev().find(has_liquidity)
    } else {
        (start..BITMAP_HALF).find(has_liquidity)
    };
    found.ok_or(SwapError::InsufficientLiquidity)
}

/// The fee terms one bin's swap is charged at.
#[derive(Clone, Copy)]
struct Fees {
    rate: u128,
    on_input: bool,
    limit_orders: bool,
    protocol_share: u16,
}

/// One bin's part of a swap.
struct BinQuote {
    /// Fee included when it's charged on the way in.
    amount_in: u64,
    /// Fee excluded when it's charged on the way out.
    amount_out: u64,
    fee: u64,
    protocol_fee: u64,
}

/// What a bin's limit orders on the swap's side hold: its open orders, and
/// what's left of its orders a swap has begun. Bids buy X, so a swap of X
/// in fills them; asks, a swap of Y in.
fn limit_orders(bin: &Bin, swap_for_y: bool) -> (u64, u64) {
    if swap_for_y != bin.limit_order_ask_side {
        (bin.open_order_amount, bin.processed_order_remaining_amount)
    } else {
        (0, 0)
    }
}

/// The most a bin pays out: its reserves of the token out, and its limit
/// orders where the pool fills them.
fn max_amount_out(bin: &Bin, swap_for_y: bool, with_limit_orders: bool) -> u64 {
    let reserves = if swap_for_y {
        bin.amount_y
    } else {
        bin.amount_x
    };
    if !with_limit_orders {
        return reserves;
    }
    let (open, processed) = limit_orders(bin, swap_for_y);
    reserves.saturating_add(open).saturating_add(processed)
}

/// The bin's stored price, or its id's where none is stored yet.
fn bin_price(bin: &Bin, id: i32, bin_step: u16) -> Result<u128, SwapError> {
    if bin.price != 0 {
        return Ok(bin.price);
    }
    price_from_id(id, bin_step).ok_or(SwapError::Overflow)
}

/// One layer of a bin (its reserves, or a kind of its orders) filled from
/// `amount` in, fee excluded.
struct Fill {
    amount_in: u64,
    amount_left: u64,
    amount_out: u64,
}

fn fill(amount: u64, available: u64, price: u128, swap_for_y: bool) -> Result<Fill, SwapError> {
    if available == 0 {
        return Ok(Fill {
            amount_in: 0,
            amount_left: amount,
            amount_out: 0,
        });
    }
    let all_in = amount_in(available, price, swap_for_y, true)?;
    if amount >= all_in {
        Ok(Fill {
            amount_in: all_in,
            amount_left: amount - all_in,
            amount_out: available,
        })
    } else {
        Ok(Fill {
            amount_in: amount,
            amount_left: 0,
            amount_out: amount_out(amount, price, swap_for_y, false)?,
        })
    }
}

/// A bin filled from `amount` in, fee excluded: its reserves first, then
/// what's left of begun orders, then open orders. Also what the reserves
/// took, which earns their fee share.
fn fill_bin(
    bin: &Bin,
    price: u128,
    amount: u64,
    swap_for_y: bool,
    with_limit_orders: bool,
) -> Result<(Fill, u64), SwapError> {
    let reserves = if swap_for_y {
        bin.amount_y
    } else {
        bin.amount_x
    };
    let mm = fill(amount, reserves, price, swap_for_y)?;
    if !with_limit_orders || mm.amount_left == 0 {
        let mm_in = mm.amount_in;
        return Ok((mm, mm_in));
    }
    let (open, processed) = limit_orders(bin, swap_for_y);
    let begun = fill(mm.amount_left, processed, price, swap_for_y)?;
    let mut total_in = add(mm.amount_in, begun.amount_in)?;
    let mut total_out = add(mm.amount_out, begun.amount_out)?;
    if begun.amount_left > 0 {
        let new = fill(begun.amount_left, open, price, swap_for_y)?;
        total_in = add(total_in, new.amount_in)?;
        total_out = add(total_out, new.amount_out)?;
    }
    Ok((
        Fill {
            amount_in: total_in,
            amount_left: amount - total_in,
            amount_out: total_out,
        },
        mm.amount_in,
    ))
}

/// The protocol's part of a bin's fee: its share of what the reserves
/// earned (rounded down from the reserves' part, which rounds up), and
/// the half of the orders' part their owners don't keep.
fn protocol_fee(
    fee: u64,
    protocol_share: u16,
    mm_in: u64,
    total_in: u64,
) -> Result<u64, SwapError> {
    if total_in == 0 || fee == 0 {
        return Ok(0);
    }
    let mm_fee = to_u64((u128::from(fee) * u128::from(mm_in)).div_ceil(u128::from(total_in)))?;
    let orders_fee = fee.checked_sub(mm_fee).ok_or(SwapError::Overflow)?;
    let owners = to_u64(u128::from(orders_fee) * LIMIT_ORDER_FEE_SHARE / BASIS_POINT_MAX)?;
    let mm_protocol = to_u64(u128::from(mm_fee) * u128::from(protocol_share) / BASIS_POINT_MAX)?;
    add(orders_fee - owners, mm_protocol)
}

/// A bin swapped from `amount` in, fee included where it's charged on the
/// way in. Whatever the bin can't take comes back as less `amount_in`.
fn exact_in_at_bin(
    bin: &Bin,
    price: u128,
    fees: &Fees,
    amount: u64,
    swap_for_y: bool,
) -> Result<BinQuote, SwapError> {
    let mut fee = 0;
    let mut net_in = amount;
    if fees.on_input {
        fee = fee_within_amount(amount, fees.rate)?;
        net_in = amount.checked_sub(fee).ok_or(SwapError::Overflow)?;
    }
    let (filled, mm_in) = fill_bin(bin, price, net_in, swap_for_y, fees.limit_orders)?;
    let mut gross_in = amount;
    if filled.amount_left > 0 {
        // The bin empties: the fee is charged on what it took.
        net_in -= filled.amount_left;
        gross_in = net_in;
        if fees.on_input {
            fee = fee_on_amount(net_in, fees.rate)?;
            gross_in = add(net_in, fee)?;
        }
    }
    let mut out = filled.amount_out;
    if !fees.on_input {
        fee = fee_within_amount(out, fees.rate)?;
        out -= fee;
    }
    Ok(BinQuote {
        amount_in: gross_in,
        amount_out: out,
        fee,
        protocol_fee: protocol_fee(fee, fees.protocol_share, mm_in, filled.amount_in)?,
    })
}

/// A bin swapped for `amount` out, fee excluded where it's charged on the
/// way out; or emptied, where it holds no more.
fn exact_out_at_bin(
    bin: &Bin,
    price: u128,
    fees: &Fees,
    amount: u64,
    swap_for_y: bool,
) -> Result<BinQuote, SwapError> {
    let mut gross_out = amount;
    if !fees.on_input {
        gross_out = add(amount, fee_on_amount(amount, fees.rate)?)?;
    }
    if gross_out >= max_amount_out(bin, swap_for_y, fees.limit_orders) {
        return exact_in_at_bin(bin, price, fees, u64::MAX, swap_for_y);
    }
    // What each layer needs in for its part of the amount out, rounded up.
    let reserves = if swap_for_y {
        bin.amount_y
    } else {
        bin.amount_x
    };
    let (open, processed) = limit_orders(bin, swap_for_y);
    let mut remaining = gross_out;
    let mut net_in = 0u64;
    for available in [reserves, processed, open] {
        let part = remaining.min(available);
        net_in = add(net_in, amount_in(part, price, swap_for_y, true)?)?;
        remaining -= part;
        if remaining == 0 {
            break;
        }
    }
    let gross_in = if fees.on_input {
        add(net_in, fee_on_amount(net_in, fees.rate)?)?
    } else {
        net_in
    };
    let mut quote = exact_in_at_bin(bin, price, fees, gross_in, swap_for_y)?;
    // Rounding up may pay out more than asked: the payer gets the amount,
    // and the protocol what's over by more than an atom.
    if quote.amount_out > amount {
        let over = quote.amount_out - amount;
        if over > 1 {
            quote.protocol_fee = add(quote.protocol_fee, over)?;
        }
    }
    quote.amount_out = amount;
    Ok(quote)
}

/// What `amount_out` atoms out of a bin at `price` cost in: X for Y
/// (`swap_for_y`) is `out / price`, Y for X `out × price`.
fn amount_in(
    amount_out: u64,
    price: u128,
    swap_for_y: bool,
    round_up: bool,
) -> Result<u64, SwapError> {
    if swap_for_y {
        shl_div(amount_out, price, round_up)
    } else {
        mul_shr(amount_out, price, round_up)
    }
}

/// What `amount_in` atoms into a bin at `price` pay out.
fn amount_out(
    amount_in: u64,
    price: u128,
    swap_for_y: bool,
    round_up: bool,
) -> Result<u64, SwapError> {
    if swap_for_y {
        mul_shr(amount_in, price, round_up)
    } else {
        shl_div(amount_in, price, round_up)
    }
}

/// `x × y >> 64`, as a u64. The product runs to 192 bits: the high half of
/// `y` times `x` and the low half times `x` are each 128.
fn mul_shr(x: u64, y: u128, round_up: bool) -> Result<u64, SwapError> {
    let low = (y & u128::from(u64::MAX)) * u128::from(x);
    let high = (y >> 64) * u128::from(x);
    let mut result = high.checked_add(low >> 64).ok_or(SwapError::Overflow)?;
    if round_up && low as u64 != 0 {
        result = result.checked_add(1).ok_or(SwapError::Overflow)?;
    }
    to_u64(result)
}

/// `(x << 64) / y`, as a u64.
fn shl_div(x: u64, y: u128, round_up: bool) -> Result<u64, SwapError> {
    if y == 0 {
        return Err(SwapError::Overflow);
    }
    let shifted = u128::from(x) << 64;
    let result = if round_up {
        shifted.div_ceil(y)
    } else {
        shifted / y
    };
    to_u64(result)
}

fn to_u64(value: u128) -> Result<u64, SwapError> {
    u64::try_from(value).map_err(|_| SwapError::Overflow)
}

fn add(a: u64, b: u64) -> Result<u64, SwapError> {
    a.checked_add(b).ok_or(SwapError::Overflow)
}

#[cfg(test)]
mod tests {
    use base64::Engine as _;
    use solana_pubkey::Pubkey;

    use super::*;
    use crate::protocols::{SPCX_MINT, USDC_MINT, meteora::dlmm::state::testing::pair_bytes};

    /// The live SPCX/USDC pool (`7nVQtYQi…`) on 13 November 2026.
    const LIVE_PAIR: &str = "IQsxYrVlsQ0QJywBsASIE0wdAADwSQIAsO3//1ASAADoAwACAQAAAAAAAAAAAAAAlgIAAAAAAAB2gvZqAAAAAAAAAAAAAAAA/0sAA5YCAABLAAAAECcAAAaAnty/dCWrM1hdyRy+7OF6Fv4UsKo9teLheVQh74WAxvp6877brTo9ZfNqq8l0MbG75MLS9uDkfKYCA0UvXWF6Dtrz0/GMJZ7n/jcXCtcXfg0jPMfthN4nxpdCQdk8PEb52ouNXC5gAiSl6sDUpTzcEy++dikAYAUE4JMTZRbGAAAAAAAAAACSWxwEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAfK/9v/qTGJqg0CsT4lxABMIRa2KhMkMYY6bVxIIAhy0AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAcAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAN2tOR3Hl77KwlIblrj9vrY1xFUmi/o15V5OmaaEkZGEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAyZllJfOaqBdOzkNB4QrDIoJpL4hOc1FNYTbi9tditAwEAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";

    /// Bins 645 to 680 of its bin array 9: id, X and Y reserves, stored
    /// price, open and begun limit orders, and whether they're asks.
    #[allow(clippy::type_complexity)]
    const LIVE_BINS: [(i32, u64, u64, u128, u64, u64, bool); 36] = [
        (645, 0, 5114317533, 2285493955166797373547, 0, 0, false),
        (646, 0, 5258058005, 2302635159830548361736, 0, 0, false),
        (647, 0, 5374613397, 2319904923529277463500, 0, 0, false),
        (648, 0, 5511141491, 2337304210455747047942, 0, 0, false),
        (649, 0, 5632537344, 2354833992034165143887, 0, 0, false),
        (650, 0, 5826623898, 2372495246974421396462, 0, 0, false),
        (651, 0, 5979837258, 2390288961326729552519, 0, 0, false),
        (652, 0, 6111333858, 2408216128536680017693, 0, 0, false),
        (653, 0, 6250688657, 2426277749500705115207, 0, 0, false),
        (654, 0, 6656542623, 2444474832621960406534, 0, 0, false),
        (655, 0, 6786543070, 2462808393866625122719, 0, 0, false),
        (656, 0, 14186875457, 2481279456820624812487, 0, 0, false),
        (657, 0, 14327086813, 2499889052746779485911, 0, 0, false),
        (658, 0, 14511569962, 2518638220642380334137, 0, 0, false),
        (659, 0, 17612657341, 2537528007297198197959, 0, 0, false),
        (660, 0, 19613487791, 2556559467351927175865, 0, 0, false),
        (661, 0, 16917803750, 2575733663357066636022, 0, 0, false),
        (662, 65100, 15768521282, 2595051665832244630194, 0, 0, false),
        (663, 110006212, 0, 2614514553325986466316, 0, 0, false),
        (664, 109545025, 0, 2634123412475931364611, 0, 0, true),
        (665, 91218418, 0, 2653879338069500847021, 0, 0, true),
        (666, 90565108, 0, 2673783433105022114321, 0, 0, true),
        (667, 80634002, 0, 2693836808853309764938, 0, 0, true),
        (668, 81041077, 0, 2714040584919709601711, 0, 0, true),
        (669, 80809045, 0, 2734395889306607418817, 11565959, 0, true),
        (670, 80661322, 0, 2754903858476406982538, 8609895, 0, true),
        (671, 79170550, 0, 2775565637414980023447, 5697201, 0, true),
        (672, 78277861, 0, 2796382379695592381207, 2827395, 0, true),
        (673, 79393606, 0, 2817355247543309312548, 0, 0, true),
        (674, 77609493, 0, 2838485411899884142967, 0, 0, true),
        (675, 76519319, 0, 2859774052489133272728, 0, 0, true),
        (676, 75798546, 0, 2881222357882801772213, 0, 0, true),
        (677, 75694431, 0, 2902831525566922791526, 0, 0, false),
        (678, 75213379, 0, 2924602762008674713585, 0, 0, true),
        (679, 75018322, 0, 2946537282723739759249, 0, 0, true),
        (680, 73500205, 0, 2968636312344167807507, 0, 0, true),
    ];

    fn live() -> (LbPair, Vec<BinArray>) {
        let data = base64::engine::general_purpose::STANDARD
            .decode(LIVE_PAIR)
            .unwrap();
        let pair = LbPair::decode(Pubkey::new_unique(), &data).unwrap();
        let mut array = empty_array(&pair, 9);
        for (id, x, y, price, open, begun, asks) in LIVE_BINS {
            *bin_mut(&mut array, id) = Bin {
                amount_x: x,
                amount_y: y,
                price,
                open_order_amount: open,
                processed_order_remaining_amount: begun,
                limit_order_ask_side: asks,
                ..Bin::default()
            };
        }
        (pair, vec![array])
    }

    fn empty_array(pair: &LbPair, index: i64) -> BinArray {
        BinArray {
            index,
            pair: pair.address,
            bins: vec![Bin::default(); BINS_PER_ARRAY as usize],
        }
    }

    fn bin_mut(array: &mut BinArray, id: i32) -> &mut Bin {
        let offset = i64::from(id) - array.index * i64::from(BINS_PER_ARRAY);
        &mut array.bins[usize::try_from(offset).unwrap()]
    }

    fn mark(pair: &mut LbPair, index: i64) {
        let bit = usize::try_from(index + BITMAP_HALF).unwrap();
        pair.bin_array_bitmap[bit / 64] |= 1 << (bit % 64);
    }

    /// A pool at bin 0, 1% apart, charging 1% plus the live pool's variable
    /// fee, with its fee in the token in.
    fn pool() -> LbPair {
        let mut pair = LbPair::decode(
            Pubkey::new_unique(),
            &pair_bytes(SPCX_MINT, USDC_MINT, 0, 100),
        )
        .unwrap();
        pair.filter_period = 30;
        pair.decay_period = 600;
        pair.reduction_factor = 5000;
        pair.variable_fee_control = 7500;
        pair.max_volatility_accumulator = 150_000;
        pair.protocol_share = 1000;
        pair.last_update_timestamp = 1_000;
        pair
    }

    fn quote(
        pair: &LbPair,
        arrays: &[BinArray],
        swap_for_y: bool,
        amount: u64,
        exact_in: bool,
    ) -> Swap {
        swap(
            pair,
            arrays,
            swap_for_y,
            amount,
            exact_in,
            pair.last_update_timestamp,
            0,
        )
        .unwrap()
    }

    #[test]
    fn prices_are_the_programs_to_the_bit() {
        // The program's own, as stored in the live pool's bins.
        for (id, _, _, stored, ..) in LIVE_BINS {
            assert_eq!(price_from_id(id, 75), Some(stored), "bin {id}");
        }
        assert_eq!(price_from_id(0, 75), Some(1 << 64));
        assert_eq!(price_from_id(-662, 75), Some(131127395805358038));
        assert_eq!(price_from_id(-2468, 10), Some(1565368187489452794));
        assert_eq!(price_from_id(1, 1), Some(18448588748116922571));
        assert_eq!(price_from_id(-1, 1), Some(18444899583751176498));
        assert_eq!(price_from_id(100_000, 1), Some(406113483393644920993599));
        assert_eq!(price_from_id(-100_000, 1), Some(837899702510255));
        assert_eq!(price_from_id(-443_636, 1), Some(1));
        // Past what Q64.64 holds.
        assert_eq!(price_from_id(2000, 400), None);
    }

    #[test]
    fn the_fee_is_base_plus_variable_capped() {
        let (pair, _) = live();
        assert_eq!(
            (
                pair.base_factor,
                pair.filter_period,
                pair.decay_period,
                pair.reduction_factor
            ),
            (10_000, 300, 1200, 5000)
        );
        assert_eq!(
            (
                pair.variable_fee_control,
                pair.max_volatility_accumulator,
                pair.protocol_share
            ),
            (7500, 150_000, 1000)
        );
        assert_eq!(
            (pair.function_type, pair.collect_fee_mode, pair.pair_type),
            (2, 1, 3)
        );
        assert_eq!(fee_rate(&pair, 0), Ok(7_500_000));
        // A bin away: 7500 × (10⁴ × 75)² / 10¹¹, rounded up.
        assert_eq!(fee_rate(&pair, 10_000), Ok(7_500_000 + 42_188));
        assert_eq!(fee_rate(&pair, 150_000), Ok(7_500_000 + 9_492_188));
        let mut steep = pair.clone();
        steep.base_fee_power_factor = 2;
        assert_eq!(fee_rate(&steep, 150_000), Ok(MAX_FEE_RATE));
        assert_eq!(pair.fee_rate(), 7_500_000);
    }

    #[test]
    fn volatility_decays_across_the_filter_and_decay_periods() {
        let mut pair = pool();
        pair.active_id = 5;
        pair.volatility_accumulator = 60_000;
        pair.volatility_reference = 20_000;
        pair.index_reference = 2;
        let start = |elapsed: i64| {
            let v = Volatility::at_start(&pair, pair.last_update_timestamp + elapsed).unwrap();
            (v.accumulator, v.reference, v.index_reference)
        };
        // Within the filter period the reference stands.
        assert_eq!(start(29), (60_000, 20_000, 2));
        // Then it halves the accumulator, at the active bin.
        assert_eq!(start(30), (60_000, 30_000, 5));
        assert_eq!(start(599), (60_000, 30_000, 5));
        // And past the decay period it's gone.
        assert_eq!(start(600), (60_000, 0, 5));
        let mut v = Volatility::at_start(&pair, pair.last_update_timestamp).unwrap();
        assert_eq!(v.accumulate(&pair, 5).unwrap(), 50_000);
        assert_eq!(v.accumulate(&pair, 20).unwrap(), 150_000);
    }

    #[test]
    fn live_swaps_match_the_reference() {
        let (pair, arrays) = live();
        let now = pair.last_update_timestamp + 400;
        let run = |swap_for_y, amount, exact_in| {
            let s = swap(&pair, &arrays, swap_for_y, amount, exact_in, now, 0).unwrap();
            (
                s.amount_in,
                s.amount_out,
                s.fee,
                s.protocol_fee,
                s.active_id,
                s.volatility_accumulator,
            )
        };
        // Selling SPCX: the fee comes off the USDC out.
        assert_eq!(
            run(true, 300_000_000, true),
            (
                300_000_000,
                41_620_437_920,
                316_804_137,
                31_680_413,
                660,
                20_000
            )
        );
        assert_eq!(
            run(false, 50_000_000_000, true),
            (
                50_000_000_000,
                347_030_767,
                386_915_768,
                38_691_574,
                666,
                40_000
            )
        );
        assert_eq!(
            run(true, 30_000_000_000, false),
            (
                215_639_609,
                30_000_000_000,
                227_314_851,
                22_731_585,
                661,
                10_000
            )
        );
        // Buying SPCX past bin 668 fills the asks resting in bins 669 to 672.
        assert_eq!(
            run(false, 1_000_000_000, false),
            (
                148_326_927_461,
                1_000_000_000,
                1_394_160_976,
                157_158_331,
                674,
                120_000
            )
        );
        assert_eq!(run(true, 1_000, true), (1_000, 139_622, 1_056, 105, 662, 0));
        let sale = swap(&pair, &arrays, true, 300_000_000, true, now, 0).unwrap();
        assert_eq!(
            (sale.fee_token, sale.bins_crossed, sale.bin_arrays),
            (Token::Y, 2, vec![9])
        );
        let purchase = swap(&pair, &arrays, false, 1_000_000_000, false, now, 0).unwrap();
        assert_eq!(purchase.fee_token, Token::Y);
    }

    #[test]
    fn exact_out_costs_what_exact_in_pays_back() {
        let (mut pair, arrays) = live();
        for mode in [0, 1] {
            pair.collect_fee_mode = mode;
            for (swap_for_y, out) in [(true, 25_000_000_000), (false, 400_000_000)] {
                let bought = quote(&pair, &arrays, swap_for_y, out, false);
                assert_eq!(bought.amount_out, out);
                let paid = quote(&pair, &arrays, swap_for_y, bought.amount_in, true);
                assert!(paid.amount_out >= out, "{mode} {swap_for_y}: {paid:?}");
                let short = quote(
                    &pair,
                    &arrays,
                    swap_for_y,
                    bought.amount_in * 999 / 1000,
                    true,
                );
                assert!(short.amount_out < out, "{mode} {swap_for_y}: {short:?}");
            }
        }
    }

    #[test]
    fn a_swap_crosses_bins_and_skips_empty_arrays() {
        let mut pair = pool();
        pair.active_id = 1;
        pair.index_reference = 1;
        let mut zero = empty_array(&pair, 0);
        let mut below = empty_array(&pair, -2);
        for id in [0, 1] {
            bin_mut(&mut zero, id).amount_y = 1_000_000;
        }
        for id in [-140, -139, -71] {
            bin_mut(&mut below, id).amount_y = 1_000_000;
        }
        mark(&mut pair, 0);
        mark(&mut pair, -2);
        let arrays = [below, zero];
        // Bins 1 and 0 pay out 2,000,000 Y; the rest comes from array -2,
        // past the empty array -1, from its top bin down.
        let s = quote(&pair, &arrays, true, 2_500_000, false);
        assert_eq!(s.amount_out, 2_500_000);
        assert_eq!(s.bin_arrays, [0, -2]);
        assert_eq!(s.active_id, -71);
        assert_eq!(s.bins_crossed, 72);
        // Each bin at a higher fee than the last.
        let one = quote(&pair, &arrays, true, 990_000, false);
        assert_eq!((one.active_id, one.volatility_accumulator), (1, 0));
        let two = quote(&pair, &arrays, true, 1_990_000, false);
        assert_eq!((two.active_id, two.volatility_accumulator), (0, 10_000));
        assert_eq!(s.volatility_accumulator, 150_000);
        assert!(s.amount_in > 2_500_000);
        // All of it, and then some, isn't there.
        assert_eq!(
            swap(&pair, &arrays, true, 5_000_001, false, 1_000, 0),
            Err(SwapError::InsufficientLiquidity)
        );
    }

    #[test]
    fn limit_orders_fill_after_the_reserves() {
        let mut pair = pool();
        mark(&mut pair, 0);
        let mut array = empty_array(&pair, 0);
        *bin_mut(&mut array, 0) = Bin {
            amount_y: 1_000,
            open_order_amount: 2_000,
            processed_order_remaining_amount: 500,
            limit_order_ask_side: false,
            ..Bin::default()
        };
        let arrays = [array];
        // A bin at price 1 holds 3,500 Y for X in: its reserves, then the
        // bids. A limit-order pool fills them all.
        pair.function_type = 2;
        let s = quote(&pair, &arrays, true, 3_500, false);
        assert_eq!((s.amount_out, s.active_id), (3_500, 0));
        // 1% of 3,500 in, rounded up, on top.
        assert_eq!((s.amount_in, s.fee), (3_536, 36));
        // The reserves earn a share of the fee (2/7, rounded up: 11), of
        // which the protocol takes 10% (1); the orders' 25, it takes half (13).
        assert_eq!(s.protocol_fee, 14);
        // A mining pool's swaps fill only the reserves.
        pair.function_type = 1;
        assert_eq!(
            swap(&pair, &arrays, true, 3_500, false, 1_000, 0),
            Err(SwapError::InsufficientLiquidity)
        );
        // Bids don't sell X.
        pair.function_type = 2;
        assert_eq!(
            swap(&pair, &arrays, false, 1, true, 1_000, 0),
            Err(SwapError::InsufficientLiquidity)
        );
    }

    #[test]
    fn swaps_that_cannot_run_say_why() {
        let (mut pair, arrays) = live();
        let now = pair.last_update_timestamp;
        let run = |pair: &LbPair, arrays: &[BinArray], amount| {
            swap(pair, arrays, true, amount, true, now, 50)
        };
        assert_eq!(run(&pair, &arrays, 0), Err(SwapError::ZeroAmount));
        // Past bin 645, the swap needs array 8, not loaded.
        assert_eq!(
            run(&pair, &arrays, 2_000_000_000),
            Err(SwapError::BinArrayNotLoaded(8))
        );
        assert_eq!(run(&pair, &[], 1), Err(SwapError::BinArrayNotLoaded(9)));
        pair.pair_type = 1;
        pair.activation_point = 51;
        assert_eq!(
            run(&pair, &arrays, 1),
            Err(SwapError::NotActivated {
                activation_point: 51
            })
        );
        pair.activation_point = 50;
        assert!(run(&pair, &arrays, 1).is_ok());
        pair.status = 1;
        assert_eq!(run(&pair, &arrays, 1), Err(SwapError::Disabled));
        pair.status = 0;
        pair.active_id = 600 * BINS_PER_ARRAY;
        assert_eq!(run(&pair, &arrays, 1), Err(SwapError::BeyondBitmap(600)));
        assert_eq!(
            SwapError::BinArrayNotLoaded(8).to_string(),
            "bin array 8 isn't loaded"
        );
    }
}
