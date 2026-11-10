use std::path::PathBuf;

use crate::{chain::ChainError, protocols::phoenix::PhoenixError};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("rpc: {0}")]
    Rpc(#[from] Box<solana_rpc_client_api::client_error::Error>),
    #[error("phoenix api: {0}")]
    PhoenixApi(#[from] crate::protocols::phoenix::phoenix_rise::api::PhoenixHttpError),
    #[error("phoenix instruction: {0}")]
    Ix(#[from] crate::protocols::phoenix::phoenix_rise::ix::PhoenixIxError),
    #[error("phoenix tx builder: {0}")]
    TxBuilder(#[from] crate::protocols::phoenix::phoenix_rise::core::PhoenixTxBuilderError),
    #[error("hawkeye: {0}")]
    Hawkeye(#[from] crate::protocols::phoenix::phoenix_rise::core::PhoenixHawkeyeClientError),
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("environment variable {name}: {reason}")]
    Env { name: &'static str, reason: String },
    #[error("mandate {}: {reason}", path.display())]
    Mandate { path: PathBuf, reason: String },
    #[error("account {account}: {reason}")]
    Account { account: String, reason: String },
    #[error("invalid keypair file {}", .0.display())]
    Keypair(PathBuf),
    #[error("invalid pubkey in exchange metadata: {0}")]
    InvalidExchangeKey(String),
    #[error("unknown market {0}")]
    UnknownMarket(String),
    #[error("transaction {label} failed: {reason}")]
    Transaction { label: String, reason: String },
    /// An order the kit refused before building it: off the tick or lot grid,
    /// zero lots, or a price Phoenix cannot take.
    #[error("invalid order: {0}")]
    InvalidOrder(String),
    /// Anything else from the kit's Phoenix adapter.
    #[error(transparent)]
    Luce(PhoenixError),
}

/// `mtx-core`'s errors, as the same variants here.
impl From<mtx_core::Error> for Error {
    fn from(error: mtx_core::Error) -> Self {
        match error {
            mtx_core::Error::Io { path, source } => Self::Io { path, source },
            mtx_core::Error::Json(e) => Self::Json(e),
            mtx_core::Error::Env { name, reason } => Self::Env { name, reason },
            mtx_core::Error::Mandate { path, reason } => Self::Mandate { path, reason },
        }
    }
}

impl From<solana_rpc_client_api::client_error::Error> for Error {
    fn from(error: solana_rpc_client_api::client_error::Error) -> Self {
        Self::Rpc(Box::new(error))
    }
}

/// Keeps the error shapes bots saw before the bot plumbing ran on the kit's adapter: Phoenix SDK
/// errors keep their variants, and a transaction that fails its pre-flight
/// simulation is a [`Error::Transaction`] with the tail of its logs.
impl From<PhoenixError> for Error {
    fn from(error: PhoenixError) -> Self {
        match error {
            PhoenixError::Ix(e) => Self::Ix(e),
            PhoenixError::TxBuilder(e) => Self::TxBuilder(e),
            PhoenixError::Hawkeye(e) => Self::Hawkeye(e),
            PhoenixError::UnknownMarket(symbol) => Self::UnknownMarket(symbol),
            PhoenixError::InvalidExchangeKey(key) => Self::InvalidExchangeKey(key),
            PhoenixError::Io { path, source } => Self::Io { path, source },
            PhoenixError::Json(e) => Self::Json(e),
            PhoenixError::InvalidOrder(reason) => Self::InvalidOrder(reason),
            PhoenixError::Instrument(e) => Self::InvalidOrder(e.to_string()),
            PhoenixError::Chain(ChainError::Simulation { label, error, logs }) => {
                let tail = logs[logs.len().saturating_sub(12)..].join("\n");
                Self::Transaction {
                    label,
                    reason: format!("{error}\n{tail}"),
                }
            }
            PhoenixError::Chain(ChainError::Failed {
                label,
                signature,
                error,
            }) => Self::Transaction {
                label,
                reason: format!("{signature} failed on-chain: {error}"),
            },
            other => Self::Luce(other),
        }
    }
}

pub(crate) fn io_error(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.into();
    move |source| Error::Io { path, source }
}
