use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Kit(#[from] mtx_kit::Error),
    #[error(transparent)]
    Core(#[from] mtx_core::Error),
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
    #[error("wasm strategy {0}")]
    Wasm(String),
    #[error("meter: {0}")]
    Meter(#[from] mtx_core::MeterError),
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
    /// The snapshot has the market's address but not its makers and oracles:
    /// a scenario's moves wouldn't reach it.
    #[error(
        "snapshot {snapshot} can't move {symbol}: its warm-up traded {held} only. Play the scenario on a snapshot whose recipe warms {symbol} up"
    )]
    NotWarmed {
        snapshot: String,
        symbol: String,
        held: String,
    },
    #[error("transaction {label} failed: {reason}")]
    Transaction { label: String, reason: String },
    #[error("spline collection {account}: {reason}")]
    Splines { account: String, reason: String },
    #[error("stop-out: {0}")]
    StopOut(String),
    #[error("timed out waiting for {0}")]
    Timeout(String),
    /// The transaction layer couldn't start or read the chain.
    #[error("transaction layer: {0}")]
    TxLayer(String),
    /// A session failed before its bot started.
    #[error(transparent)]
    Setup(Box<Error>),
    /// A perp scenario's starting position didn't take: Phoenix refused it,
    /// or the bot doesn't hold it, or holding it is past what the
    /// collateral margins. The scenario's, not the machine's: not retried.
    #[error("the starting position: {0}")]
    StartPosition(String),
    /// A session's setup produced more slots than its scenario leaves it: a
    /// setup that changed, not a slow machine (setup runs on no clock).
    #[error("{scenario}'s setup took {used} slots, past the {budget} its scenario leaves it")]
    SetupSlots {
        scenario: String,
        used: u64,
        budget: u64,
    },
    /// Two runs of the same grade disagreed (see [`crate::replay`]).
    #[error("the grade didn't replay: {0}")]
    Replay(String),
    /// Scoring v2 couldn't score a grade's sessions (see [`crate::scoring_v2`]).
    #[error("scoring v2: {0}")]
    ScoringV2(String),
    #[error("invalid scenario {}: {reason}", path.display())]
    InvalidScenario { path: PathBuf, reason: String },
    #[error("invalid market calibration {}: {reason}", path.display())]
    InvalidCalibration { path: PathBuf, reason: String },
    #[error("oracle {name}: {reason}")]
    Oracle { name: String, reason: String },
    #[error("no bot at {}: a bot is a directory with a bot.wasm, or a Justfile whose `run` starts it", .0.display())]
    MissingBot(PathBuf),
    #[error("no bot.wasm in {}: this grade takes strategy components only, built into bot.wasm (`just build`)", .0.display())]
    NotAComponent(PathBuf),
    #[error("the bot has a Justfile, and `just` isn't on the grader's PATH")]
    NoJust,
    #[error(
        "--jobs must be between 1 and {1} (a process bot's sessions take one bot user each); got {0}"
    )]
    Jobs(usize, usize),
    #[error("a process bot's session needs one of the bot users to run as")]
    NoBotUser,
    #[error(
        "bot users {first} to {} don't exist: there are {users}; lower --first-bot-user or --jobs",
        first + jobs - 1
    )]
    BotUsers {
        first: usize,
        jobs: usize,
        users: usize,
    },
    #[error(
        "the machine was too loaded to run these sessions on time, so they can't be scored: {0}; rerun on an idle machine"
    )]
    Degraded(String),
    #[error(
        "{0} is not calibrated: run `mtx calibrate`, and keep the set's total reference PnL positive"
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
