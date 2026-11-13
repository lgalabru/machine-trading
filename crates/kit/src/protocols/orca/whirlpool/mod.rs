//! Orca Whirlpool pools (feature `orca-whirlpool`): concentrated liquidity
//! on a grid of ticks, as the program's source lays its accounts out.
//! Needs no chain: the decoders, the pool math and the instructions build
//! for `wasm32-wasip2`.
//!
//! | Whirlpool | Here |
//! |-----------|------|
//! | `Whirlpool` | [`PoolState`]: the price, the active liquidity, the fee |
//! | `FixedTickArray` and `DynamicTickArray` (88 ticks, PDA by start tick) | [`TickArray`], [`tick_array_address`] |
//! | `Position` and its NFT | [`Position`], [`position_address`] |
//! | `swap_v2`, `open_position_with_token_extensions`, `increase_liquidity_v2`, `decrease_liquidity_v2`, `update_fees_and_rewards`, `collect_fees_v2` | [`ix`] |
//! | prices, ticks, a position's amounts and fees | [`math`] |
//!
//! Token A and token B are ordered by mint address, so a pool's base may be
//! either: [`PoolState::base_is_a`] says which.

pub mod ix;
pub mod math;
mod state;

use solana_pubkey::Pubkey;
pub use state::{PoolState, Position, Tick, TickArray};

use crate::protocols::{NamedMarket, SPCX_MINT, USDC_MINT};

/// The Whirlpool program.
pub const PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");

/// The pools the Orca venue profile names.
pub const POOLS: &[NamedMarket] = &[NamedMarket {
    symbol: "SPCX-USDC",
    address: solana_pubkey::pubkey!("FgjxPHeYsq1axH29fnGKZi6UyRWi4tMPHgu1gc3ezA9U"),
    base_mint: SPCX_MINT,
    quote_mint: USDC_MINT,
}];

/// Ticks in one tick array.
pub const TICK_ARRAY_SIZE: i32 = 88;

/// The tick array holding `tick`: its first tick.
pub fn tick_array_start(tick: i32, tick_spacing: u16) -> i32 {
    let span = TICK_ARRAY_SIZE * i32::from(tick_spacing);
    tick.div_euclid(span) * span
}

/// The tick array starting at `start`: the start tick is a seed in
/// decimal text.
pub fn tick_array_address(pool: &Pubkey, start: i32) -> Pubkey {
    Pubkey::find_program_address(
        &[b"tick_array", pool.as_ref(), start.to_string().as_bytes()],
        &PROGRAM_ID,
    )
    .0
}

/// The pool's oracle account, which a swap names whether or not the pool
/// has one (only pools with an adaptive fee do).
pub fn oracle_address(pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"oracle", pool.as_ref()], &PROGRAM_ID).0
}

/// The position an NFT mint stands for.
pub fn position_address(position_mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"position", position_mint.as_ref()], &PROGRAM_ID).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_arrays_start_on_their_span() {
        assert_eq!(tick_array_start(120, 64), 0);
        assert_eq!(tick_array_start(-1, 64), -5632);
        assert_eq!(tick_array_start(-5632, 64), -5632);
        assert_eq!(tick_array_start(49497, 64), 45056);
    }

    #[test]
    fn tick_array_seeds_are_the_pool_and_the_start_tick_as_text() {
        // Two of the live pool's arrays, as mainnet holds them.
        let pool = POOLS[0].address;
        assert_eq!(
            tick_array_address(&pool, 45056).to_string(),
            "35vtKgcS2HKiM1ZPZv62TtBibrk46KUXdFf1M5umsbPZ"
        );
        assert_eq!(
            tick_array_address(&pool, -444_928).to_string(),
            "BUA1cjZ67i4yp8iLz1nuCeZbEXbcNZpCQLyAnAcnErTe"
        );
    }
}
