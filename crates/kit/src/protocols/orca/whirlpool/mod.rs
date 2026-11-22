//! Orca Whirlpool concentrated-liquidity pools (feature `orca-whirlpool`):
//! decoding, an exact swap quote, and the `swap_v2` instruction.
//!
//! Atomic arbitrage (`docs/rfd/0005-atomic-arbitrage-tasks.md`) lives on
//! the last atom: a route one atom short reverts, and one that leaves an
//! atom on the table loses to a bot that didn't. So the quote is the
//! program's own swap, ported line by line from
//! `orca-so/whirlpools@cc683ec92acb5e7606caa3d8e514d9383e9c7c11`
//! (`programs/whirlpool/src`), not an approximation of the curve: the same
//! integer math and rounding ([`math`]), the same three-array walk over
//! fixed and dynamic tick arrays ([`state`]), and the adaptive fee an
//! oracle holds.
//!
//! | Whirlpool | Here |
//! |-----------|------|
//! | `Whirlpool`, `TickArray`, `DynamicTickArray`, `Oracle` | [`Whirlpool`], [`TickArray`], [`Oracle`] |
//! | which tick arrays a swap walks | [`tick_array_addresses`] (to fetch), [`SwapQuote::tick_arrays`] (to pass) |
//! | `swap_v2`'s result: vault deltas, fee, end price and tick | [`quote()`] → [`SwapQuote`] |
//! | `swap_v2` | [`ix::swap_v2`], with [`ix::create_token_accounts`] |
//!
//! To swap: fetch the pool, the tick arrays [`tick_array_addresses`] lists
//! (an address with no account is [`TickArray::uninitialized`]) and, for
//! an adaptive-fee pool ([`Whirlpool::is_adaptive_fee_tier`]), its oracle
//! ([`ix::oracle_address`]); quote; then send [`ix::swap_v2`] with the
//! quote's tick arrays and a threshold from its amounts.
//!
//! Not modelled: Token-2022 transfer fees (on such a mint `swap_v2` moves
//! the fee on top of what the quote says) and transfer hooks (the builder
//! passes no hook accounts), and supplemental tick arrays (a swap past the
//! third array fails, in the quote as on chain).
//!
//! # Exactness
//!
//! Checked against the deployed program on a Surfpool fork of mainnet
//! (Surfpool 1.6.1, slot 472311307), on 2026-11-21: a fresh keypair,
//! funded with the `surfnet_setAccount` and `surfnet_setTokenAccount`
//! cheatcodes, swaps through `swap_v2` with the quote's own amount as its
//! threshold; both vaults and both wallet accounts must move by exactly
//! the quoted amounts, and the pool must end at the quoted price, tick and
//! liquidity (`tests/orca_whirlpool.rs`,
//! `swaps_on_a_mainnet_fork_move_exactly_the_quoted_amounts`, which says
//! how to run it). Every swap matched to the atom:
//!
//! - SOL/USDC ([`SOL_USDC_POOL`], static fee): 11 swaps, exact in and
//!   exact out both ways, from 1 atom to 20,000 SOL, crossing up to 100
//!   initialized ticks over 2 tick arrays; and 2 swaps that need a fourth
//!   array, which the quote and the program both refused.
//! - Five adaptive-fee SOL pools (tick spacings 1, 4, 8, 64 and 128; one
//!   with a Token-2022 mint): 19 swaps of up to 3% of a vault, over fixed,
//!   dynamic and uninitialized tick arrays, with adaptive fees up to 4.5%;
//!   and one swap past the third array, refused by both.
//!
//! Each `TickArraySequenceInvalidIndex` refusal is the program's too, but
//! only its failing is compared, not its error code.

mod error;
mod fee;
pub mod ix;
pub mod math;
mod quote;
pub mod state;

pub use error::{ProgramFailure, WhirlpoolError};
pub use quote::{
    SWAP_TICK_ARRAYS_MAX, SwapDirection, SwapMode, SwapQuote, SwapRequest, quote,
    tick_array_addresses, tick_array_start_indexes,
};
use solana_pubkey::Pubkey;
pub use state::{Oracle, TICK_ARRAY_SIZE, TickArray, TickArrayLayout, Whirlpool};

pub use super::VENUE;

/// The Whirlpool program.
pub const PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");

/// The SOL/USDC pool with 4 bp fees and tick spacing 4.
pub const SOL_USDC_POOL: Pubkey =
    solana_pubkey::pubkey!("Czfq3xZZDmsdGdUyrNLtRhGc47cXcZtLG4crryfu44zE");

/// Steps (price moves to a tick or a tick group's edge) one quote may
/// take. A step costs the program well over 100 compute units and a
/// transaction has 1.4 million, so no swap that lands takes more.
pub const STEPS_MAX: usize = 14_000;

/// Compute units to request for one `swap_v2` that may cross many ticks:
/// the transaction maximum. Crossing an initialized tick costs a few
/// thousand units; on the mainnet fork a swap across 111 of them ran out
/// of 400,000. A route that budgets several swaps can size each from
/// [`SwapQuote::ticks_crossed`].
pub const COMPUTE_UNIT_LIMIT: u32 = 1_400_000;
