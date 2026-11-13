//! Raydium CLMM pools (feature `raydium-clmm`): concentrated liquidity on a
//! grid of ticks, as the program's IDL lays its accounts out. Needs no
//! chain: the decoders, the pool math and the instructions build for
//! `wasm32-wasip2`.
//!
//! | Raydium CLMM | Here |
//! |--------------|------|
//! | `PoolState`, `AmmConfig` | [`PoolState`], [`AmmConfig`]: the price, the active liquidity, the fee |
//! | `TickArrayState` (60 ticks, PDA by start tick) | [`TickArray`], [`tick_array_address`] |
//! | `PersonalPositionState` and its NFT | [`PersonalPosition`], [`personal_position_address`] |
//! | `swap_v2`, `open_position_with_token22_nft`, `increase_liquidity_v2`, `decrease_liquidity_v2` | [`ix`] |
//! | prices, ticks, a position's amounts and fees | [`math`] |
//!
//! Token 0 and token 1 are ordered by mint address, so a pool's base may be
//! either: [`PoolState::base_is_0`] says which.

pub mod ix;
pub mod math;
mod state;

use solana_pubkey::Pubkey;
pub use state::{AmmConfig, PersonalPosition, PoolState, Tick, TickArray};

use crate::protocols::{NamedMarket, SPCX_MINT, USDC_MINT};

/// The Raydium CLMM program.
pub const PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");

/// The pools the Raydium venue profile names.
pub const POOLS: &[NamedMarket] = &[NamedMarket {
    symbol: "SPCX-USDC",
    address: solana_pubkey::pubkey!("FjuBy7jjf9DXj9d3R7cHpvcnoFW2iQxf7F7P3vqx4Jza"),
    base_mint: SPCX_MINT,
    quote_mint: USDC_MINT,
}];

/// Ticks in one tick array.
pub const TICK_ARRAY_SIZE: i32 = 60;

/// The tick array holding `tick`: its first tick.
pub fn tick_array_start(tick: i32, tick_spacing: u16) -> i32 {
    let span = TICK_ARRAY_SIZE * i32::from(tick_spacing);
    tick.div_euclid(span) * span
}

/// The tick array starting at `start`.
pub fn tick_array_address(pool: &Pubkey, start: i32) -> Pubkey {
    Pubkey::find_program_address(
        &[b"tick_array", pool.as_ref(), &start.to_be_bytes()],
        &PROGRAM_ID,
    )
    .0
}

/// A pool's tick array bitmap extension, which records the tick arrays the
/// pool's own bitmap can't reach.
pub fn bitmap_extension_address(pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[b"pool_tick_array_bitmap_extension", pool.as_ref()],
        &PROGRAM_ID,
    )
    .0
}

/// The position an NFT mint stands for.
pub fn personal_position_address(nft_mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"position", nft_mint.as_ref()], &PROGRAM_ID).0
}

/// The pool's record of a tick range, which older positions shared. The
/// program no longer reads it, but its instructions still take it.
pub fn protocol_position_address(pool: &Pubkey, tick_lower: i32, tick_upper: i32) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"position",
            pool.as_ref(),
            &tick_lower.to_be_bytes(),
            &tick_upper.to_be_bytes(),
        ],
        &PROGRAM_ID,
    )
    .0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_arrays_start_on_their_span() {
        assert_eq!(tick_array_start(120, 3), 0);
        assert_eq!(tick_array_start(-120, 3), -180);
        assert_eq!(tick_array_start(-1002, 30), -1800);
        assert_eq!(tick_array_start(-600, 10), -600);
        assert_eq!(tick_array_start(49438, 60), 46800);
    }
}
