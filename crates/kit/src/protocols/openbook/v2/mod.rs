//! OpenBook v2 spot markets on Solana (feature `openbook-v2`), as the
//! OpenBook venue profile (`specs/venues/openbook.md`) defines them.
//!
//! Anyone can create a market, so a market is named by its address, or by
//! a symbol the profile gives it ([`MARKETS`]); instruments are
//! `openbook:spot:<SYMBOL>`. A market's own `name` field is not a symbol.
//!
//! | OpenBook v2 | Core |
//! |-------------|------|
//! | the `Market` account | [`MarketAccount`] → [`OpenBookV2Market`] → [`Instrument`](crate::core::Instrument) (spot) |
//! | price lots, base lots | exact [`Decimal`](rust_decimal::Decimal) conversions on [`OpenBookV2Market`] |
//! | the two `BookSide` accounts | [`LeafOrder`]s with owners and placement times; [`OrderBook`](crate::core::OrderBook) with sizes |
//! | an `OpenOrdersAccount` | [`OpenOrdersPosition`] (free and locked funds, maker volume) |
//! | the account (wallet token accounts and every open orders account the `OpenOrdersIndexer` lists) | [`SpotBalances`](crate::core::SpotBalances) from `holdings()`; its base's total is the profile's position |
//! | take-only (`place_take_order`), post-only and limit orders, cancel all, settle | [`OpenBookV2Trader`] plans, [`chain::execute`](crate::chain::execute) or [`chain::fire_and_forget`](crate::chain::fire_and_forget) sends |
//! | order flow | [`OpenBookV2Driver`] → [`ExecutionDriver`](crate::core::ExecutionDriver) |
//!
//! Instructions follow the program's IDL ([`ix`]). A maker's fills settle
//! into its open orders account when `consume_events` processes the event
//! heap, which this adapter does not run.

mod driver;
mod error;
pub mod fills;
pub mod ix;
mod market;
mod meter;
pub mod state;
mod trader;

pub use driver::OpenBookV2Driver;
pub use error::OpenBookV2Error;
pub use market::{MarketKeys, OpenBookV2Market};
pub use meter::{OpenBookV2Meter, balances};
use solana_pubkey::Pubkey;
pub use state::{LeafOrder, MarketAccount, OpenOrdersIndexer, OpenOrdersPosition};
pub use trader::{ACCOUNT_NUMBER, Book, MATCH_LIMIT, OpenBookV2Order, OpenBookV2Trader, read_book};

pub use super::VENUE;
use crate::protocols::{NamedMarket, USDC_MINT, WRAPPED_SOL_MINT};

/// The OpenBook v2 program.
pub const PROGRAM_ID: Pubkey =
    solana_pubkey::pubkey!("opnb2LAfJYbRMAHHvqjCwQxanZn7ReEHp1k81EohpZb");

/// The markets the OpenBook venue profile names.
pub const MARKETS: &[NamedMarket] = &[NamedMarket {
    symbol: "SOL-USDC",
    address: solana_pubkey::pubkey!("CFSMrBssNG8Ud1edW59jNLnq2cwrQ9uY5cM3wXmqRJj3"),
    base_mint: WRAPPED_SOL_MINT,
    quote_mint: USDC_MINT,
}];

/// Compute-unit limit for OpenBook v2 transactions.
pub const COMPUTE_UNIT_LIMIT: u32 = 400_000;
