//! A strategy component as the live runtime hosts it: the desk the grader
//! would build for it on a Phoenix perp, with its market read from the
//! account cache rather than through Hawkeye's views. Sessions a grader
//! hands the runtime ([`super::session`]) and the strategies it runs on its
//! own ([`super::strategies`]) both start theirs here.

use std::{
    io::Write,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use mtx_core::Mandate;
use mtx_kit::venue::PhoenixVenue;
use solana_keypair::Keypair;

use super::{
    feed::AccountCache,
    perp::{PerpReader, PerpReading, ReaderKey},
};
use crate::{
    desk::{Clock, Desk, PerpReads, Reader, Terms, WasmBot},
    host::{HttpReads, NoRawSends},
    venue::{
        Market as _, Seat,
        perps::{PerpMarket, PerpSeat},
    },
};

/// Everything a hosted strategy is started with.
pub struct BotSpec<'a> {
    pub component: &'a Path,
    /// The node its sends and its own reads go to.
    pub rpc_url: &'a str,
    pub keypair: &'a Keypair,
    pub exchange_path: &'a Path,
    pub symbol: &'a str,
    pub mandate: &'a Mandate,
    pub clock: Clock,
    pub end_slot: u64,
    /// The position cap on its market, and its drawdown limit.
    pub cap: f64,
    pub max_drawdown: f64,
    /// How its market is read, from `cache`, at the slot `slot` holds.
    pub reader: PerpReader,
    pub cache: Arc<Mutex<AccountCache>>,
    pub slot: Arc<AtomicU64>,
    /// Readings shared across the runtime's strategies.
    pub readings: Arc<Readings>,
    /// Where its prints go.
    pub log: Box<dyn Write + Send>,
    /// The chain its orders go through, if not the node's RPC at
    /// `rpc_url`: the leaders' TPU ([`super::tpu::TpuChainClient`]).
    pub sends: Option<Arc<dyn mtx_kit::chain::ChainClient>>,
    /// Watches each step: the strategy's journals ([`super::journal`]).
    pub observer: Option<Arc<dyn crate::desk::StepObserver>>,
    /// A shadow's paper position, read as its own (the chain's is flat:
    /// it sends nothing). `None` for a live strategy.
    pub paper_position: Option<Arc<super::journal::PaperPosition>>,
    /// The exchanges' prices it reads as venues, if the runtime has any.
    pub references: Option<Arc<super::reference::Latest>>,
    /// Whether an order waits to land before the step goes on. Through
    /// `sends`, a live strategy fires and forgets: a step can't wait on
    /// each of its orders.
    pub dispatch: mtx_kit::protocols::phoenix::eternal::Dispatch,
}

/// Load the component and call its `start`.
pub async fn start(spec: BotSpec<'_>) -> Result<WasmBot, String> {
    let exchange_json = std::fs::read_to_string(spec.exchange_path)
        .map_err(|e| format!("{}: {e}", spec.exchange_path.display()))?;
    let venue = PhoenixVenue::from_json(&exchange_json)?;
    let authority = solana_signer::Signer::pubkey(spec.keypair);
    if spec.reader.trader() != venue.trader_account(&authority).to_bytes() {
        return Err(format!(
            "{} is read for another account than {authority}",
            spec.symbol
        ));
    }
    let market = PerpMarket::new(
        venue.exchange().clone(),
        spec.exchange_path.to_path_buf(),
        spec.symbol,
        spec.rpc_url,
        authority,
    )
    .map_err(|e| e.to_string())?;
    let trader = market
        .trader(spec.keypair.insecure_clone(), spec.rpc_url)
        .map_err(|e| e.to_string())?;
    // A seat whose sends go through a chain put in place of the RPC sends
    // every order through it, its cancels included: a shadow strategy must
    // send nothing, and a TPU one nothing by RPC.
    let pooled = spec.sends.is_some();
    let trader = Arc::new(match spec.sends {
        Some(chain) => mtx_kit::Trader {
            inner: trader.inner.with_chain(chain).with_dispatch(spec.dispatch),
            ..trader
        },
        None => trader,
    });
    let reads: Arc<dyn PerpReads> = Arc::new(CacheReads {
        reader: spec.reader,
        cache: spec.cache,
        slot: spec.slot,
        readings: spec.readings,
        last: Mutex::new(None),
        paper_position: spec.paper_position,
    });
    // Post-only orders are checked against the cache's book: the trader's
    // own read simulates against its account, which a shadow strategy's
    // key may not have yet.
    let seats: Vec<Box<dyn Seat>> = vec![Box::new(
        PerpSeat::new(market.instrument().to_string(), trader)
            .pooled(pooled)
            .reading_top(reads.clone()),
    )];
    let markets = mtx_kit::VenueInfo::of_perp_markets(
        spec.mandate,
        [(
            market.instrument().clone(),
            Some(market.address().to_string()),
        )],
    );
    WasmBot::<NoRawSends>::start(
        spec.component,
        HttpReads::boxed(spec.rpc_url),
        spec.clock,
        Terms {
            end_slot: spec.end_slot,
            mandate: spec.mandate,
            cap: spec.cap,
            max_drawdown: spec.max_drawdown,
        },
        Desk {
            markets,
            seats,
            symbol: spec.symbol.to_string(),
            venue: exchange_json,
            authority: authority.to_string(),
            reader: Reader::Live(reads),
            sends: None,
            rfqs: None,
            references: spec.references.map(|latest| {
                Arc::new(super::reference::References(latest)) as Arc<dyn crate::desk::References>
            }),
        },
        spec.log,
        spec.observer,
        None,
    )
    .await
    .map_err(|e| e.to_string())
}

