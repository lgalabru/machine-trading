//! Plumbing for mth trading bots, on the [`luce`] trading library.
//!
//! A bot is started by the grader with its environment described by
//! [`BotEnv`]: an RPC endpoint, a funded keypair whose Phoenix trader account
//! already holds collateral, the market to trade, the slot the session ends
//! at, and the limits it must stay within. The chain is an offline Solana
//! sandbox, so Phoenix's market metadata comes from a file instead of
//! Phoenix's HTTP API.
//!
//! Two levels:
//!
//! - **The bot contract**: [`BotEnv`], and [`Trader`] with its `f64`
//!   interface (`mark_usd`, `position`, `place_limit`, `place_market`,
//!   `cancel_all`). Enough for most bots.
//! - **luce**, re-exported whole as [`luce`] and in part at the top level:
//!   exact [`Decimal`] prices and quantities, the Phoenix market as an
//!   [`Instrument`], [`Order`]s, [`RiskLimits`] (see
//!   [`BotEnv::risk_limits`]), and [`ExecutionDriver`] (see
//!   [`Trader::driver`]). [`Trader::luce`] is the luce trader underneath.

mod env;
mod error;
mod phoenix;
mod trader;
mod tx;

pub use env::BotEnv;
pub use error::{Error, Result};
/// The trading library botkit is built on.
pub use luce;
/// Re-exported so bots don't have to pin matching versions themselves.
pub use luce::protocols::phoenix::phoenix_rise;
pub use luce::{
    Decimal,
    core::{
        Asset, ExecutionDriver, Exposure, Fill, Instrument, InstrumentId, Order, OrderBook,
        Portfolio, RiskCheck, RiskCheckResult, RiskLimits, RiskReport, Rounding, TradeContext,
        Verdict,
    },
    protocols::phoenix::{
        PhoenixDriver, PhoenixExchange, PhoenixMarket, PhoenixOrder, PhoenixTrader,
        phoenix_rise::math::Side,
    },
};
pub use phoenix::{Phoenix, QUOTE_LOTS_PER_USD, load_exchange};
pub use solana_keypair::Keypair;
pub use solana_pubkey::Pubkey;
pub use solana_rpc_client::rpc_client::RpcClient;
pub use solana_signer::Signer;
pub use trader::{Position, Trader};
pub use tx::{rpc_client, send};
