//! mtx-kit: trading on Solana, and the plumbing mtx bots are built on.
//!
//! # Bots
//!
//! A bot is started by the grader with its environment described by
//! [`BotEnv`]: an RPC endpoint, a funded keypair whose Phoenix trader account
//! already holds collateral, the market to trade, the slot the session ends
//! at, and the [`Mandate`] it must stay within. The chain is an offline Solana
//! sandbox, so Phoenix's market metadata comes from a file instead of
//! Phoenix's HTTP API.
//!
//! Most bots write a [`Strategy`], and [`run`] answers the grader, reads the
//! market every step, and sends the strategy's [`Orders`] inside the mandate.
//! See [`run`](mod@run) and the [`prelude`]. Below it, [`Trader`] has an
//! `f64` interface (`mark_usd`, `position`, `place_limit`, `place_market`,
//! `cancel_all`), and [`Turns`] speaks the turn protocol alone.
//!
//! # The trading library
//!
//! Three layers, each depending only on the ones below it, on the vocabulary
//! of `mtx-core` (re-exported in [`core`]):
//!
//! ```text
//!  protocols   phoenix (feature "phoenix"): a Phoenix perp as an Instrument,
//!     │        exact tick/lot math, a trader that plans orders, an ExecutionDriver;
//!     │        phoenix::v1, openbook::v2 (spot books, the same pieces), pyth (prices)
//!     ▼
//!  chain       ChainClient · TransactionSigner · InstructionGroup → pack →
//!     │        preview → execute (simulate before every signature) · tokens
//!  lending     the LendingProvider contract, reserve math, lending risk checks
//!     ▼
//!  core        Asset · Instrument · Order · OrderBook · Fill · Position ·
//!              Portfolio · RiskLimits · ExecutionDriver · SimulatedExecution ·
//!              Yield · Strategy · Performance. Pure data, no I/O.
//! ```
//!
//! # Conventions
//!
//! - **Exact amounts.** Everything is a [`Decimal`]. Conversions to the
//!   integers venues use (native token units, ticks, lots) are exact or they
//!   fail; nothing rounds silently. Where rounding is wanted, the caller
//!   picks a direction ([`core::Rounding`]).
//! - **Providers describe; the kit acts.** A protocol adapter reads state into
//!   core types and turns an intent into
//!   [`InstructionGroup`](chain::InstructionGroup)s. Packing, simulation,
//!   signing and sending happen in [`chain`], under the caller's signer.
//!   Adapters never see a key.
//! - **Simulate before signing.** [`chain::execute`] simulates every
//!   transaction right before signing it and refuses to sign one that would
//!   fail. [`chain::fire_and_forget`], for worlds that land a transaction after the
//!   sender's turn, does too unless the caller turns it off.
//! - **One risk vocabulary.** Trading limits ([`core::RiskLimits`]) and
//!   lending checks ([`lending::risk`]) both report
//!   [`Pass / Warn / Reject`](core::RiskCheckResult); any rejection blocks.
//! - **Async I/O.** The chain layer and protocol adapters are async (tokio).
//!   [`core`] is synchronous and pure.
//!
//! # Features
//!
//! | Feature | Adds |
//! |---------|------|
//! | `phoenix` (default) | [`protocols::phoenix`]: Phoenix Eternal perpetuals; and the bot plumbing on them |
//! | `phoenix-v1` | [`protocols::phoenix::v1`]: Phoenix v1 spot order books |
//! | `phoenix-v1-venue` | Phoenix v1's decoding, conversions and instructions alone, for `wasm32-wasip2` |
//! | `openbook-v2` | [`protocols::openbook::v2`]: OpenBook v2 spot order books |
//! | `openbook-v2-venue` | OpenBook v2's decoding, conversions and instructions alone, for `wasm32-wasip2` |
//! | `pyth` | [`protocols::pyth`]: Pyth push-oracle price feeds |
//! | `test-util` | [`chain::mock::MockChainClient`] for downstream tests |

#[cfg(feature = "chain")]
pub mod chain;
pub mod core;
#[cfg(feature = "chain")]
pub mod lending;
#[cfg(any(feature = "chain", feature = "spot-venue"))]
pub mod protocols;
pub mod raw;
mod strategy;
#[cfg(any(feature = "chain", feature = "spot-venue"))]
pub mod token;
#[cfg(feature = "venue")]
pub mod venue;

#[cfg(feature = "component")]
pub mod component;
#[cfg(feature = "phoenix")]
mod env;
#[cfg(feature = "phoenix")]
mod error;
#[cfg(feature = "phoenix")]
mod phoenix;
#[cfg(feature = "phoenix")]
pub mod run;
#[cfg(feature = "phoenix")]
mod trader;
#[cfg(feature = "phoenix")]
mod tx;

#[cfg(feature = "phoenix")]
pub use bot::*;
#[cfg(unix)]
pub use mtx_core::Turns;
pub use mtx_core::{
    Mandate, Step, mandate,
    rust_decimal::{self, Decimal},
    turns,
};
pub use raw::{RawReceipt, RawTransaction, send_raw};
/// The Solana types the kit's signatures use, so callers need not pin
/// matching versions of the Solana crates themselves.
#[cfg(any(feature = "chain", feature = "venue", feature = "spot-venue"))]
pub use solana_pubkey::Pubkey;
#[cfg(feature = "chain")]
pub use solana_transaction::{Hash, Instruction, Signature};
pub use strategy::{
    Action, AmountUnit, Book, BotResult, Execution, FeeView, Intent, LendingPosition, LineOptions,
    LiquidityPosition, LiquidityShape, MAX_TARGET_ORDERS, Market, MarketKind, OpenOrder, Orders,
    OwnFill, Planned, Position, PriceRange, RPC_METHODS, Reports, Rfq, Role, Route, SESSION_VARS,
    SendSource, SendState, SendStatus, Session, Side, Strategy, TimeInForce, TxOptions, VenueInfo,
    VenueView, log, phoenix_market_address, plan, plan_sheet, plan_sheet_with, resting_after, rpc,
    rpc_account, rpc_accounts, step_offline, venue_views,
};

#[cfg(feature = "phoenix")]
mod bot {
    pub use solana_keypair::Keypair;
    pub use solana_rpc_client::rpc_client::RpcClient;
    pub use solana_signer::Signer;

    pub use crate::{
        core::{
            Asset, ExecutionDriver, Exposure, Fill, Instrument, InstrumentId, Order, OrderBook,
            Portfolio, RiskCheck, RiskCheckResult, RiskLimits, RiskReport, Rounding, TradeContext,
            Verdict,
        },
        env::BotEnv,
        error::{Error, Result},
        phoenix::{Phoenix, QUOTE_LOTS_PER_USD, load_exchange},
        protocols::phoenix::eternal::{
            PhoenixDriver, PhoenixExchange, PhoenixMarket, PhoenixOrder, PhoenixTrader,
            phoenix_rise,
        },
        run::run,
        trader::{Sent, Top, Trader},
        tx::{rpc_client, rpc_client_with_timeout, send},
    };
}

/// What a strategy needs: `use mtx_kit::prelude::*;`.
pub mod prelude {
    pub use crate::{
        Action, BotResult, Execution, Intent, Market, Orders, Position, SendState, Session, Side,
        Strategy, TxOptions, plan, plan_sheet, rpc, step_offline,
    };
}
