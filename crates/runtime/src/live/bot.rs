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
    perp::{PerpReader, PerpReading},
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
    /// Where its prints go.
    pub log: Box<dyn Write + Send>,
    /// The chain its orders go through, if not the node's RPC at
    /// `rpc_url`: the leaders' TPU ([`super::tpu::TpuChainClient`]).
    pub sends: Option<Arc<dyn mtx_kit::chain::ChainClient>>,
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
    let trader = Arc::new(match spec.sends {
        Some(chain) => mtx_kit::Trader {
            inner: trader.inner.with_chain(chain),
            ..trader
        },
        None => trader,
    });
    let seats: Vec<Box<dyn Seat>> = vec![Box::new(PerpSeat::new(
        market.instrument().to_string(),
        trader,
    ))];
    let markets = mtx_kit::VenueInfo::of_perp_markets(
        spec.mandate,
        [(
            market.instrument().clone(),
            Some(market.address().to_string()),
        )],
    );
    let reads = CacheReads {
        reader: spec.reader,
        cache: spec.cache,
        slot: spec.slot,
    };
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
            reader: Reader::Live(Arc::new(reads)),
            sends: None,
            rfqs: None,
        },
        spec.log,
        None,
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
}

impl CacheReads {
    fn read(&self) -> Result<PerpReading, String> {
        let cache = self
            .cache
            .lock()
            .map_err(|_| "the account cache was poisoned".to_string())?;
        self.reader.read(&cache, self.slot.load(Ordering::Acquire))
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
        let reading = self.read()?;
        Ok(mtx_kit::Position {
            size: reading.position,
            entry_usd: reading.entry_usd,
        })
    }
}
