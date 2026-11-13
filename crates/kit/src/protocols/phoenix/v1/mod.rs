//! Phoenix v1 spot markets on Solana (feature `phoenix-v1`), as the Phoenix
//! venue profile's spot markets (`specs/venues/phoenix.md`) define them.
//! Feature `phoenix-v1-venue` alone builds what needs no chain, for a
//! strategy component: [`state`], [`PhoenixV1Market`] and [`ix`].
//!
//! Phoenix v1 is a central limit order book for spot tokens: one account
//! per market holds its parameters, both books and every trader's seat
//! (its `TraderState`). It is a different program from Phoenix Eternal
//! ([`super::eternal`]), on the same venue: instruments are
//! `phoenix:spot:<SYMBOL>`, with the symbols in [`MARKETS`], or the
//! market's address for any other.
//!
//! | Phoenix v1 | Core |
//! |------------|------|
//! | the market account | [`MarketState`] (book with owners and sequence numbers, seats) |
//! | a market | [`PhoenixV1Market`] → [`Instrument`](crate::core::Instrument) (spot, base and quote mints) |
//! | the account (wallet token accounts and seat) | [`SpotBalances`](crate::core::SpotBalances) from `holdings()`; its base's total is the profile's position |
//! | ticks, base lots, quote lots | exact [`Decimal`](rust_decimal::Decimal) conversions on [`PhoenixV1Market`] |
//! | the book | [`OrderBook`](crate::core::OrderBook) with sizes; [`OwnedOrder`]s with their traders |
//! | `Swap`, post-only and limit orders, cancel all | [`PhoenixV1Trader`] plans, [`chain::execute`](crate::chain::execute) or [`chain::fire_and_forget`](crate::chain::fire_and_forget) sends |
//! | order flow | [`PhoenixV1Driver`] → [`ExecutionDriver`](crate::core::ExecutionDriver) |
//!
//! Swaps settle from and to the trader's associated token accounts and need
//! no seat. Resting orders need one, claimed through the seat manager
//! ([`ix::claim_seat`]). [`ix`] has every instruction the trader side uses.

#[cfg(feature = "phoenix-v1")]
mod driver;
#[cfg(feature = "phoenix-v1")]
mod error;
#[cfg(feature = "phoenix-v1")]
pub mod fills;
pub mod ix;
mod market;
#[cfg(feature = "phoenix-v1")]
mod meter;
pub mod state;
#[cfg(feature = "phoenix-v1")]
mod trader;

#[cfg(feature = "phoenix-v1")]
pub use driver::PhoenixV1Driver;
#[cfg(feature = "phoenix-v1")]
pub use error::PhoenixV1Error;
pub use market::{OwnedOrder, PhoenixV1Market};
#[cfg(feature = "phoenix-v1")]
pub use meter::{PhoenixV1Meter, balances};
use solana_pubkey::Pubkey;
pub use state::{BookSide, MarketHeader, MarketState, RestingOrder, TraderState};
#[cfg(feature = "phoenix-v1")]
pub use trader::{PhoenixV1Order, PhoenixV1Trader};

pub use super::VENUE;
use crate::protocols::{NamedMarket, USDC_MINT, WRAPPED_SOL_MINT};

/// The Phoenix v1 program.
pub const PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("PhoeNiXZ8ByJGLkxNfZRnkUfjvmuYqLR89jjFHGqdXY");

/// The Phoenix seat manager, which grants seats to any trader.
pub const SEAT_MANAGER_ID: Pubkey =
    solana_pubkey::pubkey!("PSMxQbAoDWDbvd9ezQJgARyq6R9L5kJAasaLDVcZwf1");

/// The spot markets the Phoenix venue profile names.
pub const MARKETS: &[NamedMarket] = &[NamedMarket {
    symbol: "SOL-USDC",
    address: solana_pubkey::pubkey!("4DoNfFBfF7UokCC2FQzriy7yHK6DY6NVdYpuekQ5pRgg"),
    base_mint: WRAPPED_SOL_MINT,
    quote_mint: USDC_MINT,
}];

/// Compute-unit limit for Phoenix v1 transactions.
pub const COMPUTE_UNIT_LIMIT: u32 = 400_000;
