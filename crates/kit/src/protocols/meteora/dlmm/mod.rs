//! Meteora DLMM pools (feature `meteora-dlmm`): liquidity in discrete bins,
//! each a constant-sum pool at one price, as the program's IDL lays its
//! accounts out. Needs no chain: the decoders, the bin math and the
//! instructions build for `wasm32-wasip2`.
//!
//! | Meteora DLMM | Here |
//! |--------------|------|
//! | `LbPair` | [`LbPair`]: the active bin, the bin step, the fee |
//! | `BinArray` (70 bins, PDA by index) | [`BinArray`], [`bin_array_address`] |
//! | `PositionV2` (70 bins of shares, up to 1,400 once grown) | [`Position`] |
//! | `swap2`, `swap_exact_out2`, `initialize_position`, `add_liquidity_by_strategy2`, `remove_liquidity_by_range2`, `claim_fee2` | [`ix`] |
//! | bin prices, a position's amounts and fees | [`math`] |
//!
//! Token X is the pool's base and token Y its quote: a bin's price is Y per
//! X.

pub mod ix;
pub mod math;
mod state;

use solana_pubkey::Pubkey;
pub use state::{Bin, BinArray, LbPair, Position};

use crate::protocols::{NamedMarket, SPCX_MINT, USDC_MINT};

/// The Meteora DLMM program.
pub const PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");

/// The pools the Meteora venue profile names.
pub const POOLS: &[NamedMarket] = &[NamedMarket {
    symbol: "SPCX-USDC",
    address: solana_pubkey::pubkey!("7nVQtYQipN564E9oBi6yZFGox4WL6CR4zsLyYipqgTWu"),
    base_mint: SPCX_MINT,
    quote_mint: USDC_MINT,
}];

/// Bins in one bin array, and the most one position spans.
pub const BINS_PER_ARRAY: i32 = 70;

/// The bin array holding `bin`.
pub fn bin_array_index(bin: i32) -> i64 {
    i64::from(bin.div_euclid(BINS_PER_ARRAY))
}

/// The bin arrays a new position from `lower_bin_id` names in its
/// instructions: the one holding that bin and the next. A new position spans
/// at most [`BINS_PER_ARRAY`] bins, so the two hold it whole; a grown one
/// names its own ([`Position::bin_arrays`]).
pub fn position_bin_arrays(lower_bin_id: i32) -> Vec<i64> {
    let lower = bin_array_index(lower_bin_id);
    vec![lower, lower + 1]
}

/// A pool's bin array `index`.
pub fn bin_array_address(pair: &Pubkey, index: i64) -> Pubkey {
    Pubkey::find_program_address(
        &[b"bin_array", pair.as_ref(), &index.to_le_bytes()],
        &PROGRAM_ID,
    )
    .0
}

/// A pool's bin array bitmap extension, which records the bin arrays its
/// own bitmap can't reach.
pub fn bitmap_extension_address(pair: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"bitmap", pair.as_ref()], &PROGRAM_ID).0
}

/// The program's event authority (Anchor's event CPI).
pub fn event_authority() -> Pubkey {
    Pubkey::find_program_address(&[b"__event_authority"], &PROGRAM_ID).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bins_fall_in_arrays_of_seventy() {
        assert_eq!(bin_array_index(0), 0);
        assert_eq!(bin_array_index(69), 0);
        assert_eq!(bin_array_index(662), 9);
        assert_eq!(bin_array_index(-1), -1);
        assert_eq!(bin_array_index(-70), -1);
        assert_eq!(bin_array_index(-71), -2);
        assert_eq!(position_bin_arrays(662), [9, 10]);
        assert_eq!(position_bin_arrays(-1), [-1, 0]);
    }
}
