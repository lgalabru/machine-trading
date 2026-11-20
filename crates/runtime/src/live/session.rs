//! Sessions the live runtime plays for a grader (`docs/rfd/0004`, step 3):
//! the grader moves the world and scores, and the runtime hosts the
//! strategy component, reads its market from the account cache, and sends
//! its orders as the bot.
//!
//! The runtime listens on a Unix socket ([`SessionConfig::socket`]). A
//! grader's turn host (`mtx_grader::session::TurnHost`) connects, writes the session's variables as one line of JSON
//! ([`mtx_core::turns::write_variables`]), and then plays the turn protocol
//! ([`mtx_core::turns`]) over the same stream, as it would with a process
//! bot. At each turn the runtime first waits until the feed has applied
//! every write the chain made so far, so the strategy reads the state the
//! grader's turn found.

use std::{
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use async_trait::async_trait;
use mtx_core::turns::{Turns, read_variables};
use mtx_kit::{BotEnv, venue::PhoenixVenue};
use serde::Deserialize;

use super::{
    feed::{AccountCache, FeedProgress},
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

/// Where the runtime takes sessions, and the strategy it plays them with:
/// the `session` member of the runtime's configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfig {
    /// The Unix socket the runtime listens on. A file already there is
    /// removed first.
    pub socket: PathBuf,
    /// The strategy component (`bot.wasm`).
    pub component: PathBuf,
    /// Where the strategy's prints go. Truncated at each session.
    pub log: PathBuf,
}

/// How long a session waits for the runtime to be ready: the backfill's
/// own timeout, and as long again for its writes.
const READY_WITHIN: Duration = Duration::from_secs(60);

/// How often a session looks again whether the runtime is ready.
const READY_LOOKS_EVERY: Duration = Duration::from_millis(10);

/// Whether the runtime's cache holds the accounts it started with (what
/// its backfill read): set once, by the runtime's thread.
#[derive(Debug, Default)]
pub struct Ready(std::sync::atomic::AtomicBool);

impl Ready {
    pub fn set(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Wait until it is set, for at most `within`: whether it was.
    fn wait(&self, within: Duration) -> bool {
        let deadline = std::time::Instant::now() + within;
        while !self.0.load(Ordering::Acquire) {
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(READY_LOOKS_EVERY);
        }
        true
    }
}

/// How long a turn waits for the feed to apply what the chain wrote before
/// it: past it, the strategy steps on what the cache holds, and the turn
/// says so in the log.
const SETTLE_WITHIN: Duration = Duration::from_secs(2);

/// What a session's strategy is told: its variables, without the ones that
/// describe its environment ([`BotEnv::vars`]), which the runtime reads.
fn task_variables(vars: &[(String, String)]) -> Vec<(String, String)> {
    const ENVIRONMENT: [&str; 8] = [
        BotEnv::RPC_URL,
        BotEnv::KEYPAIR,
        BotEnv::EXCHANGE,
        BotEnv::SYMBOL,
        BotEnv::END_SLOT,
        BotEnv::MAX_POSITION,
        BotEnv::MAX_DRAWDOWN,
        BotEnv::MANDATE,
    ];
    vars.iter()
        .filter(|(name, _)| !ENVIRONMENT.contains(&name.as_str()))
        .cloned()
        .collect()
}

/// Listen on `config.socket` and play each session a grader brings, one at
/// a time, on a thread of the runtime's own. `readers` are the markets the
/// runtime watches: a session's market must be one, read for the session's
/// own account.
pub fn spawn(
    config: SessionConfig,
    readers: Vec<PerpReader>,
    cache: Arc<Mutex<AccountCache>>,
    progress: FeedProgress,
    ready: Arc<Ready>,
) -> Result<JoinHandle<()>, String> {
    // A socket left by a runtime that didn't stop cleanly would refuse the
    // bind.
    match std::fs::remove_file(&config.socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", config.socket.display())),
    }
    let listener = UnixListener::bind(&config.socket)
        .map_err(|e| format!("{}: {e}", config.socket.display()))?;
    std::thread::Builder::new()
        .name("mtx-live-sessions".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "no async runtime for sessions; none will be played");
                    return;
                }
            };
            for stream in listener.incoming() {
                let stream = match stream {
                    Ok(stream) => stream,
                    Err(error) => {
                        tracing::warn!(%error, "a session's connection failed");
                        continue;
                    }
                };
                if !ready.wait(READY_WITHIN) {
                    tracing::warn!("the runtime isn't ready: the session is refused");
                    continue;
                }
                let played =
                    runtime.block_on(play(stream, &config, &readers, cache.clone(), &progress));
                match played {
                    Ok(steps) => tracing::info!(steps, "a session ended"),
                    Err(error) => tracing::warn!(%error, "a session failed"),
                }
            }
        })
        .map_err(|e| format!("the sessions' thread: {e}"))
}

