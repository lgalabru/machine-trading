//! The strategies a live runtime runs on its own (`docs/rfd/0004`, step 4):
//! each stepped on the chain's slots, controlled while it runs through a
//! Unix socket (`mtx live`, [`mtx_core::control`]), and kept on disk so a
//! restart brings the same set back.
//!
//! - **Steps.** A running strategy steps every `slots_per_step` slots, on
//!   the slot the feed reports. If its previous step is still under way
//!   when the next is due, that step is skipped and counted, never queued.
//! - **Control.** A command reaches a strategy at its step boundary: one
//!   that needs the strategy waits for its step to end.
//! - **The desired state** (each strategy's spec, its component's hash, and
//!   whether it runs) is rewritten on every change. At start the runtime
//!   restarts each strategy, and one whose component changed since is kept
//!   stopped.
//! - **The enforcer** pauses a strategy and cancels its orders when its
//!   drawdown passes the mandate's limit, and pauses every strategy when
//!   the feed has dropped events: nothing read from a lossy feed is traded
//!   on.

use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use mtx_core::{
    Mandate,
    control::{
        AddStrategy, Command, FeedStatus, MESSAGE_BYTES_MAX, OnPause, Reply, StrategyState,
        StrategyStatus,
    },
    mandate::{InstrumentRef, Observed},
};
use mtx_kit::chain::RpcChainClient;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use tokio::sync::{mpsc, oneshot, watch};

use super::{
    bot::{self, BotSpec},
    feed::{AccountCache, FeedProgress, Watch},
    perp::PerpReader,
};
use crate::desk::{Clock, WasmBot};

/// Where the runtime takes commands and keeps its strategies: the
/// `control` member of its configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControlConfig {
    /// The Unix socket `mtx live` talks to. A file already there is
    /// removed first.
    pub socket: PathBuf,
    /// The desired state, rewritten on every change and read at start.
    pub state: PathBuf,
    /// The node strategies send through and read what the cache doesn't
    /// hold from, and new strategies' accounts are backfilled from.
    pub rpc_url: String,
    /// A directory for each strategy's prints, `<name>.log`.
    pub logs: PathBuf,
    /// How long one of a strategy's calls may run: past it the call is
    /// interrupted and the strategy stops. Under a slot by default.
    #[serde(default = "default_step_deadline_ms")]
    pub step_deadline_ms: u64,
    /// Send over the leaders' TPU rather than the node's RPC.
    #[serde(default)]
    pub tpu: Option<super::tpu::TpuConfig>,
}

fn default_step_deadline_ms() -> u64 {
    DEFAULT_STEP_DEADLINE_MS
}

/// A step's deadline unless the configuration says otherwise: most of a
/// 400 ms slot, leaving the rest to plan, sign and send.
const DEFAULT_STEP_DEADLINE_MS: u64 = 250;

/// The most strategies one runtime runs.
pub const STRATEGIES_MAX: usize = 32;

/// The most commands waiting for the runtime: `mtx live` sends one a
/// connection, and a person types them.
const COMMANDS_WAITING_MAX: usize = 16;

/// How often the control thread looks for a connection: a person typing
/// `mtx live` waits this long at most.
const ACCEPT_LOOKS_EVERY: Duration = Duration::from_millis(50);

/// How long a step waits for the feed to apply what the chain wrote
/// before its slot: past it, the strategy steps on what the cache holds.
const SETTLE_WITHIN: Duration = Duration::from_secs(1);

/// How many slots the runtime may lag what the chain told of before every
/// strategy is paused: a little more than a step, so a busy slot doesn't
/// stop anything, and well under a second of a market moving on.
const FEED_SLOTS_BEHIND_MAX: u64 = 4;

/// How far past the touch a flattening market order may fill.
const FLATTEN_SLIPPAGE: f64 = 0.05;

/// How long the backfill of a new strategy's accounts may take.
const BACKFILL_TIMEOUT: Duration = Duration::from_secs(30);

