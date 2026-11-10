//! Phoenix Eternal perpetuals on Solana (feature `phoenix`).
//!
//! | Phoenix | Core |
//! |---------|------|
//! | `/exchange` metadata | [`PhoenixExchange`] |
//! | a perp market | [`PhoenixMarket`] → [`Instrument`](crate::core::Instrument) (perpetual, base = the asset, quote = USDC) |
//! | ticks, base lots, quote lots | exact [`Decimal`](rust_decimal::Decimal) conversions on [`PhoenixMarket`] |
//! | Hawkeye BBO view | [`OrderBook`](crate::core::OrderBook) (top of book, prices only) |
//! | Hawkeye margin view | [`Position`](crate::core::Position) |
//! | limit / market order, cancel all | [`PhoenixTrader`] plans, [`chain::execute`](crate::chain::execute) sends |
//! | order flow | [`PhoenixDriver`] → [`ExecutionDriver`](crate::core::ExecutionDriver) |
//!
//! The trader account is the authority's cross-margin account (PDA index 0,
//! subaccount 0). Phoenix's own SDK, `phoenix-rise`, is re-exported as
//! [`phoenix_rise`] for anything not wrapped here.

mod driver;
mod error;
mod exchange;
mod market;
mod trader;

pub use driver::PhoenixDriver;
pub use error::PhoenixError;
pub use exchange::{PhoenixExchange, load_exchange};
pub use market::{DEFAULT_QUOTE_LOT_DECIMALS, PhoenixMarket};
/// Phoenix's SDK, re-exported so dependents do not have to pin it.
pub use phoenix_rise;
pub use trader::{PhoenixOrder, PhoenixTrader};

/// The venue identifier of every Phoenix instrument.
pub const VENUE: &str = "phoenix";

/// Compute-unit limit for Phoenix transactions, which routinely exceed the
/// 200k default.
pub const COMPUTE_UNIT_LIMIT: u32 = 1_400_000;
