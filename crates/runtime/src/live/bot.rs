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

/// The reads that make a node list accounts by their owner. Without an
/// account index it reads every account for each one: on mtx-alpha, 55
/// minutes (2026-11-24, a strategy's `getTokenAccountsByOwner`).
pub const SCANS: [&str; 2] = ["getProgramAccounts", "getTokenAccountsByOwner"];

/// A strategy's read may take this long where its step has no deadline:
/// a slot, past which what it read is stale.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(400);

/// A hosted strategy's own reads of the node (its `rpc` import), on a
/// chain that doesn't wait for it:
///
/// - each ends within the step's deadline, answered or not. A step's
///   deadline can't interrupt a read the host is making for it, so a read
///   the node never answers held the step for as long: an hour, once.
/// - a read that lists accounts by owner ([`SCANS`]) is refused unless
///   the node is said to have an index for it (`control.rpc_scans`):
///   giving up on it wouldn't stop the node from scanning.
pub struct LiveReads {
    inner: HttpReads,
    timeout: std::time::Duration,
    scans: bool,
}

impl LiveReads {
    /// Reads of the node at `url`, each within `deadline` (a slot without
    /// one), with `scans` allowed or not.
    pub fn boxed(
        url: &str,
        deadline: Option<std::time::Duration>,
        scans: bool,
    ) -> Box<dyn crate::host::Reads> {
        let timeout = deadline.unwrap_or(READ_TIMEOUT);
        assert!(!timeout.is_zero(), "a read takes some time");
        Box::new(Self {
            inner: HttpReads::new(url),
            timeout,
            scans,
        })
    }
}

#[async_trait]
impl crate::host::Reads for LiveReads {
    async fn rpc(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        if !self.scans && SCANS.contains(&method) {
            tracing::warn!(
                counter.mtx_strategy_reads_refused = 1_u64,
                method,
                "a strategy's read would scan the node's accounts; refused"
            );
            return Err(format!(
                "{method} is refused here: this node has no account index, and would read every \
                 account to answer"
            ));
        }
        match tokio::time::timeout(self.timeout, self.inner.rpc(method, params)).await {
            Ok(answer) => answer,
            Err(_) => {
                tracing::warn!(
                    counter.mtx_strategy_reads_late = 1_u64,
                    method,
                    "a strategy's read wasn't answered in time"
                );
                Err(format!(
                    "{method} wasn't answered within {} ms",
                    self.timeout.as_millis()
                ))
            }
        }
    }
}

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
    /// What answers its own reads of the node: [`LiveReads`] on a chain
    /// that doesn't wait for it, the node itself in a session.
    pub reads: Box<dyn crate::host::Reads>,
}

/// Everything a strategy on pools is started with (`live::pooldesk`).
pub struct PoolBotSpec<'a> {
    pub component: &'a Path,
    /// The node its own reads go to.
    pub rpc_url: &'a str,
    pub keypair: &'a Keypair,
    /// What it counts in.
    pub symbol: &'a str,
    pub mandate: &'a Mandate,
    pub clock: Clock,
    pub end_slot: u64,
    /// What it holds on paper, and its drawdown limit.
    pub cap: f64,
    pub max_drawdown: f64,
    pub pools: &'a [crate::venue::amm::Pool],
    /// The wallet it starts with.
    pub wallet: mtx_core::session::Wallet,
    pub log: Box<dyn Write + Send>,
    /// The chain its routes go through: a shadow's, which sends nothing.
    pub sends: Arc<dyn mtx_kit::chain::ChainClient>,
    pub observer: Option<Arc<dyn crate::desk::StepObserver>>,
    pub references: Option<Arc<super::reference::Latest>>,
    /// What answers its own reads of the node: [`LiveReads`] on a chain
    /// that doesn't wait for it, the node itself in a session.
    pub reads: Box<dyn crate::host::Reads>,
}

/// Load a pool strategy's component and call its `start`: the desk a
/// session builds for the bench's atomic arbitrage tasks, a seat on each
/// pool and a wallet the host measures.
pub async fn start_pools(spec: PoolBotSpec<'_>) -> Result<WasmBot, String> {
    use crate::venue::amm::PoolSeat;
    let authority = solana_signer::Signer::pubkey(spec.keypair);
    // Pooled: a send reports its signature at once, and nothing waits on
    // a chain that will never see it.
    let seats: Vec<Box<dyn Seat>> = spec
        .pools
        .iter()
        .map(|pool| {
            let seat = PoolSeat::new(
                pool.clone(),
                spec.sends.clone(),
                spec.keypair.insecure_clone(),
            );
            Box::new(seat.pooled(true)) as Box<dyn Seat>
        })
        .collect();
    let markets = spec
        .pools
        .iter()
        .map(|pool| mtx_kit::VenueInfo {
            id: pool.instrument.to_string(),
            kind: pool.instrument.kind.into(),
            address: Some(pool.address.to_string()),
            tradable: true,
        })
        .collect();
    WasmBot::<NoRawSends>::start(
        spec.component,
        spec.reads,
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
            venue: super::pooldesk::metadata(spec.pools),
            authority: authority.to_string(),
            reader: Reader::Lending { start: spec.wallet },
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
        spec.reads,
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn a_read_that_would_scan_the_node_is_refused_before_it_is_sent() {
        // Nothing listens there: a refusal that reached the node would say
        // so instead.
        let mut reads = LiveReads::boxed(
            "http://127.0.0.1:9",
            Some(Duration::from_millis(200)),
            false,
        );
        for method in SCANS {
            let refused = reads.rpc(method, serde_json::json!([])).await.unwrap_err();
            assert!(refused.contains("no account index"), "{refused}");
        }
    }

    #[tokio::test]
    async fn a_read_the_node_never_answers_ends_at_the_deadline() {
        // A listener that accepts and says nothing, as a node deep in a
        // scan does.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let held = std::thread::spawn(move || {
            let accepted = listener.accept();
            std::thread::sleep(Duration::from_millis(600));
            drop(accepted);
        });
        let started = std::time::Instant::now();
        let mut reads = LiveReads::boxed(&url, Some(Duration::from_millis(100)), true);
        let late = reads
            .rpc("getSlot", serde_json::json!([]))
            .await
            .unwrap_err();
        assert!(late.contains("within 100 ms"), "{late}");
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "{:?}",
            started.elapsed()
        );
        // With scans allowed, a listing is sent like any read, and ends
        // the same way.
        let late = reads
            .rpc(SCANS[0], serde_json::json!([]))
            .await
            .unwrap_err();
        assert!(late.contains("within 100 ms"), "{late}");
        held.join().unwrap();
    }
}