/// Play one session to its end: the number of turns the strategy took.
async fn play(
    mut stream: UnixStream,
    config: &SessionConfig,
    readers: &[PerpReader],
    cache: Arc<Mutex<AccountCache>>,
    progress: &FeedProgress,
) -> Result<u64, String> {
    let vars = read_variables(&mut stream).map_err(|e| format!("the session's variables: {e}"))?;
    let env = BotEnv::from_vars(|name| {
        vars.iter()
            .find(|(n, _)| n == name)
            .map(|(_, value)| value.clone())
    })
    .map_err(|e| e.to_string())?;
    let keypair = env.keypair().map_err(|e| e.to_string())?;
    let authority = solana_signer::Signer::pubkey(&keypair);
    let exchange_json = std::fs::read_to_string(&env.exchange_path)
        .map_err(|e| format!("{}: {e}", env.exchange_path.display()))?;
    let venue = PhoenixVenue::from_json(&exchange_json)?;
    let reader = readers
        .iter()
        .find(|reader| reader.symbol() == env.symbol)
        .ok_or_else(|| format!("the runtime doesn't watch {}", env.symbol))?;
    if reader.trader() != venue.trader_account(&authority).to_bytes() {
        return Err(format!(
            "the runtime reads {} for another account than {authority}",
            env.symbol
        ));
    }
    let mandate = env.mandate().map_err(|e| e.to_string())?;
    let market = PerpMarket::new(
        venue.exchange().clone(),
        env.exchange_path.clone(),
        &env.symbol,
        &env.rpc_url,
        authority,
    )
    .map_err(|e| e.to_string())?;
    let trader = Arc::new(
        market
            .trader(keypair.insecure_clone(), &env.rpc_url)
            .map_err(|e| e.to_string())?,
    );
    let seats: Vec<Box<dyn Seat>> = vec![Box::new(PerpSeat::new(
        market.instrument().to_string(),
        trader,
    ))];
    let markets = mtx_kit::VenueInfo::of_perp_markets(
        &mandate,
        [(
            market.instrument().clone(),
            Some(market.address().to_string()),
        )],
    );
    let mut turns = tokio::task::block_in_place(|| Turns::over(stream))
        .map_err(|e| format!("the grader's hello: {e}"))?;
    let hello = turns.hello().clone();
    let slot = Arc::new(AtomicU64::new(hello.start_slot));
    let reads = CacheReads {
        reader: reader.clone(),
        cache,
        slot: slot.clone(),
    };
    let log =
        std::fs::File::create(&config.log).map_err(|e| format!("{}: {e}", config.log.display()))?;
    let mut bot = WasmBot::<NoRawSends>::start(
        &config.component,
        HttpReads::boxed(&env.rpc_url),
        Clock {
            seed: hello.seed.unwrap_or_default(),
            steps: hello.steps,
            slots_per_step: hello.slots_per_step,
            start_slot: hello.start_slot,
            vars: task_variables(&vars),
            reports: hello.reports.into(),
        },
        Terms {
            end_slot: hello.end_slot,
            mandate: &mandate,
            cap: env.max_position,
            max_drawdown: env.max_drawdown_usd,
        },
        Desk {
            markets,
            seats,
            symbol: env.symbol.clone(),
            venue: exchange_json,
            authority: authority.to_string(),
            reader: Reader::Live(Arc::new(reads)),
            sends: None,
            rfqs: None,
        },
        Box::new(log),
        None,
        None,
    )
    .await
    .map_err(|e| e.to_string())?;
    let mut played = 0;
    loop {
        let next = tokio::task::block_in_place(|| turns.next())
            .map_err(|e| format!("the turn socket: {e}"))?;
        let Some(step) = next else {
            break;
        };
        if !progress.settle(SETTLE_WITHIN) {
            tracing::warn!(
                k = step.k,
                "the feed is behind; the step reads what the cache holds"
            );
        }
        slot.store(step.slot, Ordering::Release);
        if bot.turn(&step, None).await {
            played += 1;
        }
    }
    bot.end().await;
    Ok(played)
}

/// A session's market, as the account cache holds it at the step's slot.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_strategy_is_told_the_tasks_variables_not_its_environment() {
        let vars = vec![
            (BotEnv::SYMBOL.to_string(), "SPCX".to_string()),
            ("MTX_STOP_OUT".to_string(), "1".to_string()),
        ];
        assert_eq!(
            task_variables(&vars),
            [("MTX_STOP_OUT".to_string(), "1".to_string())]
        );
    }
}