/// Steps in a session whose mandate has no term: about 27 years of 400 ms
/// slots, which no strategy reaches, and which keeps its fuel arithmetic
/// far from overflow.
const NO_TERM_STEPS: u64 = u32::MAX as u64;

/// What the runtime keeps on disk.
#[derive(Debug, Default, Serialize, Deserialize)]
struct DesiredState {
    strategies: Vec<Stored>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    /// The strategy as it was added, with `run` saying whether it runs.
    spec: AddStrategy,
    /// The component's SHA-256 when it was added: one that changed since
    /// isn't restarted.
    component_sha256: String,
}

/// The parts of the runtime a strategy reads its market through.
#[derive(Clone)]
pub struct Shared {
    pub cache: Arc<Mutex<AccountCache>>,
    pub watch: Arc<Watch>,
    pub progress: FeedProgress,
}

/// What the strategies' thread starts with besides the configuration.
pub struct Inputs {
    pub shared: Shared,
    /// The newest slot the feed reported.
    pub slots: watch::Receiver<u64>,
    /// The slots' starts and ends, for the TPU's leader tracker, when the
    /// configuration sends over the TPU.
    pub slot_events:
        Option<mpsc::Receiver<solana_tpu_client_next::node_address_service::SlotEvent>>,
}

/// Start the strategies' thread: restore the desired state, then step the
/// running strategies on `slots`, and serve commands on the socket.
pub fn spawn(config: ControlConfig, inputs: Inputs) -> Result<(), String> {
    let Inputs {
        shared,
        slots,
        slot_events,
    } = inputs;
    std::fs::create_dir_all(&config.logs).map_err(|e| format!("{}: {e}", config.logs.display()))?;
    let (commands, received) = mpsc::channel(COMMANDS_WAITING_MAX);
    listen(&config.socket, commands)?;
    std::thread::Builder::new()
        .name("mtx-live-strategies".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::error!(%error, "no async runtime for strategies; none will run");
                    return;
                }
            };
            runtime.block_on(async move {
                let tpu = match (&config.tpu, slot_events) {
                    (Some(tpu), Some(events)) => {
                        match super::tpu::Tpu::start(tpu, &config.rpc_url, events).await {
                            Ok(tpu) => Some(Arc::new(tpu)),
                            Err(error) => {
                                // Strategies still run, sending through the RPC.
                                tracing::error!(%error, "the TPU client didn't start; sends go through the RPC");
                                None
                            }
                        }
                    }
                    _ => None,
                };
                let mut manager = Manager::new(config, shared, tpu);
                manager.restore().await;
                manager.serve(slots, received).await;
            });
        })
        .map_err(|e| format!("the strategies' thread: {e}"))?;
    Ok(())
}

/// A command and where its reply goes.
type Request = (Command, oneshot::Sender<Reply>);

/// Take commands on `socket`, one a connection, on a thread of their own,
/// and hand each to the manager.
fn listen(socket: &Path, commands: mpsc::Sender<Request>) -> Result<(), String> {
    match std::fs::remove_file(socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", socket.display())),
    }
    let listener = UnixListener::bind(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    // Not blocking, so the thread sees when the strategies stop (the
    // runtime was unloaded) and ends, rather than waiting on a socket
    // nobody will answer.
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("{}: {e}", socket.display()))?;
    std::thread::Builder::new()
        .name("mtx-live-control".into())
        .spawn(move || {
            while !commands.is_closed() {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let answered = stream
                            .set_nonblocking(false)
                            .map_err(|e| e.to_string())
                            .and_then(|()| answer(stream, &commands));
                        if let Err(error) = answered {
                            tracing::warn!(%error, "a control connection failed");
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(ACCEPT_LOOKS_EVERY);
                    }
                    Err(error) => tracing::warn!(%error, "a control connection failed"),
                }
            }
        })
        .map_err(|e| format!("the control thread: {e}"))?;
    Ok(())
}

