pub type Result<T> = std::result::Result<T, Error>;

/// What a host's own work can fail with. An embedder with an error type of
/// its own maps each variant onto it (the grader does).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Kit(#[from] mtx_kit::Error),
    #[error(transparent)]
    Core(#[from] mtx_core::Error),
    #[error("rpc: {0}")]
    Rpc(#[from] Box<solana_rpc_client_api::client_error::Error>),
    #[error("phoenix instruction: {0}")]
    Ix(#[from] phoenix_rise::ix::PhoenixIxError),
    #[error("phoenix tx builder: {0}")]
    TxBuilder(#[from] phoenix_rise::core::PhoenixTxBuilderError),
    #[error("hawkeye: {0}")]
    Hawkeye(#[from] phoenix_rise::core::PhoenixHawkeyeClientError),
    #[error("meter: {0}")]
    Meter(#[from] mtx_core::MeterError),
    /// A component that didn't load: not a component, not of a world the
    /// host knows, or past the host's limits.
    #[error("wasm strategy {0}")]
    Wasm(String),
    #[error("invalid pubkey in exchange metadata: {0}")]
    InvalidExchangeKey(String),
    #[error("unknown market {0}")]
    UnknownMarket(String),
    #[error("transaction {label} failed: {reason}")]
    Transaction { label: String, reason: String },
}

impl From<solana_rpc_client_api::client_error::Error> for Error {
    fn from(error: solana_rpc_client_api::client_error::Error) -> Self {
        Self::Rpc(Box::new(error))
    }
}
