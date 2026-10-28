//! Plumbing for ott trading bots.
//!
//! A bot is started by the grader with its environment described by
//! [`BotEnv`]: an RPC endpoint, a funded keypair whose Phoenix trader account
//! already holds collateral, the market to trade, and the slot the session
//! ends at. The chain is an offline Solana sandbox, so Phoenix's market
//! metadata comes from a file instead of Phoenix's HTTP API.

mod env;
mod error;
mod phoenix;
mod trader;
mod tx;

pub use env::BotEnv;
pub use error::{Error, Result};
pub use phoenix::{Phoenix, QUOTE_LOTS_PER_USD, load_exchange};
/// Re-exported so bots don't have to pin matching versions themselves.
pub use phoenix_rise;
pub use phoenix_rise::math::Side;
pub use solana_keypair::Keypair;
pub use solana_pubkey::Pubkey;
pub use solana_rpc_client::rpc_client::RpcClient;
pub use solana_signer::Signer;
pub use trader::{Position, Trader};
pub use tx::{rpc_client, send};
