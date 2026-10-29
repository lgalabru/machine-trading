use std::path::PathBuf;

use thiserror::Error;

use crate::{
    chain::{ChainError, PlanError},
    core::InstrumentError,
};

/// Errors from the Phoenix adapter.
#[derive(Debug, Error)]
pub enum PhoenixError {
    /// Chain access or plan execution failed.
    #[error(transparent)]
    Chain(#[from] ChainError),
    /// Instructions could not be packed.
    #[error(transparent)]
    Plan(#[from] PlanError),
    /// A price or quantity is off the market's grid, or the market metadata
    /// does not describe a valid instrument.
    #[error(transparent)]
    Instrument(#[from] InstrumentError),
    /// The order cannot be expressed on Phoenix.
    #[error("invalid order: {0}")]
    InvalidOrder(String),
    /// Phoenix's instruction layer refused the order ticket.
    #[error("phoenix instruction: {0}")]
    Ix(#[from] phoenix_rise::ix::PhoenixIxError),
    /// Phoenix's transaction builder failed.
    #[error("phoenix tx builder: {0}")]
    TxBuilder(#[from] phoenix_rise::core::PhoenixTxBuilderError),
    /// A Hawkeye (read-only, simulated) view failed.
    #[error("hawkeye: {0}")]
    Hawkeye(#[from] phoenix_rise::core::PhoenixHawkeyeClientError),
    /// The exchange metadata has no market with this symbol.
    #[error("unknown market {0}")]
    UnknownMarket(String),
    /// The exchange metadata carries a malformed address.
    #[error("invalid pubkey in exchange metadata: {0}")]
    InvalidExchangeKey(String),
    /// The exchange metadata carries a number this crate cannot represent.
    #[error("invalid {what} in exchange metadata: {value}")]
    InvalidExchangeValue { what: &'static str, value: String },
    /// Reading exchange metadata failed.
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Exchange metadata is not valid JSON.
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}
