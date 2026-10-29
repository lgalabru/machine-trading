//! Luce: a trading library for Solana.
//!
//! Three layers, each depending only on the ones below it:
//!
//! ```text
//!  protocols   phoenix (feature "phoenix"): a Phoenix perp as an Instrument,
//!     │        exact tick/lot math, a trader that plans orders, an ExecutionDriver
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
//! - **Providers describe; luce acts.** A protocol adapter reads state into
//!   core types and turns an intent into
//!   [`InstructionGroup`](chain::InstructionGroup)s. Packing, simulation,
//!   signing and sending happen in [`chain`], under the caller's signer.
//!   Adapters never see a key.
//! - **Simulate before signing.** [`chain::execute`] simulates every
//!   transaction right before signing it and refuses to sign one that would
//!   fail.
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
//! | `phoenix` | [`protocols::phoenix`]: Phoenix Eternal perpetuals |
//! | `test-util` | [`chain::mock::MockChainClient`] for downstream tests |

pub mod chain;
pub mod core;
pub mod lending;
pub mod protocols;

pub use rust_decimal::{self, Decimal};
