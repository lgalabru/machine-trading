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

use mtx_core::turns::{Turns, read_variables};
use mtx_kit::BotEnv;
use serde::Deserialize;

use super::{
    bot::{self, BotSpec},
    feed::{AccountCache, FeedProgress},
    perp::PerpReader,
};
use crate::desk::Clock;

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
    let reader = readers
        .iter()
        .find(|reader| reader.symbol() == env.symbol)
        .ok_or_else(|| format!("the runtime doesn't watch {}", env.symbol))?;
    let mandate = env.mandate().map_err(|e| e.to_string())?;
    let mut turns = tokio::task::block_in_place(|| Turns::over(stream))
        .map_err(|e| format!("the grader's hello: {e}"))?;
    let hello = turns.hello().clone();
    let slot = Arc::new(AtomicU64::new(hello.start_slot));
    let log =
        std::fs::File::create(&config.log).map_err(|e| format!("{}: {e}", config.log.display()))?;
    let mut bot = bot::start(BotSpec {
        component: &config.component,
        rpc_url: &env.rpc_url,
        keypair: &keypair,
        exchange_path: &env.exchange_path,
        symbol: &env.symbol,
        mandate: &mandate,
        clock: Clock {
            seed: hello.seed.unwrap_or_default(),
            steps: hello.steps,
            slots_per_step: hello.slots_per_step,
            start_slot: hello.start_slot,
            vars: task_variables(&vars),
            reports: hello.reports.into(),
            // As the grader hosts it: a session is scored against the
            // grader's own run, which has no deadline.
            step_deadline: None,
        },
        end_slot: hello.end_slot,
        cap: env.max_position,
        max_drawdown: env.max_drawdown_usd,
        reader: reader.clone(),
        cache,
        slot: slot.clone(),
        log: Box::new(log),
        // As the grader's component sends: through the bot's endpoint.
        sends: None,
        observer: None,
        readings: Default::default(),
        // A session's bot trades on its own account, never on paper.
        paper_position: None,
        dispatch: mtx_kit::protocols::phoenix::eternal::Dispatch::Confirm,
    })
    .await?;
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