fn answer(stream: UnixStream, commands: &mpsc::Sender<Request>) -> Result<(), String> {
    let mut line = String::new();
    BufReader::new(stream.try_clone().map_err(|e| e.to_string())?)
        .take(MESSAGE_BYTES_MAX as u64)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    let reply = match serde_json::from_str::<Command>(&line) {
        Ok(command) => {
            let (reply, replied) = oneshot::channel();
            commands
                .blocking_send((command, reply))
                .map_err(|_| "the runtime's strategies stopped".to_string())?;
            replied
                .blocking_recv()
                .map_err(|_| "the runtime dropped the command".to_string())?
        }
        Err(error) => Reply::Error {
            error: format!("not a command: {error}"),
        },
    };
    let mut text = serde_json::to_string(&reply).map_err(|e| e.to_string())?;
    text.push('\n');
    (&stream)
        .write_all(text.as_bytes())
        .map_err(|e| e.to_string())
}

/// One strategy the runtime runs.
struct Strategy {
    spec: AddStrategy,
    component_sha256: String,
    authority: Pubkey,
    keypair: Arc<Keypair>,
    reader: PerpReader,
    /// The slot its market is read at: its step's.
    slot: Arc<AtomicU64>,
    cap: f64,
    max_drawdown: f64,
    /// The component, behind the lock its steps and commands take.
    inner: Arc<tokio::sync::Mutex<Inner>>,
    /// What status reports, readable while a step runs.
    stats: Arc<Mutex<Stats>>,
}

struct Inner {
    /// `None` if it couldn't start: a component that changed or didn't
    /// load. It is kept so that the desired state still holds it.
    bot: Option<WasmBot>,
    /// Its next step's number.
    k: u64,
    /// What the enforcer has seen of it under its current mandate.
    observed: Observed,
}

#[derive(Debug, Clone)]
struct Stats {
    state: StrategyState,
    steps: u64,
    skipped: u64,
    /// The slot its next step is due at.
    due: u64,
    last_step_ms: Option<u64>,
    last_slot: Option<u64>,
    position: Option<f64>,
    pnl: Option<f64>,
    drawdown: Option<f64>,
}

struct Manager {
    config: ControlConfig,
    shared: Shared,
    /// The TPU client strategies send through, if the runtime sends so.
    tpu: Option<Arc<super::tpu::Tpu>>,
    strategies: BTreeMap<String, Strategy>,
    /// The latest slot the feed reported.
    slot: u64,
}

impl Manager {
    fn new(config: ControlConfig, shared: Shared, tpu: Option<Arc<super::tpu::Tpu>>) -> Self {
        Self {
            config,
            shared,
            tpu,
            strategies: BTreeMap::new(),
            slot: 0,
        }
    }

    /// Restart every strategy the desired state holds, as it was.
    async fn restore(&mut self) {
        let stored = match std::fs::read(&self.config.state) {
            Ok(bytes) => match serde_json::from_slice::<DesiredState>(&bytes) {
                Ok(state) => state.strategies,
                Err(error) => {
                    tracing::error!(%error, "the desired state can't be read; starting with none");
                    return;
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                tracing::error!(%error, "the desired state can't be read; starting with none");
                return;
            }
        };
        for stored in stored.into_iter().take(STRATEGIES_MAX) {
            let name = stored.spec.name.clone();
            if let Err(error) = self.add(stored.spec, Some(stored.component_sha256)).await {
                tracing::warn!(name, %error, "a strategy couldn't be restored");
            }
        }
    }

    async fn serve(
        &mut self,
        mut slots: watch::Receiver<u64>,
        mut commands: mpsc::Receiver<Request>,
    ) {
        loop {
            tokio::select! {
                changed = slots.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let slot = *slots.borrow_and_update();
                    self.slot = self.slot.max(slot);
                    self.step(self.slot).await;
                }
                request = commands.recv() => {
                    let Some((command, reply)) = request else {
                        return;
                    };
                    let answer = self.handle(command).await;
                    // A client that hung up doesn't need the answer.
                    let _ = reply.send(answer);
                }
            }
        }
    }