/// A hosted strategy's market, as the account cache holds it at the slot
/// of its step.
struct CacheReads {
    reader: PerpReader,
    cache: Arc<Mutex<AccountCache>>,
    slot: Arc<AtomicU64>,
    /// Every strategy's latest readings, shared.
    readings: Arc<Readings>,
    /// The step's reading, by slot: a step asks for the top, the oracle and
    /// the position (and a post-only order for the top again), and decoding
    /// the market is most of a read's cost (hundreds of microseconds on
    /// Phoenix's SOL book). One decode a step, every answer from the same
    /// snapshot.
    last: Mutex<Option<(u64, PerpReading)>>,
    /// A shadow's paper position, read in place of the chain's.
    paper_position: Option<Arc<super::journal::PaperPosition>>,
}

impl CacheReads {
    fn read(&self) -> Result<PerpReading, String> {
        let slot = self.slot.load(Ordering::Acquire);
        // Poisoned: the last reading is not trusted, and read again.
        if let Ok(last) = self.last.lock()
            && let Some((at, reading)) = *last
            && at == slot
        {
            return Ok(reading);
        }
        let key = self.reader.key();
        if let Some(reading) = self.readings.get(key, slot) {
            return self.keep(slot, reading);
        }
        // The accounts under the lock, a reference count each; the decode
        // after letting go of it.
        let waited = std::time::Instant::now();
        let snapshot = self
            .cache
            .lock()
            .map_err(|_| "the account cache was poisoned".to_string())?
            .snapshot(&self.reader.addresses());
        let decoding = std::time::Instant::now();
        let reading = self.reader.read(&snapshot, slot)?;
        tracing::info!(
            histogram.mtx_read_lock_us =
                u64::try_from(decoding.duration_since(waited).as_micros()).unwrap_or(u64::MAX),
            histogram.mtx_read_decode_us =
                u64::try_from(decoding.elapsed().as_micros()).unwrap_or(u64::MAX),
            symbol = self.reader.symbol(),
            "a market read"
        );
        self.readings.put(key, slot, reading);
        self.keep(slot, reading)
    }

    fn keep(&self, slot: u64, reading: PerpReading) -> Result<PerpReading, String> {
        // Poisoned: not kept, and the next call reads again.
        if let Ok(mut last) = self.last.lock() {
            *last = Some((slot, reading));
        }
        Ok(reading)
    }
}

/// The readings of every market the runtime reads, by reader and slot:
/// strategies that read one market for one account at one slot (two
/// strategies sharing a key) decode it once between them.
#[derive(Debug, Default)]
pub struct Readings {
    latest: Mutex<std::collections::HashMap<ReaderKey, (u64, PerpReading)>>,
}

/// Readings kept, one a reader: the strategies a runtime runs
/// (`strategies::STRATEGIES_MAX`, 32) with room for keys that came and went.
/// Past it, every reading older than the one put is forgotten: a reading is
/// only ever shared within its slot.
const READINGS_MAX: usize = 256;

impl Readings {
    fn get(&self, key: ReaderKey, slot: u64) -> Option<PerpReading> {
        // Poisoned: no shared reading, and the reader decodes its own.
        let latest = self.latest.lock().ok()?;
        latest
            .get(&key)
            .filter(|(at, _)| *at == slot)
            .map(|(_, reading)| *reading)
    }

    fn put(&self, key: ReaderKey, slot: u64, reading: PerpReading) {
        // Poisoned: nothing shared, as in `get`.
        let Ok(mut latest) = self.latest.lock() else {
            return;
        };
        if latest.len() >= READINGS_MAX {
            latest.retain(|_, (at, _)| *at >= slot);
        }
        let newer = latest.get(&key).is_none_or(|(at, _)| slot >= *at);
        if newer {
            latest.insert(key, (slot, reading));
        }
        debug_assert!(
            latest.len() <= READINGS_MAX,
            "no more readers than the bound read in one slot"
        );
    }
}

#[async_trait]
impl PerpReads for CacheReads {
    async fn top(&self) -> Result<mtx_kit::Top, String> {
        let reading = self.read()?;
        Ok(mtx_kit::Top {
            mark: reading.mark,
            index: reading.index,
            best_bid: reading.best_bid,
            best_ask: reading.best_ask,
        })
    }

    async fn oracle_usd(&self) -> Result<f64, String> {
        Ok(self.read()?.oracle)
    }

    async fn position(&self) -> Result<mtx_kit::Position, String> {
        if let Some(paper) = &self.paper_position {
            return Ok(paper.get());
        }
        let reading = self.read()?;
        Ok(mtx_kit::Position {
            size: reading.position,
            entry_usd: reading.entry_usd,
        })
    }
}
