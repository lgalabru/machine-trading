//! The Solana runtime: everything that touches a node or a key.
//!
//! | Concern | Contract | Implementations |
//! |---------|----------|-----------------|
//! | Reading chain state, sending transactions | [`ChainClient`] | [`RpcChainClient`]; [`mock::MockChainClient`] (tests, feature `test-util`) |
//! | Producing signatures | [`TransactionSigner`] | [`Keypair`](solana_keypair::Keypair) |
//! | Building transactions | [`InstructionGroup`] → [`pack`] → [`ExecutionPlan`] | |
//! | Running them | [`preview`], [`execute`] | simulate before every signature |
//! | Sending without waiting | [`fire_and_forget`], [`fire_and_forget_step`] | for worlds that land a transaction after the sender's turn |
//! | Tokens | [`token`] | ATAs, SOL wrapping, balances, under SPL Token or Token-2022 |
//! | Lookup tables | [`lookup`] | reading, creating and extending them, for v0 transactions |
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
pub mod lookup;
#[cfg(any(test, feature = "test-util"))]
pub mod mock;
pub mod signer;
pub mod token;
pub mod tx;

pub use client::{
    ChainClient, ExecutedInstruction, InnerInstruction, RpcChainClient, SimulationOutcome,
    TokenBalanceChange, TransactionSummary,
};
pub use error::{ChainError, PlanError, SignerError};
pub use signer::{TransactionSigner, is_fully_signed, read_keypair_file};
pub use tx::{
    AddressLookupTableAccount, DEFAULT_COMPUTE_UNIT_LIMIT, ExecuteOptions, ExecutionPlan,
    ExecutionReceipt, FireAndForgetOptions, InstructionGroup, MAX_TRANSACTION_SIZE, SendMode,
    StepPreview, TransactionPlan, execute, fire_and_forget, fire_and_forget_step, pack,
    pack_with_lookup_tables, preview,
};