    /// Step every running strategy whose step is due at `slot`.
    async fn step(&mut self, slot: u64) {
        let progress = &self.shared.progress;
        let stale = match (progress.dropped(), progress.slots_behind()) {
            (0, behind) if behind <= FEED_SLOTS_BEHIND_MAX => None,
            (0, behind) => Some(format!("the feed is {behind} slots behind the chain")),
            (dropped, _) => Some(format!(
                "the feed dropped {dropped} events: the cache may be stale"
            )),
        };
        if let Some(why) = stale {
            let running: Vec<String> = self
                .strategies
                .iter()
                .filter(|(_, s)| lock(&s.stats).state == StrategyState::Running)
                .map(|(name, _)| name.clone())
                .collect();
            for name in running {
                if let Err(error) = self.pause(&name, OnPause::Cancel, why.clone()).await {
                    tracing::warn!(name, %error, "a strategy couldn't be paused");
                }
            }
            return;
        }
        for (name, strategy) in &self.strategies {
            let mut stats = lock(&strategy.stats);
            if stats.state != StrategyState::Running || slot < stats.due {
                continue;
            }
            stats.due = slot + strategy.spec.slots_per_step;
            let Ok(mut inner) = strategy.inner.clone().try_lock_owned() else {
                stats.skipped += 1;
                continue;
            };
            drop(stats);
            let (name, progress) = (name.clone(), self.shared.progress.clone());
            let (slot_cell, stats) = (strategy.slot.clone(), strategy.stats.clone());
            let (mandate, close_on_breach) =
                (strategy.spec.mandate.clone(), strategy.spec.close_on_breach);
            let instrument = instrument_of(&strategy.spec.symbol);
            // The step's clock starts as its slot reaches the strategies.
            let started = std::time::Instant::now();
            tokio::spawn(async move {
                let inner = &mut *inner;
                let Some(bot) = inner.bot.as_mut() else {
                    return;
                };
                if !tokio::task::block_in_place(|| progress.settle(SETTLE_WITHIN)) {
                    tracing::warn!(
                        name,
                        slot,
                        "the feed is behind; the step reads what the cache holds"
                    );
                }
                slot_cell.store(slot, Ordering::Release);
                let step = mtx_core::Step::new(inner.k, slot, false);
                let answered = bot.turn(&step, None).await;
                let took_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                inner.k += 1;
                let market = bot.last_market().cloned();
                // After the trade: what the step left, into what the
                // enforcer has seen, and the mandate's audit of it all.
                if let Some(market) = &market {
                    let observed = &mut inner.observed;
                    observed.record(&instrument, market.position.size);
                    observed.max_drawdown_usd = observed.max_drawdown_usd.max(market.drawdown);
                    if market.resting_bids > 0.0 {
                        observed.record_open_order(&instrument, mtx_core::Side::Buy);
                    }
                    if market.resting_asks > 0.0 {
                        observed.record_open_order(&instrument, mtx_core::Side::Sell);
                    }
                }
                let breach = mandate.audit(&inner.observed);
                let paused = {
                    let mut stats = lock(&stats);
                    stats.steps += u64::from(answered);
                    stats.last_step_ms = Some(took_ms);
                    if let Some(market) = &market {
                        stats.last_slot = Some(market.slot);
                        stats.position = Some(market.position.size);
                        stats.pnl = Some(market.pnl);
                        stats.drawdown = Some(market.drawdown);
                    }
                    if let Some(why) = &bot.stopped {
                        stats.state = StrategyState::Stopped { why: why.clone() };
                    }
                    match (&breach, &stats.state) {
                        (Some(breach), StrategyState::Running) => {
                            stats.state = StrategyState::Paused {
                                why: format!("the enforcer: {breach}"),
                            };
                            true
                        }
                        _ => false,
                    }
                };
                if paused {
                    for failed in bot.cancel_all().await {
                        tracing::warn!(name, failed, "an order couldn't be cancelled");
                    }
                    if close_on_breach && let Err(error) = bot.flatten(FLATTEN_SLIPPAGE).await {
                        tracing::warn!(name, %error, "the position couldn't be closed");
                    }
                }
            });
        }
    }

