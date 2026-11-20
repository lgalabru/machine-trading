//! A strategy component in a grade (`docs/rfd/0002-wasm-strategies.md`): a
//! bot directory with a `bot.wasm` is a component of the `mtx:strategy`
//! world, which the grader calls instead of starting a process. The host
//! and the desk that plays a session are `mtx_runtime`'s; this is what a
//! grade hands them: the bot's guarded endpoint for its reads, the
//! transaction layer's pool for its sends, the clients' requests for
//! quotes, and the session's raw sender.

use std::{io::Write, path::Path, sync::Arc};

use async_trait::async_trait;
use mtx_kit::SendStatus;
use mtx_runtime::desk::{RawDesk, Rfqs, Sends};
pub use mtx_runtime::{
    desk::{Account, Clock, Reader, StepObserver, StepTrace, Terms, VenueReading, Wallet},
    host::{
        BOT_WASM, FUEL_PER_CALL, FUEL_PER_STEP, HttpReads, MEMORY_BYTES, Outcome, component_in,
    },
};
use solana_keypair::Keypair;

use crate::{
    actors::library::RfqBoard,
    raw::{RawConfig, RawSender},
    txlayer::{TxEngine, client::PoolClient, engine::Fate},
};

/// A strategy component whose raw sends the session's sender takes.
pub type WasmStrategy = mtx_runtime::host::WasmStrategy<RawSender>;

/// A component playing one session of a grade.
pub type WasmBot = mtx_runtime::desk::WasmBot<RawSender>;

/// What the grader trades and reads for a component: a seat on each market
/// the session trades, with the bot's key, through the bot's guarded
/// endpoint, and how its market is read.
pub struct Desk {
    /// The session's markets, the bot's own first, as `start` lists them
    /// (the interface's `venues`).
    pub markets: Vec<mtx_kit::VenueInfo>,
    /// A seat on each market the session trades. A line for a market
    /// without one comes back not sent.
    pub seats: Vec<Box<dyn crate::venue::Seat>>,
    /// The market traded, as the venue names it, and the venue's metadata,
    /// as `start` gets them.
    pub symbol: String,
    pub venue: String,
    /// The bot's key, base58.
    pub authority: String,
    pub reader: Reader,
    /// Under the transaction layer: the chain the seats send through, which
    /// takes each line's transaction options, and the layer itself. Both or
    /// neither.
    pub pool: Option<Arc<PoolClient>>,
    pub tx: Option<Arc<TxEngine>>,
    /// The requests for quotes open to the bot, where clients send any
    /// (see [`crate::actors::library::rfq`]).
    pub rfqs: Option<RfqBoard>,
}

/// Loads the component at `path` and calls its `start`. Its own `rpc` reads
/// go to `rpc_url`, the bot's guarded endpoint; `raw` enables its raw
/// sends.
#[allow(clippy::too_many_arguments)]
pub async fn start(
    path: &Path,
    rpc_url: &str,
    clock: Clock,
    terms: Terms<'_>,
    desk: Desk,
    log: Box<dyn Write + Send>,
    observer: Option<Arc<dyn StepObserver>>,
    raw: Option<RawConfig>,
) -> crate::Result<WasmBot> {
    assert_eq!(
        desk.pool.is_some(),
        desk.tx.is_some(),
        "a desk under the transaction layer has its pool and its engine"
    );
    // Raw sends are a perp session's: a scenario enables them nowhere
    // else. They go out as the sheet's do, through the desk's trader.
    let raw = match (raw, &desk.reader) {
        (Some(raw), Reader::Perp(trader)) => {
            let sender = RawSender::new(
                raw,
                trader.authority(),
                trader.env.exchange()?,
                &trader.env.symbol,
                &trader.env.rpc_url,
            );
            Some(match &desk.pool {
                Some(pool) => sender.with_pool(pool.clone()),
                None => sender,
            })
        }
        _ => None,
    };
    let sends: Option<Arc<dyn Sends>> = match (desk.pool, desk.tx) {
        (Some(pool), Some(engine)) => Some(Arc::new(LayerSends { pool, engine })),
        _ => None,
    };
    let desk = mtx_runtime::desk::Desk {
        markets: desk.markets,
        seats: desk.seats,
        symbol: desk.symbol,
        venue: desk.venue,
        authority: desk.authority,
        reader: desk.reader,
        sends,
        rfqs: desk.rfqs.map(|board| Arc::new(board) as Arc<dyn Rfqs>),
    };
    Ok(WasmBot::start(
        path,
        HttpReads::boxed(rpc_url),
        clock,
        terms,
        desk,
        log,
        observer,
        raw,
    )
    .await?)
}

/// The transaction layer, as a desk's sends see it: the pool the seats
/// send into, and the engine that lands them.
struct LayerSends {
    pool: Arc<PoolClient>,
    engine: Arc<TxEngine>,
}

#[async_trait]
impl Sends for LayerSends {
    fn set_options(&self, options: Option<mtx_kit::TxOptions>) {
        self.pool.set_options(options);
    }

    async fn flush_bundles(&self) -> Vec<(Vec<String>, Option<String>)> {
        self.pool.flush_bundles().await
    }

    fn fate(&self, signature: &str) -> Option<Fate> {
        self.engine.fate(signature)
    }

    fn fee_view(&self) -> mtx_kit::FeeView {
        self.engine.fee_view()
    }
}

impl Rfqs for RfqBoard {
    fn open(&self) -> Vec<mtx_kit::Rfq> {
        RfqBoard::open(self)
    }

    fn take_fills(&self) -> Vec<mtx_kit::OwnFill> {
        RfqBoard::take_fills(self)
    }
}

impl RawDesk for RawSender {
    async fn begin(&mut self, k: u64, account: Option<Account>) {
        RawSender::begin(self, k, account).await;
    }

    fn close(&mut self) {
        RawSender::close(self);
    }

    fn discard(&mut self, why: &str) -> Vec<SendStatus> {
        RawSender::discard(self, why)
    }

    async fn flush(&mut self, signer: &Keypair) -> Vec<SendStatus> {
        RawSender::flush(self, signer).await
    }

    fn take_pooled(&mut self) -> Vec<(u32, String)> {
        RawSender::take_pooled(self)
            .into_iter()
            .map(|(id, signature)| (id, signature.to_string()))
            .collect()
    }
}
