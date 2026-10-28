use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("rpc: {0}")]
    Rpc(#[from] Box<solana_rpc_client_api::client_error::Error>),
    #[error("phoenix api: {0}")]
    PhoenixApi(#[from] phoenix_rise::api::PhoenixHttpError),
    #[error("phoenix instruction: {0}")]
    Ix(#[from] phoenix_rise::ix::PhoenixIxError),
    #[error("phoenix tx builder: {0}")]
    TxBuilder(#[from] phoenix_rise::core::PhoenixTxBuilderError),
    #[error("hawkeye: {0}")]
    Hawkeye(#[from] phoenix_rise::core::PhoenixHawkeyeClientError),
    #[error("{}: {source}", path.display())]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("environment variable {name}: {reason}")]
    Env { name: &'static str, reason: String },
    #[error("invalid keypair file {}", .0.display())]
    Keypair(PathBuf),
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

pub(crate) fn io_error(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.into();
    move |source| Error::Io { path, source }
}