    async fn handle(&mut self, command: Command) -> Reply {
        let done = match command {
            Command::Status => Ok(()),
            Command::Pause { name, orders } => {
                self.each(name, |m, name| m.pause_by_command(name, orders))
                    .await
            }
            Command::Resume { name } => self.each(name, Self::resume).await,
            Command::Kill => {
                self.each(None, |m, name| {
                    m.pause(name, OnPause::Cancel, "the kill switch".to_string())
                })
                .await
            }
            Command::Mandate { name, mandate } => self.change_mandate(&name, mandate).await,
            Command::Add(spec) => self.add(spec, None).await,
            Command::Remove { name } => self.remove(&name).await,
        };
        match done.and_then(|()| self.persist()) {
            Ok(()) => Reply::Ok {
                feed: FeedStatus {
                    slot: self.slot,
                    behind: self.shared.progress.slots_behind(),
                    dropped: self.shared.progress.dropped(),
                },
                strategies: self.status(),
            },
            Err(error) => Reply::Error { error },
        }
    }

    /// Do `action` to `name`, or to every strategy.
    async fn each<'a, F, Fut>(
        &'a mut self,
        name: Option<String>,
        mut action: F,
    ) -> Result<(), String>
    where
        F: FnMut(&'a Self, &'a str) -> Fut,
        Fut: std::future::Future<Output = Result<(), String>> + 'a,
    {
        let this: &'a Self = self;
        match name {
            Some(name) => {
                let (name, _) = this
                    .strategies
                    .get_key_value(&name)
                    .ok_or_else(|| format!("no strategy {name}"))?;
                action(this, name).await
            }
            None => {
                for name in this.strategies.keys() {
                    action(this, name).await?;
                }
                Ok(())
            }
        }
    }

    async fn pause_by_command(&self, name: &str, orders: OnPause) -> Result<(), String> {
        self.pause(name, orders, "paused".to_string()).await
    }

    /// Stop stepping `name`, at its step boundary, and do `orders` to its
    /// resting orders. A stopped strategy stays stopped.
    async fn pause(&self, name: &str, orders: OnPause, why: String) -> Result<(), String> {
        let strategy = self.get(name)?;
        let mut inner = strategy.inner.lock().await;
        {
            let mut stats = lock(&strategy.stats);
            if matches!(stats.state, StrategyState::Stopped { .. }) {
                return Ok(());
            }
            stats.state = StrategyState::Paused { why };
        }
        let Some(bot) = inner.bot.as_mut() else {
            return Ok(());
        };
        match orders {
            OnPause::Keep => Ok(()),
            OnPause::Cancel => cancelled(bot.cancel_all().await),
            OnPause::Flatten => {
                cancelled(bot.cancel_all().await)?;
                bot.flatten(FLATTEN_SLIPPAGE).await
            }
        }
    }

    async fn resume(&self, name: &str) -> Result<(), String> {
        let strategy = self.get(name)?;
        let mut stats = lock(&strategy.stats);
        if let StrategyState::Paused { .. } = stats.state {
            stats.state = StrategyState::Running;
            stats.due = self.slot;
        }
        Ok(())
    }

    /// Restart `name` under `mandate`, paused or running as it was.
    async fn change_mandate(&mut self, name: &str, mandate: Mandate) -> Result<(), String> {
        mandate.validate()?;
        let (cap, max_drawdown) = limits(&mandate, &self.get(name)?.spec.symbol)?;
        let start_slot = self.slot;
        let strategy = self.get(name)?;
        let mut spec = strategy.spec.clone();
        spec.mandate = mandate;
        let mut inner = strategy.inner.lock().await;
        if let Some(mut bot) = inner.bot.take() {
            bot.end().await;
        }
        let bot = self.start_bot(&spec, strategy, start_slot).await;
        let stopped = bot.as_ref().err().cloned();
        inner.bot = bot.ok();
        inner.k = 0;
        // A new mandate audits from here on.
        inner.observed = Observed::default();
        drop(inner);
        let strategy = self
            .strategies
            .get_mut(name)
            .expect("the strategy changed above");
        strategy.spec = spec;
        strategy.cap = cap;
        strategy.max_drawdown = max_drawdown;
        if let Some(why) = stopped {
            lock(&strategy.stats).state = StrategyState::Stopped { why: why.clone() };
            return Err(why);
        }
        Ok(())
    }

    /// Compile, start and keep `spec`: paused unless it runs. `restored`
    /// is the component's hash when the desired state brings it back.
    async fn add(&mut self, spec: AddStrategy, restored: Option<String>) -> Result<(), String> {
        if self.strategies.contains_key(&spec.name) {
            return Err(format!("a strategy is already named {}", spec.name));
        }
        if self.strategies.len() == STRATEGIES_MAX {
            return Err(format!(
                "a runtime runs at most {STRATEGIES_MAX} strategies"
            ));
        }
        if spec.slots_per_step == 0 {
            return Err("a strategy steps every slot at most: slots_per_step is at least 1".into());
        }
        spec.mandate.validate()?;
        let (cap, max_drawdown) = limits(&spec.mandate, &spec.symbol)?;
        let component = std::fs::read(&spec.component)
            .map_err(|e| format!("{}: {e}", spec.component.display()))?;
        let component_sha256 = format!("{:x}", Sha256::digest(&component));
        let keypair = read_keypair(&spec.keypair)?;
        let authority = solana_signer::Signer::pubkey(&keypair);
        let exchange = std::fs::read_to_string(&spec.exchange)
            .map_err(|e| format!("{}: {e}", spec.exchange.display()))?;
        let exchange = serde_json::from_str(&exchange)
            .map_err(|e| format!("{}: {e}", spec.exchange.display()))?;
        let reader = PerpReader::new(exchange, &spec.symbol, &authority)?;
        self.backfill(&reader).await?;
        let strategy = Strategy {
            component_sha256: component_sha256.clone(),
            authority,
            keypair: Arc::new(keypair),
            reader,
            slot: Arc::new(AtomicU64::new(self.slot)),
            cap,
            max_drawdown,
            inner: Arc::new(tokio::sync::Mutex::new(Inner {
                bot: None,
                k: 0,
                observed: Observed::default(),
            })),
            stats: Arc::new(Mutex::new(Stats {
                state: StrategyState::Paused {
                    why: "added".into(),
                },
                steps: 0,
                skipped: 0,
                due: 0,
                last_step_ms: None,
                last_slot: None,
                position: None,
                pnl: None,
                drawdown: None,
            })),
            spec: spec.clone(),
        };
        let state = match restored {
            Some(hash) if hash != component_sha256 => Err(format!(
                "its component changed since it was added (sha256 {hash}, now {component_sha256})"
            )),
            _ => self.start_bot(&spec, &strategy, self.slot).await,
        };
        match state {
            Ok(bot) => {
                strategy.inner.lock().await.bot = Some(bot);
                let mut stats = lock(&strategy.stats);
                stats.due = self.slot;
                if spec.run {
                    stats.state = StrategyState::Running;
                }
            }
            Err(why) => {
                lock(&strategy.stats).state = StrategyState::Stopped { why };
            }
        }
        self.strategies.insert(spec.name.clone(), strategy);
        Ok(())
    }

    /// Pause `name`, cancel its orders, `end` it and forget it.
    async fn remove(&mut self, name: &str) -> Result<(), String> {
        self.pause(name, OnPause::Cancel, "removed".into()).await?;
        let strategy = self.strategies.remove(name).expect("paused above");
        let mut inner = strategy.inner.lock().await;
        if let Some(mut bot) = inner.bot.take() {
            bot.end().await;
        }
        Ok(())
    }

    async fn start_bot(
        &self,
        spec: &AddStrategy,
        strategy: &Strategy,
        start_slot: u64,
    ) -> Result<WasmBot, String> {
        let log_path = self.config.logs.join(format!("{}.log", spec.name));
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map_err(|e| format!("{}: {e}", log_path.display()))?;
        let end_slot = spec
            .mandate
            .term
            .as_ref()
            .map_or(u64::MAX, |t| t.until_slot);
        let steps = match spec.mandate.term {
            Some(_) => end_slot.saturating_sub(start_slot) / spec.slots_per_step,
            None => NO_TERM_STEPS,
        };
        bot::start(BotSpec {
            component: &spec.component,
            rpc_url: &self.config.rpc_url,
            keypair: &strategy.keypair,
            exchange_path: &spec.exchange,
            symbol: &spec.symbol,
            mandate: &spec.mandate,
            clock: Clock {
                seed: mtx_core::turns::scenario_seed(&spec.name),
                steps,
                slots_per_step: spec.slots_per_step,
                start_slot,
                vars: Vec::new(),
                reports: mtx_kit::Reports::default(),
                step_deadline: Some(Duration::from_millis(self.config.step_deadline_ms)),
            },
            end_slot,
            cap: strategy.cap,
            max_drawdown: strategy.max_drawdown,
            reader: strategy.reader.clone(),
            cache: self.shared.cache.clone(),
            slot: strategy.slot.clone(),
            log: Box::new(log),
            sends: self.sends(spec)?,
        })
        .await
    }

    /// The chain a strategy's orders go through: nowhere in a shadow run,
    /// the leaders' TPU when the runtime sends so, else the node's RPC.
    fn sends(
        &self,
        spec: &AddStrategy,
    ) -> Result<Option<Arc<dyn mtx_kit::chain::ChainClient>>, String> {
        let rpc = || RpcChainClient::new(self.config.rpc_url.clone());
        if spec.shadow {
            let journal = self.config.logs.join(format!("{}.shadow.jsonl", spec.name));
            return Ok(Some(Arc::new(super::shadow::ShadowChain::new(
                rpc(),
                &journal,
            )?)));
        }
        Ok(self.tpu.as_ref().map(|tpu| {
            Arc::new(super::tpu::TpuChainClient::new(rpc(), tpu.clone()))
                as Arc<dyn mtx_kit::chain::ChainClient>
        }))
    }

    /// Watch a new strategy's accounts, and read them once from the node:
    /// the feed only tells of writes.
    async fn backfill(&self, reader: &PerpReader) -> Result<(), String> {
        let addresses = reader.addresses();
        for address in &addresses {
            self.shared.watch.add(*address);
        }
        let chain = RpcChainClient::with_timeout(&self.config.rpc_url, BACKFILL_TIMEOUT);
        let mut found = AccountCache::default();
        super::backfill(&chain, &addresses, &mut found).await?;
        lock(&self.shared.cache).merge(found);
        Ok(())
    }

    fn get(&self, name: &str) -> Result<&Strategy, String> {
        self.strategies
            .get(name)
            .ok_or_else(|| format!("no strategy {name}"))
    }

    fn status(&self) -> Vec<StrategyStatus> {
        self.strategies
            .iter()
            .map(|(name, strategy)| {
                let stats = lock(&strategy.stats).clone();
                StrategyStatus {
                    name: name.clone(),
                    state: stats.state,
                    symbol: strategy.spec.symbol.clone(),
                    authority: strategy.authority.to_string(),
                    cap: strategy.cap,
                    max_drawdown: strategy.max_drawdown,
                    shadow: strategy.spec.shadow,
                    last_step_ms: stats.last_step_ms,
                    steps: stats.steps,
                    skipped: stats.skipped,
                    last_slot: stats.last_slot,
                    position: stats.position,
                    pnl: stats.pnl,
                    drawdown: stats.drawdown,
                }
            })
            .collect()
    }

    /// Write the desired state: each strategy's spec, its component's
    /// hash, and whether it runs. Written beside the file and renamed over
    /// it, so a crash leaves the old state or the new, never half of one.
    fn persist(&self) -> Result<(), String> {
        let state = DesiredState {
            strategies: self
                .strategies
                .values()
                .map(|strategy| Stored {
                    spec: AddStrategy {
                        run: lock(&strategy.stats).state == StrategyState::Running,
                        ..strategy.spec.clone()
                    },
                    component_sha256: strategy.component_sha256.clone(),
                })
                .collect(),
        };
        let text = serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?;
        let path = &self.config.state;
        let partial = path.with_extension("partial");
        std::fs::write(&partial, text).map_err(|e| format!("{}: {e}", partial.display()))?;
        std::fs::rename(&partial, path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// The mandate's position cap on `symbol`'s perpetual, and its drawdown
/// limit (none is no limit).
fn limits(mandate: &Mandate, symbol: &str) -> Result<(f64, f64), String> {
    let instrument = instrument_of(symbol);
    let allowance = mandate
        .allowance(&instrument)
        .ok_or_else(|| format!("the mandate doesn't allow {instrument}"))?;
    Ok((
        allowance.max_position,
        mandate.limits.max_drawdown_usd.unwrap_or(f64::INFINITY),
    ))
}

/// `symbol`'s perpetual, as mandates name it.
fn instrument_of(symbol: &str) -> InstrumentRef {
    InstrumentRef {
        venue: crate::venue::perps::VENUE.into(),
        kind: mtx_core::mandate::Kind::Perp,
        symbol: symbol.into(),
    }
}

fn read_keypair(path: &Path) -> Result<Keypair, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let bytes: Vec<u8> =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Keypair::try_from(bytes.as_slice()).map_err(|_| format!("{} is not a keypair", path.display()))
}

/// Every cancel went through, or why not.
fn cancelled(failed: Vec<String>) -> Result<(), String> {
    match failed.is_empty() {
        true => Ok(()),
        false => Err(format!("orders not cancelled: {}", failed.join("; "))),
    }
}

/// A lock no thread holds across a panic: each holder only copies numbers
/// in or out.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mandate_gives_the_cap_on_its_market_and_its_drawdown_limit() {
        let mandate = Mandate::perp("phoenix", "SOL", 2.5, 300.0);
        assert_eq!(limits(&mandate, "SOL"), Ok((2.5, 300.0)));
        assert!(
            limits(&mandate, "BTC")
                .unwrap_err()
                .contains("doesn't allow")
        );
    }

    #[test]
    fn the_desired_state_reads_back_as_it_was_written() {
        let state = DesiredState {
            strategies: vec![Stored {
                spec: AddStrategy {
                    name: "carry".into(),
                    component: "/bots/carry.wasm".into(),
                    mandate: Mandate::perp("phoenix", "SOL", 1.0, 100.0),
                    keypair: "/keys/carry.json".into(),
                    exchange: "/data/exchange.json".into(),
                    symbol: "SOL".into(),
                    slots_per_step: 2,
                    run: true,
                    close_on_breach: false,
                    shadow: false,
                },
                component_sha256: "ab".into(),
            }],
        };
        let text = serde_json::to_string(&state).unwrap();
        let back: DesiredState = serde_json::from_str(&text).unwrap();
        assert_eq!(back.strategies.len(), 1);
        assert_eq!(back.strategies[0].spec, state.strategies[0].spec);
        assert_eq!(back.strategies[0].component_sha256, "ab");
    }

    #[test]
    fn cancels_that_failed_say_which() {
        assert_eq!(cancelled(Vec::new()), Ok(()));
        let error = cancelled(vec!["phoenix:perp:SOL: refused".into()]).unwrap_err();
        assert!(error.contains("phoenix:perp:SOL"), "{error}");
    }
}
