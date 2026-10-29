//! The Solana runtime: everything that touches a node or a key.
//!
//! | Concern | Contract | Implementations |
//! |---------|----------|-----------------|
//! | Reading chain state, sending transactions | [`ChainClient`] | [`RpcChainClient`]; [`mock::MockChainClient`] (tests, feature `test-util`) |
//! | Producing signatures | [`TransactionSigner`] | [`Keypair`](solana_keypair::Keypair) |
//! | Building transactions | [`InstructionGroup`] → [`pack`] → [`ExecutionPlan`] | |
//! | Running them | [`preview`], [`execute`] | simulate before every signature |
//! | Tokens | [`token`] | ATAs, SOL wrapping, balances |
//!
//! # Async
//!
//! This layer is async: [`ChainClient`] wraps the non-blocking RPC client,
//! and signing is async so remote signers fit. Protocol SDKs this crate
//! builds on (Phoenix's transaction builder and Hawkeye views) are async
//! too, so one tokio runtime drives everything. Callers that need a blocking
//! path (a grader setting up a chain before any bot runs) can still use
//! [`TransactionPlan`] to build transactions and send them with the blocking
//! RPC client themselves.

pub mod client;
pub mod error;
#[cfg(any(test, feature = "test-util"))]
pub mod mock;
pub mod signer;
pub mod token;
pub mod tx;

pub use client::{
    ChainClient, RpcChainClient, SimulationOutcome, TokenBalanceChange, TransactionSummary,
};
pub use error::{ChainError, PlanError, SignerError};
pub use signer::{TransactionSigner, is_fully_signed, read_keypair_file};
pub use tx::{
    DEFAULT_COMPUTE_UNIT_LIMIT, ExecuteOptions, ExecutionPlan, ExecutionReceipt, InstructionGroup,
    MAX_TRANSACTION_SIZE, StepPreview, TransactionPlan, execute, pack, preview,
};
