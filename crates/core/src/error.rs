use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Botkit(#[from] mt_botkit::Error),
    #[error("surfnet: {0}")]
    Surfnet(#[from] surfpool_sdk::SurfnetError),
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
    #[error("invalid pubkey in exchange metadata: {0}")]
    InvalidExchangeKey(String),
    #[error("unknown market {0}")]
    UnknownMarket(String),
    #[error("transaction {label} failed: {reason}")]
    Transaction { label: String, reason: String },
    #[error("timed out waiting for {0}")]
    Timeout(String),
    #[error("invalid scenario {}: {reason}", path.display())]
    InvalidScenario { path: PathBuf, reason: String },
    #[error("no bot at {}: a bot is a directory with a run.sh", .0.display())]
    MissingBot(PathBuf),
    #[error("--jobs must be between 1 and {1}, one per bot user; got {0}")]
    Jobs(usize, usize),
    #[error(
        "the machine was too loaded to run these sessions on time, so they can't be scored: {0}; rerun on an idle machine"
    )]
    Degraded(String),
    #[error(
        "{0} is not calibrated: run `mt calibrate`, and keep the set's total reference PnL positive"
    )]
    Uncalibrated(String),
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
