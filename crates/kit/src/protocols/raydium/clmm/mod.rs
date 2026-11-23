//! Raydium CLMM concentrated-liquidity pools (feature `raydium-clmm`):
//! decoding, an exact swap quote, and the `swap_v2` instruction.
//!
//! Atomic arbitrage (`docs/rfd/0005-atomic-arbitrage-tasks.md`) lives on
//! the last atom: a route that is one atom short reverts, and one that
//! leaves an atom on the table is beaten by a bot that didn't. So the quote
//! is the program's own swap, ported line by line from
//! `raydium-io/raydium-clmm@6d7a0f82d2f293c553c69aaf943377367c6c4cc6`
//! (`programs/amm/src`), not an approximation of the curve: the same
//! integer math and rounding ([`math`]), the same walk over tick arrays
//! and bitmaps ([`state`]), dynamic fees, fees on the output token, and
//! limit orders resting on ticks ([`quote`]).
//!
//! | Raydium CLMM | Here |
//! |--------------|------|
//! | `PoolState`, `AmmConfig`, `TickArrayState`, `TickArrayBitmapExtension` | [`Pool`], [`AmmConfig`], [`TickArray`], [`BitmapExtension`] |
//! | which tick arrays a swap enters | [`tick_arrays_in_direction`] (to fetch), [`SwapQuote::tick_arrays`] (to pass) |
//! | `swap_v2`'s result: vault deltas, fee, end price and tick | [`quote()`] → [`SwapQuote`] |
//! | `swap_v2` | [`ix::swap_v2`], with [`ix::create_token_accounts`] |
//!
//! To swap: fetch the pool, its config, its bitmap extension (if it has
//! one: [`ix::bitmap_extension_address`]) and the tick arrays
//! [`tick_arrays_in_direction`] lists; quote; then send
//! [`ix::swap_v2`] with the quote's tick arrays and a threshold from its
//! amounts.
//!
//! Token-2022 transfer fees are not modelled: on a mint with a transfer
//! fee, `swap_v2` moves the fee on top of what the quote says.
//!
//! # Exactness
//!
//! Checked against the deployed program (last deployed in slot 459932378)
//! on a Surfpool fork of mainnet, on 2026-11-21: a fresh keypair, funded
//! with the `surfnet_setAccount` and `surfnet_setTokenAccount` cheatcodes,
//! swaps through `swap_v2` with the quote's own amount as its threshold,
//! and both vaults and both wallet accounts must move by exactly the
//! quoted amounts (`tests/raydium_clmm.rs`,
//! `swaps_on_a_mainnet_fork_move_exactly_the_quoted_amounts`, which says
//! how to run it). Every swap matched to the atom:
//!
//! - SOL/USDC ([`SOL_USDC_POOL`], fee on the input): 8 swaps, exact in and
//!   exact out both ways, from 100 atoms to 1,500 SOL and 120,000 USDC,
//!   crossing up to 49 initialized ticks over 3 tick arrays.
//! - A pool that takes its fee in token 0 (`Bu9HQ5sL…`): 4 swaps.
//! - Two pools with a dynamic fee (`CZPArpHe…`, which also takes its fee
//!   in token 1, and `GvgbFE8A…`): 3 swaps each, one of them a 1-atom
//!   input; and on each, a swap past the last liquidity that the quote
//!   and the program both refused.
//!
//! Not yet seen on chain: limit orders (the mainnet pools above had none
//! resting where the swaps went; the unit tests replay the program's own
//! limit-order tests), and a dynamic fee across tick spacing groups.

mod error;
pub mod ix;
pub mod math;
mod quote;
pub mod state;

pub use error::ClmmError;
pub use quote::{SwapDirection, SwapMode, SwapQuote, SwapRequest, quote, tick_arrays_in_direction};
use solana_pubkey::Pubkey;
pub use state::{AmmConfig, BitmapExtension, DynamicFee, FeeOn, Pool, PoolToken, Tick, TickArray};

pub use super::VENUE;

/// The Raydium CLMM program.
pub const PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");

/// The SOL/USDC pool with the most liquidity (4 bp, tick spacing 1).
pub const SOL_USDC_POOL: Pubkey =
    solana_pubkey::pubkey!("3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv");

/// Tick arrays one swap may enter. A transaction locks at most 64
/// accounts (`MAX_TX_ACCOUNT_LOCKS`), and `swap_v2` itself takes 13.
pub const TICK_ARRAYS_MAX: usize = 64;

/// Steps (price moves between ticks or tick spacing groups) one quote may
/// take. A step costs the program well over 100 compute units and a
/// transaction has 1.4 million, so no swap that lands takes more.
pub const STEPS_MAX: usize = 14_000;

/// Compute units to request for one `swap_v2`: Raydium's own SDK asks for
/// 600,000 for a swap that crosses many ticks.
pub const COMPUTE_UNIT_LIMIT: u32 = 600_000;
