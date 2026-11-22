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
        AddStrategy, Command, CommandKind, ControlError, ErrorCode, FeedStatus, MESSAGE_BYTES_MAX,
        OnPause, Reply, StrategyState, StrategyStatus,
    },
    events::{Event, PauseReason, Side as EventSide, StepTimings},
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
    feed::{AccountCache, FeedProgress, SlotClock, Watch},
    perp::PerpReader,
};
use crate::desk::{Clock, TurnTimings, WasmBot};

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
    /// The strategies' worker threads: steps run on them, at once across
    /// strategies.
    #[serde(default = "default_workers")]
    pub workers: usize,
    /// Cores to pin the strategies' threads to, away from the validator's
    /// busiest: a step that never waits on a busy core. Unpinned without.
    #[serde(default)]
    pub cores: Option<Vec<usize>>,
    /// The Unix socket a sidecar reads the runtime's events on
    /// ([`super::events`], `docs/sidecar.md`). A file already there is
    /// removed first.
    #[serde(default)]
    pub events: Option<PathBuf>,
    /// Exchanges' public prices, read as venues by every strategy
    /// (`live::reference`): none by default.
    #[serde(default)]
    pub references: Vec<super::reference::ReferenceConfig>,
    /// Markets watched without trading them, each told as a `quote` and
    /// its trades (`live::watching`): none by default.
    #[serde(default)]
    pub watch: Option<super::watching::WatchConfig>,
}

/// Four: a step is a fraction of a millisecond of one core, and four let
/// a handful of strategies step at once with room for their sends.
fn default_workers() -> usize {
    4
}

/// The most worker threads: a runtime beside a validator takes a few
/// cores, not the machine.
const WORKERS_MAX: usize = 16;

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

/// Control connections at once: a sidecar's, its UI's, and people's.
const CONTROL_CONNECTIONS_MAX: usize = 16;

/// How often the feed's metrics are recorded, in slots: every few seconds,
/// a Grafana panel's resolution.
const FEED_METRICS_EVERY_SLOTS: u64 = 10;

/// How often held strategies wait to ask whether the node has caught up,
/// in slots: a few seconds, so a starting node isn't asked every slot.
const HEALTH_EVERY_SLOTS: u64 = 10;

/// How long the node may take to say whether it has caught up.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);

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
    /// What a sidecar is told as it happens.
    pub events: super::events::Events,
    /// The newest blockhash, for the strategies' transactions.
    pub blockhash: Arc<super::blockhash::LatestBlockhash>,
    /// Market readings, shared across strategies.
    pub readings: Arc<super::bot::Readings>,
    /// The exchanges' latest prices (`control.references`), as every
    /// strategy reads them.
    pub references: Arc<super::reference::Latest>,
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
pub fn spawn(config: ControlConfig, inputs: Inputs) -> Result<std::thread::JoinHandle<()>, String> {
    let Inputs {
        shared,
        slots,
        slot_events,
    } = inputs;
    std::fs::create_dir_all(&config.logs).map_err(|e| format!("{}: {e}", config.logs.display()))?;
    let (commands, received) = mpsc::channel(COMMANDS_WAITING_MAX);
    let connections = Arc::new(super::stopping::Connections::default());
    listen(&config.socket, commands.clone(), connections.clone())?;
    if let Some(socket) = &config.events {
        super::events::listen(
            socket,
            shared.events.clone(),
            commands.clone(),
            connections.clone(),
        )?;
    }
    super::reference::spawn(
        &config.references,
        shared.references.clone(),
        &config.logs,
        &connections,
    )?;
    let sockets = (config.socket.clone(), config.events.clone());
    let thread = std::thread::Builder::new()
        .name("mtx-live-strategies".into())
        .spawn(move || {
            let runtime = match strategies_runtime(&config) {
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
                manager.start_watching().await;
                manager.serve(slots, received).await;
            });
            // The commands' receiver is gone with the manager: the
            // listeners see it once woken.
            wake(&sockets.0);
            if let Some(events) = &sockets.1 {
                wake(events);
            }
            // Every connection shut and every socket thread joined: a
            // sidecar's kept connection sees the runtime close it.
            let left = connections.stop();
            if left > 0 {
                tracing::error!(left, "socket threads still ran after the strategies stopped");
            }
        })
        .map_err(|e| format!("the strategies' thread: {e}"))?;
    Ok(thread)
}

/// The strategies' async runtime: its worker threads, pinned to the cores
/// the configuration names, one a thread, round the list.
fn strategies_runtime(config: &ControlConfig) -> std::io::Result<tokio::runtime::Runtime> {
    let workers = config.workers.clamp(1, WORKERS_MAX);
    let cores = config.cores.clone().unwrap_or_default();
    let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(workers).enable_all();
    if !cores.is_empty() {
        builder.on_thread_start(move || {
            let core = cores[next.fetch_add(1, Ordering::Relaxed) % cores.len()];
            if !core_affinity::set_for_current(core_affinity::CoreId { id: core }) {
                tracing::warn!(core, "a strategies' thread couldn't be pinned");
            }
        });
    }
    builder.build()
}

/// A command and where its reply goes.
type Request = super::events::Request;

/// Take commands on `socket`, on threads of their own, and hand each to
/// the manager. A connection may carry many commands, each answered before
/// the next is read: a sidecar connects once.
fn listen(
    socket: &Path,
    commands: mpsc::Sender<Request>,
    connections: Arc<super::stopping::Connections>,
) -> Result<(), String> {
    match std::fs::remove_file(socket) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", socket.display())),
    }
    // Blocking: a connection is answered the moment it comes. When the
    // strategies stop, their thread connects once to wake this one, which
    // sees them gone and ends ([`wake`]).
    let listener = UnixListener::bind(socket).map_err(|e| format!("{}: {e}", socket.display()))?;
    let open = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let kept = connections.clone();
    connections
        .spawn("mtx-live-control", move || {
            let connections = kept;
            loop {
                let accepted = listener.accept();
                if commands.is_closed() || connections.stopped() {
                    return;
                }
                match accepted {
                    Ok((stream, _)) => {
                        if open.load(Ordering::Acquire) >= CONTROL_CONNECTIONS_MAX {
                            tracing::warn!(
                                max = CONTROL_CONNECTIONS_MAX,
                                "a control connection refused: too many"
                            );
                            continue;
                        }
                        let kept = match connections.keep(&stream) {
                            Ok(kept) => kept,
                            Err(error) => {
                                tracing::warn!(%error, "a control connection can't be kept");
                                continue;
                            }
                        };
                        open.fetch_add(1, Ordering::AcqRel);
                        let (commands, opened) = (commands.clone(), open.clone());
                        let spawned = connections.spawn("mtx-live-control-connection", move || {
                            if let Err(error) = answer(stream, &commands) {
                                tracing::warn!(%error, "a control connection failed");
                            }
                            // Shut for the peer too, whichever side ended it.
                            drop(kept);
                            opened.fetch_sub(1, Ordering::AcqRel);
                        });
                        if let Err(error) = spawned {
                            // The connection went with the thread that
                            // never ran: it no longer counts.
                            open.fetch_sub(1, Ordering::AcqRel);
                            tracing::warn!(%error, "a control connection's thread didn't start");
                        }
                    }
                    Err(error) => tracing::warn!(%error, "a control connection failed"),
                }
            }
        })
        .map_err(|e| format!("the control thread: {e}"))?;
    Ok(())
}

/// Answer one connection's commands, in order, until it closes.
fn answer(stream: UnixStream, commands: &mpsc::Sender<Request>) -> Result<(), String> {
    stream.set_nonblocking(false).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut writer = &stream;
    loop {
        let mut line = String::new();
        let read = (&mut reader)
            .take(MESSAGE_BYTES_MAX as u64)
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        if read == 0 {
            return Ok(());
        }
        let reply = match serde_json::from_str::<Command>(&line) {
            Ok(command) => {
                let (reply, replied) = oneshot::channel();
                match commands.blocking_send((command, reply)) {
                    Ok(()) => replied.blocking_recv().unwrap_or_else(|_| stopped()),
                    Err(_) => stopped(),
                }
            }
            Err(error) => Reply::Error {
                error: ControlError::new(ErrorCode::NotACommand, format!("not a command: {error}")),
            },
        };
        let mut text = serde_json::to_string(&reply).map_err(|e| e.to_string())?;
        text.push('\n');
        writer
            .write_all(text.as_bytes())
            .map_err(|e| e.to_string())?;
        // A line past the bound was cut, and the rest of it is no command.
        if !line.ends_with('\n') {
            return Err(format!("a command past {MESSAGE_BYTES_MAX} bytes"));
        }
    }
}

/// Wake a listener blocked on `socket` so it sees the strategies gone. A
/// listener already gone has nothing to wake.
fn wake(socket: &Path) {
    drop(UnixStream::connect(socket));
}

fn stopped() -> Reply {
    Reply::Error {
        error: ControlError::new(ErrorCode::Stopped, "the runtime's strategies stopped"),
    }
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
    /// Its market's trades, read each step into `market.tape`. `None` if
    /// the exchange file couldn't be read for them: it steps on, blind to
    /// the tape.
    tape: Option<super::journal::TapeReader>,
}

#[derive(Debug, Clone)]
struct Stats {
    state: StrategyState,
    steps: u64,
    skipped: u64,
    /// The slot its next step is due at.
    due: u64,
    last_step_ms: Option<u64>,
    sends_landed: u64,
    sends_failed: u64,
    last_send_error: Option<String>,
    last_slot: Option<u64>,
    position: Option<f64>,
    pnl: Option<f64>,
    drawdown: Option<f64>,
}

impl Stats {
    /// A strategy just added: paused until its component starts, nothing
    /// stepped or sent yet.
    fn added() -> Self {
        Self {
            state: StrategyState::Paused {
                why: "added".into(),
            },
            steps: 0,
            skipped: 0,
            due: 0,
            last_step_ms: None,
            sends_landed: 0,
            sends_failed: 0,
            last_send_error: None,
            last_slot: None,
            position: None,
            pnl: None,
            drawdown: None,
        }
    }
}

/// What a strategy is started with, apart from the manager: cloned onto
/// a task, so that an `add`'s slow work (its files, its accounts from the
/// node, its component) doesn't hold the other strategies' steps.
#[derive(Clone)]
struct Ctx {
    config: ControlConfig,
    shared: Shared,
    tpu: Option<Arc<super::tpu::Tpu>>,
}

impl Ctx {
    /// Read `spec`'s files and accounts, and start its component: the
    /// strategy, paused unless it runs, or stopped if its component didn't
    /// start (kept, so the desired state still holds it). `restored` is
    /// the component's hash when the desired state brings it back.
    async fn prepare(
        &self,
        spec: AddStrategy,
        restored: Option<String>,
        slot: u64,
    ) -> Result<Strategy, ControlError> {
        let (cap, max_drawdown) = limits(&spec.mandate, &spec.symbol).map_err(invalid)?;
        let component =
            std::fs::read(&spec.component).map_err(|e| unreadable(&spec.component, e))?;
        let component_sha256 = format!("{:x}", Sha256::digest(&component));
        let keypair = super::read_keypair(&spec.keypair)
            .map_err(|e| ControlError::new(ErrorCode::Unreadable, e))?;
        let authority = solana_signer::Signer::pubkey(&keypair);
        let exchange =
            std::fs::read_to_string(&spec.exchange).map_err(|e| unreadable(&spec.exchange, e))?;
        let exchange =
            serde_json::from_str(&exchange).map_err(|e| unreadable(&spec.exchange, e))?;
        let reader = PerpReader::new(exchange, &spec.symbol, &authority).map_err(invalid)?;
        // A shadow run's key may have no account on the venue: flat, since
        // it sends nothing.
        let reader = match spec.shadow {
            true => reader.flat_without_account(),
            false => reader,
        };
        self.backfill(&reader)
            .await
            .map_err(|e| ControlError::new(ErrorCode::StartFailed, e))?;
        let strategy = Strategy {
            component_sha256: component_sha256.clone(),
            authority,
            keypair: Arc::new(keypair),
            reader,
            slot: Arc::new(AtomicU64::new(slot)),
            cap,
            max_drawdown,
            inner: Arc::new(tokio::sync::Mutex::new(Inner {
                bot: None,
                k: 0,
                observed: Observed::default(),
                tape: match self.tape_reader(&spec) {
                    Ok(reader) => Some(reader),
                    Err(error) => {
                        tracing::warn!(strategy = spec.name, %error, "its market's tape can't be read; it steps without it");
                        None
                    }
                },
            })),
            stats: Arc::new(Mutex::new(Stats::added())),
            spec: spec.clone(),
        };
        let state = match restored {
            Some(hash) if hash != component_sha256 => Err(format!(
                "its component changed since it was added (sha256 {hash}, now {component_sha256})"
            )),
            _ => self.start_bot(&spec, &strategy, slot).await,
        };
        match state {
            Ok(bot) => {
                strategy.inner.lock().await.bot = Some(bot);
                let mut stats = lock(&strategy.stats);
                stats.due = slot;
                if spec.run {
                    stats.state = StrategyState::Running;
                }
            }
            Err(why) => {
                // Kept, stopped, so the desired state still holds it: the
                // add is done, and the event says it can't step.
                self.shared.events.emit(Event::Stopped {
                    strategy: spec.name.clone(),
                    why: why.clone(),
                });
                lock(&strategy.stats).state = StrategyState::Stopped { why };
            }
        }
        Ok(strategy)
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
        // A shadow reads its strict paper book's position as its own.
        let paper_position = spec
            .shadow
            .then(|| Arc::new(super::journal::PaperPosition::default()));
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
                // The tape: its market's trades since its last step.
                reports: mtx_kit::Reports {
                    tape: true,
                    ..mtx_kit::Reports::default()
                },
                step_deadline: Some(Duration::from_millis(self.config.step_deadline_ms)),
            },
            end_slot,
            cap: strategy.cap,
            max_drawdown: strategy.max_drawdown,
            reader: strategy.reader.clone(),
            cache: self.shared.cache.clone(),
            slot: strategy.slot.clone(),
            readings: self.shared.readings.clone(),
            log: Box::new(log),
            sends: self.sends(spec)?,
            paper_position: paper_position.clone(),
            references: (!self.config.references.is_empty())
                .then(|| self.shared.references.clone()),
            dispatch: self.dispatch(spec),
            observer: Some(Arc::new(
                super::journal::Journal::open(
                    &self.config.logs,
                    &spec.name,
                    spec.shadow,
                    Some(self.tape_reader(spec)?),
                    self.shared.events.clone(),
                )?
                .sharing_position_if(paper_position.clone())
                .behind()?,
            )),
        })
        .await
    }

    /// Where a strategy's journal reads its market's trades.
    fn tape_reader(&self, spec: &AddStrategy) -> Result<super::journal::TapeReader, String> {
        self.tape_reader_for(&spec.exchange, &spec.symbol)
    }

    /// Where `symbol`'s trades are read, from the `/exchange` at `exchange`.
    fn tape_reader_for(
        &self,
        exchange: &Path,
        symbol: &str,
    ) -> Result<super::journal::TapeReader, String> {
        let json = std::fs::read_to_string(exchange)
            .map_err(|e| format!("{}: {e}", exchange.display()))?;
        let venue = mtx_kit::venue::PhoenixVenue::from_json(&json)?;
        let market =
            mtx_kit::protocols::phoenix::eternal::PhoenixExchange::new(venue.exchange().clone())
                .market(symbol)
                .map_err(|e| e.to_string())?;
        Ok(super::journal::TapeReader::new(
            self.shared.watch.clone(),
            market,
        ))
    }

    /// Whether a strategy's orders wait to land: never through a shadow or
    /// the TPU, which watch for a send's fate after it; the node's own
    /// simulation first only where the TPU is told to (`preflight`).
    fn dispatch(&self, spec: &AddStrategy) -> mtx_kit::protocols::phoenix::eternal::Dispatch {
        use mtx_kit::protocols::phoenix::eternal::Dispatch;
        match (&self.tpu, &self.config.tpu) {
            _ if spec.shadow => Dispatch::FireAndForget { simulate: false },
            (Some(_), Some(config)) => Dispatch::FireAndForget {
                simulate: config.preflight,
            },
            _ => Dispatch::Confirm,
        }
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
            return Ok(Some(Arc::new(super::shadow::ShadowChain::live(
                rpc(),
                &journal,
                &spec.name,
                self.shared.blockhash.clone(),
                Some(self.shared.progress.clone()),
            )?)));
        }
        Ok(self.tpu.as_ref().map(|tpu| {
            Arc::new(super::tpu::TpuChainClient::new(
                rpc(),
                tpu.clone(),
                &spec.name,
                self.shared.blockhash.clone(),
                self.shared.events.clone(),
            )) as Arc<dyn mtx_kit::chain::ChainClient>
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
}

/// An `add` prepared off the strategies' loop, and where its answer goes.
struct Prepared {
    spec: AddStrategy,
    outcome: Result<Strategy, ControlError>,
    reply: oneshot::Sender<Reply>,
}

struct Manager {
    config: ControlConfig,
    shared: Shared,
    /// The TPU client strategies send through, if the runtime sends so.
    tpu: Option<Arc<super::tpu::Tpu>>,
    strategies: BTreeMap<String, Strategy>,
    /// Strategies the desired state holds that aren't running here: waiting
    /// for the node to catch up, or that
    /// couldn't be restored, and why. Kept in the desired state, so a
    /// restart that can't bring one back never forgets it.
    held: Vec<(Stored, Option<String>)>,
    /// The latest slot the feed reported.
    slot: u64,
    /// The trades of each market a strategy runs on, by symbol, read once
    /// for the event socket however many strategies share the market.
    tapes: BTreeMap<String, super::journal::TapeReader>,
    /// The markets watched without trading them (`control.watch`).
    watching: Option<super::watching::Watching>,
    /// Strategies being added, off the loop: their names are taken.
    adding: std::collections::BTreeSet<String>,
    /// Where an `add`'s task hands back the strategy it prepared.
    prepared: (mpsc::Sender<Prepared>, Option<mpsc::Receiver<Prepared>>),
}

impl Manager {
    fn new(config: ControlConfig, shared: Shared, tpu: Option<Arc<super::tpu::Tpu>>) -> Self {
        Self {
            config,
            shared,
            tpu,
            strategies: BTreeMap::new(),
            held: Vec::new(),
            slot: 0,
            tapes: BTreeMap::new(),
            watching: None,
            adding: std::collections::BTreeSet::new(),
            prepared: {
                let (sender, receiver) = mpsc::channel(STRATEGIES_MAX);
                (sender, Some(receiver))
            },
        }
    }

    fn ctx(&self) -> Ctx {
        Ctx {
            config: self.config.clone(),
            shared: self.shared.clone(),
            tpu: self.tpu.clone(),
        }
    }

    /// Tell the event socket the trades on every market a strategy runs
    /// on, since the last slot. A market whose exchange file can't be read
    /// has no trades to tell, and its strategy fails on the same file.
    /// Start watching `control.watch`'s markets: their accounts into the
    /// feed and the cache, their trades onto the event stream. A market that
    /// can't be read is left out, and said so.
    async fn start_watching(&mut self) {
        let Some(config) = self.config.watch.clone() else {
            return;
        };
        let venue = match std::fs::read_to_string(&config.exchange)
            .map_err(|e| format!("{}: {e}", config.exchange.display()))
            .and_then(|json| mtx_kit::venue::PhoenixVenue::from_json(&json))
        {
            Ok(venue) => venue,
            Err(error) => {
                tracing::error!(%error, "the watched markets can't be listed; none is watched");
                return;
            }
        };
        let (mut markets, mut infos) = (Vec::new(), Vec::new());
        for symbol in super::watching::symbols(&venue, &config.symbols) {
            let reader =
                match PerpReader::new(venue.exchange().clone(), &symbol, &Pubkey::default()) {
                    Ok(reader) => reader.flat_without_account(),
                    Err(error) => {
                        tracing::warn!(symbol, %error, "a watched market can't be read");
                        continue;
                    }
                };
            if let Err(error) = self.ctx().backfill(&reader).await {
                tracing::warn!(symbol, %error, "a watched market's accounts weren't found");
                continue;
            }
            if !self.tapes.contains_key(&symbol) {
                match self.ctx().tape_reader_for(&config.exchange, &symbol) {
                    Ok(tape) => {
                        self.tapes.insert(symbol.clone(), tape);
                    }
                    Err(error) => {
                        tracing::warn!(symbol, %error, "a watched market's trades can't be read")
                    }
                }
            }
            if let Some(info) = venue
                .markets()
                .into_iter()
                .find(|m| m.instrument.symbol == symbol)
            {
                infos.push(info);
            }
            markets.push((symbol, reader));
        }
        tracing::info!(
            markets = markets.len(),
            every_slots = config.every_slots,
            "watching markets"
        );
        self.watching = Some(super::watching::Watching::new(
            markets,
            infos,
            config.every_slots,
        ));
    }

    /// Read this slot's share of the watched markets, each told as a
    /// `quote`: only when someone listens.
    fn emit_quotes(&mut self, slot: u64) {
        let Some(watching) = self.watching.as_mut() else {
            return;
        };
        for (symbol, reader) in watching.due() {
            let reading = {
                let cache = lock(&self.shared.cache);
                reader.read(&*cache, slot)
            };
            match reading {
                Ok(reading) => self.shared.events.emit(Event::Quote {
                    symbol: symbol.clone(),
                    slot,
                    best_bid: reading.best_bid,
                    best_ask: reading.best_ask,
                    mark: reading.mark,
                }),
                Err(error) => tracing::debug!(symbol, %error, "a watched market wasn't read"),
            }
        }
    }

    fn emit_trades(&mut self) {
        let wanted: Vec<AddStrategy> = self
            .strategies
            .values()
            .filter(|s| !self.tapes.contains_key(&s.spec.symbol))
            .map(|s| s.spec.clone())
            .collect();
        for spec in wanted {
            match self.ctx().tape_reader(&spec) {
                Ok(reader) => {
                    self.tapes.insert(spec.symbol.clone(), reader);
                }
                Err(error) => {
                    tracing::warn!(symbol = spec.symbol, %error, "a market's trades can't be read");
                }
            }
        }
        for (symbol, tape) in &mut self.tapes {
            for trade in tape.trades("events") {
                self.shared.events.emit(Event::Trade {
                    symbol: symbol.clone(),
                    slot: trade.slot,
                    taker: match trade.taker {
                        mtx_kit::Side::Bid => EventSide::Buy,
                        mtx_kit::Side::Ask => EventSide::Sell,
                    },
                    price: trade.price,
                    size: trade.size,
                });
            }
        }
    }

    /// Read the desired state: its strategies wait in `held` until the
    /// node has caught up ([`Manager::restore_held`]): a strategy's start
    /// reads the market, which a node still replaying has behind the tip.
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
        if stored.len() > STRATEGIES_MAX {
            // Only a state written by hand, or under a larger bound, holds
            // more: what is past the bound is not restored, and the next
            // write of the desired state forgets it.
            tracing::error!(
                stored = stored.len(),
                max = STRATEGIES_MAX,
                "the desired state holds more strategies than a runtime runs; the rest are dropped"
            );
        }
        self.held = stored
            .into_iter()
            .take(STRATEGIES_MAX)
            .map(|stored| (stored, None))
            .collect();
    }

    /// Whether the node says it has caught up with the cluster (its RPC's
    /// `getHealth`): its accounts are then the chain's, and a strategy can
    /// start on them.
    async fn node_is_healthy(&self) -> bool {
        // An error or no answer in time is a node not to start on yet: the
        // held strategies are asked about again in HEALTH_EVERY_SLOTS.
        RpcChainClient::with_timeout(&self.config.rpc_url, HEALTH_TIMEOUT)
            .rpc()
            .get_health()
            .await
            .is_ok()
    }

    /// Start every held strategy that hasn't been tried: once the node has
    /// caught up. One that fails stays held, stopped, with the reason.
    async fn restore_held(&mut self) {
        let waiting: Vec<Stored> = self
            .held
            .iter()
            .filter(|(_, why)| why.is_none())
            .map(|(stored, _)| stored.clone())
            .collect();
        self.held.retain(|(_, why)| why.is_some());
        for stored in waiting {
            let name = stored.spec.name.clone();
            match self
                .add(stored.spec.clone(), Some(stored.component_sha256.clone()))
                .await
            {
                Ok(()) => tracing::info!(name, "a strategy was restored"),
                Err(error) => {
                    tracing::warn!(name, %error, "a strategy couldn't be restored");
                    self.held
                        .push((stored, Some(format!("not restored: {error}"))));
                }
            }
        }
    }

    async fn serve(
        &mut self,
        mut slots: watch::Receiver<u64>,
        mut commands: mpsc::Receiver<Request>,
    ) {
        let mut prepared = self.prepared.1.take().expect("the strategies serve once");
        loop {
            tokio::select! {
                done = prepared.recv() => {
                    if let Some(done) = done {
                        self.finish_add(done);
                    }
                }
                changed = slots.changed() => {
                    if changed.is_err() {
                        return;
                    }
                    let slot = *slots.borrow_and_update();
                    self.slot = self.slot.max(slot);
                    if self.held.iter().any(|(_, why)| why.is_none())
                        && slot.is_multiple_of(HEALTH_EVERY_SLOTS)
                        && self.node_is_healthy().await
                    {
                        self.restore_held().await;
                        if let Err(error) = self.persist() {
                            tracing::error!(%error, "the desired state can't be written");
                        }
                    }
                    self.step(self.slot).await;
                    // After the steps are on their way: decoding the tape
                    // and the watched markets for the event stream is no
                    // strategy's business.
                    self.emit_trades();
                    self.emit_quotes(self.slot);
                }
                request = commands.recv() => {
                    let Some((command, reply)) = request else {
                        return;
                    };
                    if let Command::Add(spec) = command {
                        self.begin_add(spec, reply);
                        continue;
                    }
                    let answer = self.handle(command).await;
                    // A client that hung up doesn't need the answer.
                    let _ = reply.send(answer);
                }
            }
        }
    }

    /// Step every running strategy whose step is due at `slot`, each on a
    /// task of its own: unless the feed is stale, which pauses them all.
    async fn step(&mut self, slot: u64) {
        let woke = std::time::Instant::now();
        self.report_feed(slot);
        if let Some(why) = self.feed_staleness() {
            self.pause_running_for_feed(why).await;
            return;
        }
        for (name, strategy) in &self.strategies {
            let Some(inner) = self.take_due(name, strategy, slot) else {
                continue;
            };
            let task = StepTask {
                name: name.clone(),
                slot,
                woke,
                // The step's clock starts as its slot reaches the strategies.
                started: std::time::Instant::now(),
                clock: self.shared.progress.clocks().get(slot),
                progress: self.shared.progress.clone(),
                events: self.shared.events.clone(),
                slot_cell: strategy.slot.clone(),
                stats: strategy.stats.clone(),
                mandate: strategy.spec.mandate.clone(),
                close_on_breach: strategy.spec.close_on_breach,
                instrument: instrument_of(&strategy.spec.symbol),
                shadow: strategy.spec.shadow,
            };
            let span = tracing::info_span!(
                "strategy.step",
                strategy = %name,
                slot,
                k = inner.k,
                shadow = task.shadow,
                answered = tracing::field::Empty,
                step_ms = tracing::field::Empty,
            );
            tokio::spawn(tracing::Instrument::instrument(task.run(inner), span));
        }
    }

    /// Tell the event stream of `slot`, and the metrics every few slots.
    fn report_feed(&self, slot: u64) {
        let progress = &self.shared.progress;
        self.shared.events.emit(Event::Slot {
            slot,
            behind: progress.slots_behind(),
            dropped: progress.dropped(),
        });
        if slot.is_multiple_of(FEED_METRICS_EVERY_SLOTS) {
            tracing::info!(
                gauge.mtx_feed_slot = slot,
                gauge.mtx_feed_slots_behind = progress.slots_behind(),
                gauge.mtx_feed_dropped = progress.dropped(),
                gauge.mtx_strategies = self.strategies.len() as u64,
                "feed"
            );
        }
    }

    /// Why nothing should be traded on the cache now, if so: the feed
    /// dropped an event, or lags the chain past [`FEED_SLOTS_BEHIND_MAX`].
    fn feed_staleness(&self) -> Option<String> {
        let progress = &self.shared.progress;
        match (progress.dropped(), progress.slots_behind()) {
            (0, behind) if behind <= FEED_SLOTS_BEHIND_MAX => None,
            (0, behind) => Some(format!("the feed is {behind} slots behind the chain")),
            (dropped, _) => Some(format!(
                "the feed dropped {dropped} events: the cache may be stale"
            )),
        }
    }

    /// The enforcer: pause every running strategy, cancelling its orders,
    /// because the feed is stale (`why`).
    async fn pause_running_for_feed(&self, why: String) {
        let running: Vec<String> = self
            .strategies
            .iter()
            .filter(|(_, s)| lock(&s.stats).state == StrategyState::Running)
            .map(|(name, _)| name.clone())
            .collect();
        for name in running {
            tracing::warn!(
                counter.mtx_enforcer_pauses = 1_u64,
                strategy = %name,
                reason = "feed",
                why = %why,
                "the enforcer pauses a strategy"
            );
            self.shared.events.emit(Event::Paused {
                strategy: name.clone(),
                reason: PauseReason::Feed,
                why: why.clone(),
            });
            if let Err(error) = self.pause(&name, OnPause::Cancel, why.clone()).await {
                tracing::warn!(name, %error, "a strategy couldn't be paused");
            }
        }
    }

    /// `strategy`'s component, locked for a step at `slot`, if it runs and
    /// its step is due: its next one is then set. A step still under way
    /// skips this one, counted, never queued.
    fn take_due(
        &self,
        name: &str,
        strategy: &Strategy,
        slot: u64,
    ) -> Option<tokio::sync::OwnedMutexGuard<Inner>> {
        let mut stats = lock(&strategy.stats);
        if stats.state != StrategyState::Running || slot < stats.due {
            return None;
        }
        stats.due = next_due(slot, strategy.spec.slots_per_step);
        assert!(stats.due > slot, "a strategy's next step is after this one");
        let Ok(inner) = strategy.inner.clone().try_lock_owned() else {
            stats.skipped += 1;
            tracing::warn!(
                counter.mtx_strategy_steps_skipped = 1_u64,
                strategy = %name,
                slot,
                "a step is skipped: the one before still runs"
            );
            self.shared.events.emit(Event::StepSkipped {
                strategy: name.to_string(),
                slot,
            });
            return None;
        };
        Some(inner)
    }

    async fn handle(&mut self, command: Command) -> Reply {
        let kind = command.kind();
        let span = tracing::info_span!("control.command", command = kind.as_str());
        let reply = tracing::Instrument::instrument(self.handle_in_span(command), span).await;
        self.report(kind, &reply);
        reply
    }

    /// Tell the logs, the metrics and the event stream what a command that
    /// changes something came to.
    fn report(&self, kind: CommandKind, reply: &Reply) {
        if !kind.changes() {
            return;
        }
        let error = match reply {
            Reply::Error { error } => Some(error.clone()),
            Reply::Ok { .. } | Reply::Markets { .. } => None,
        };
        match &error {
            Some(error) => tracing::warn!(
                counter.mtx_control_commands = 1_u64,
                command = kind.as_str(),
                outcome = "error",
                code = ?error.code,
                error = %error.error,
                "a command was refused"
            ),
            None => tracing::info!(
                counter.mtx_control_commands = 1_u64,
                command = kind.as_str(),
                outcome = "ok",
                "a command was done"
            ),
        }
        self.shared.events.emit(Event::Command {
            command: kind,
            error,
        });
    }

    /// The answer to a command done: the feed and every strategy.
    fn ok(&self) -> Reply {
        Reply::Ok {
            feed: FeedStatus {
                slot: self.slot,
                behind: self.shared.progress.slots_behind(),
                dropped: self.shared.progress.dropped(),
            },
            strategies: self.status(),
        }
    }

    async fn handle_in_span(&mut self, command: Command) -> Reply {
        if matches!(command, Command::Markets) {
            return Reply::Markets {
                markets: self
                    .watching
                    .as_ref()
                    .map(|w| w.markets().to_vec())
                    .unwrap_or_default(),
            };
        }
        // Only a command that changes something rewrites the desired state.
        let changes = !matches!(command, Command::Status);
        let done = match command {
            Command::Status | Command::Markets => Ok(()),
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
            Command::Upgrade { name, component } => self.upgrade(&name, component).await,
            Command::Add(spec) => self.add(spec, None).await,
            Command::Remove { name } => self.remove(&name).await,
        };
        let persisted = done.and_then(|()| match changes {
            true => self
                .persist()
                .map_err(|e| ControlError::new(ErrorCode::NotPersisted, e)),
            false => Ok(()),
        });
        match persisted {
            Ok(()) => self.ok(),
            Err(error) => Reply::Error { error },
        }
    }

    /// Do `action` to `name`, or to every strategy.
    async fn each<'a, F, Fut>(&'a mut self, name: Option<String>, mut action: F) -> Done
    where
        F: FnMut(&'a Self, &'a str) -> Fut,
        Fut: std::future::Future<Output = Done> + 'a,
    {
        let this: &'a Self = self;
        match name {
            Some(name) => {
                let (name, _) = this
                    .strategies
                    .get_key_value(&name)
                    .ok_or_else(|| unknown(&name))?;
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

    async fn pause_by_command(&self, name: &str, orders: OnPause) -> Done {
        self.pause(name, orders, "paused".to_string()).await
    }

    /// Stop stepping `name`, at its step boundary, and do `orders` to its
    /// resting orders. A stopped strategy stays stopped.
    async fn pause(&self, name: &str, orders: OnPause, why: String) -> Done {
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
                bot.flatten(FLATTEN_SLIPPAGE).await.map_err(|e| {
                    ControlError::new(
                        ErrorCode::Failed,
                        format!("the position couldn't be closed: {e}"),
                    )
                })
            }
        }
    }

    async fn resume(&self, name: &str) -> Done {
        let strategy = self.get(name)?;
        let mut stats = lock(&strategy.stats);
        if let StrategyState::Paused { .. } = stats.state {
            stats.state = StrategyState::Running;
            stats.due = self.slot;
        }
        Ok(())
    }

    /// Restart `name` under `mandate`, paused or running as it was.
    async fn change_mandate(&mut self, name: &str, mandate: Mandate) -> Done {
        mandate.validate().map_err(invalid)?;
        let (cap, max_drawdown) =
            limits(&mandate, &self.get(name)?.spec.symbol).map_err(invalid)?;
        let start_slot = self.slot;
        let strategy = self.get(name)?;
        let mut spec = strategy.spec.clone();
        spec.mandate = mandate;
        let mut inner = strategy.inner.lock().await;
        if let Some(mut bot) = inner.bot.take() {
            bot.end().await;
        }
        let bot = self.ctx().start_bot(&spec, strategy, start_slot).await;
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
            return Err(ControlError::new(ErrorCode::StartFailed, why));
        }
        Ok(())
    }

    /// Swap `name`'s component: end it, start the new one in its place
    /// (same name, key, mandate and state), and if that fails start the old
    /// one again, so a bad build costs a restart, not the strategy.
    async fn upgrade(&mut self, name: &str, component: PathBuf) -> Done {
        let bytes = std::fs::read(&component).map_err(|e| unreadable(&component, e))?;
        let component_sha256 = format!("{:x}", Sha256::digest(&bytes));
        let start_slot = self.slot;
        let strategy = self.get(name)?;
        let mut spec = strategy.spec.clone();
        spec.component = component;
        let mut inner = strategy.inner.lock().await;
        if let Some(mut bot) = inner.bot.take() {
            bot.end().await;
        }
        // A component that didn't load or whose `start` trapped still
        // makes a bot, stopped: for an upgrade, that is a failed start.
        let started = match self.ctx().start_bot(&spec, strategy, start_slot).await {
            Ok(mut bot) => match bot.stopped.clone() {
                Some(why) => {
                    bot.end().await;
                    Err(why)
                }
                None => Ok(bot),
            },
            Err(why) => Err(why),
        };
        let (bot, failed) = match started {
            Ok(bot) => (Ok(bot), None),
            Err(why) => {
                tracing::warn!(strategy = name, %why, "the new component didn't start; the old one starts again");
                let old = self
                    .ctx()
                    .start_bot(&strategy.spec, strategy, start_slot)
                    .await;
                (old, Some(why))
            }
        };
        let stopped = bot.as_ref().err().cloned();
        inner.bot = bot.ok();
        inner.k = 0;
        drop(inner);
        let strategy = self
            .strategies
            .get_mut(name)
            .expect("the strategy was got above");
        if failed.is_none() {
            strategy.spec = spec;
            strategy.component_sha256 = component_sha256;
        }
        if let Some(why) = stopped {
            lock(&strategy.stats).state = StrategyState::Stopped { why: why.clone() };
        }
        match failed {
            Some(why) => Err(ControlError::new(
                ErrorCode::StartFailed,
                format!("the new component didn't start ({why}); the old one runs again"),
            )),
            None => Ok(()),
        }
    }

    /// Compile, start and keep `spec`: paused unless it runs. `restored`
    /// is the component's hash when the desired state brings it back.
    async fn add(&mut self, spec: AddStrategy, restored: Option<String>) -> Done {
        self.check_add(&spec, restored.is_some())?;
        let name = spec.name.clone();
        let strategy = self.ctx().prepare(spec, restored, self.slot).await?;
        self.strategies.insert(name, strategy);
        Ok(())
    }

    /// What an `add` can be refused for at once, before its slow work: a
    /// name taken (by a strategy, one held, or one being added), no room,
    /// a malformed step or mandate.
    fn check_add(&self, spec: &AddStrategy, restoring: bool) -> Done {
        let held = self
            .held
            .iter()
            .any(|(stored, _)| stored.spec.name == spec.name);
        if self.strategies.contains_key(&spec.name)
            || self.adding.contains(&spec.name)
            || (!restoring && held)
        {
            return Err(ControlError::new(
                ErrorCode::Exists,
                format!("a strategy is already named {}", spec.name),
            ));
        }
        // Held strategies count: the desired state keeps them, and a restart
        // brings back at most STRATEGIES_MAX of what it holds.
        if self.strategies.len() + self.adding.len() + self.held.len() >= STRATEGIES_MAX {
            return Err(ControlError::new(
                ErrorCode::TooMany,
                format!("a runtime runs at most {STRATEGIES_MAX} strategies"),
            ));
        }
        if spec.slots_per_step == 0 {
            return Err(invalid(
                "a strategy steps every slot at most: slots_per_step is at least 1".into(),
            ));
        }
        spec.mandate.validate().map_err(invalid)?;
        limits(&spec.mandate, &spec.symbol).map_err(invalid)?;
        Ok(())
    }

    /// Start an `add` off the loop: refused at once if it can be, else
    /// prepared on a task while the other strategies step, and finished
    /// ([`Manager::finish_add`]) when it is ready.
    fn begin_add(&mut self, spec: AddStrategy, reply: oneshot::Sender<Reply>) {
        if let Err(error) = self.check_add(&spec, false) {
            let answer = Reply::Error { error };
            self.report(CommandKind::Add, &answer);
            let _ = reply.send(answer);
            return;
        }
        self.adding.insert(spec.name.clone());
        let (ctx, slot, prepared) = (self.ctx(), self.slot, self.prepared.0.clone());
        tokio::spawn(async move {
            let outcome = ctx.prepare(spec.clone(), None, slot).await;
            // The manager gone (the runtime stopping): nothing to finish.
            let _ = prepared
                .send(Prepared {
                    spec,
                    outcome,
                    reply,
                })
                .await;
        });
    }

    /// An `add` prepared off the loop: kept, written down, and answered.
    fn finish_add(&mut self, prepared: Prepared) {
        let Prepared {
            spec,
            outcome,
            reply,
        } = prepared;
        self.adding.remove(&spec.name);
        let answer = match outcome {
            Ok(strategy) => {
                {
                    // It was prepared at an earlier slot: it steps from now.
                    let mut stats = lock(&strategy.stats);
                    stats.due = stats.due.max(self.slot);
                }
                self.strategies.insert(spec.name.clone(), strategy);
                match self.persist() {
                    Ok(()) => self.ok(),
                    Err(e) => Reply::Error {
                        error: ControlError::new(ErrorCode::NotPersisted, e),
                    },
                }
            }
            Err(error) => Reply::Error { error },
        };
        self.report(CommandKind::Add, &answer);
        // A client that hung up doesn't need the answer.
        let _ = reply.send(answer);
    }

    /// Pause `name`, cancel its orders, `end` it and forget it.
    async fn remove(&mut self, name: &str) -> Done {
        let held = self.held.len();
        self.held.retain(|(stored, _)| stored.spec.name != name);
        if self.held.len() < held {
            return Ok(());
        }
        self.pause(name, OnPause::Cancel, "removed".into()).await?;
        let strategy = self.strategies.remove(name).expect("paused above");
        let mut inner = strategy.inner.lock().await;
        if let Some(mut bot) = inner.bot.take() {
            bot.end().await;
        }
        Ok(())
    }

    fn get(&self, name: &str) -> Result<&Strategy, ControlError> {
        self.strategies.get(name).ok_or_else(|| unknown(name))
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
                    sends_landed: stats.sends_landed,
                    sends_failed: stats.sends_failed,
                    last_send_error: stats.last_send_error,
                    steps: stats.steps,
                    skipped: stats.skipped,
                    last_slot: stats.last_slot,
                    position: stats.position,
                    pnl: stats.pnl,
                    drawdown: stats.drawdown,
                }
            })
            .chain(
                self.held
                    .iter()
                    .map(|(stored, why)| held_status(stored, why)),
            )
            .collect()
    }

    /// Write the desired state: each strategy's spec, its component's
    /// hash, and whether it runs. Written beside the file and renamed over
    /// it, so a crash leaves the old state or the new, never half of one.
    fn persist(&self) -> Result<(), String> {
        assert!(
            self.strategies.len() + self.held.len() <= STRATEGIES_MAX,
            "an add counts the running and the held strategies"
        );
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
                .chain(self.held.iter().map(|(stored, _)| stored.clone()))
                .collect(),
        };
        let text = serde_json::to_string_pretty(&state).map_err(|e| e.to_string())?;
        let path = &self.config.state;
        let partial = path.with_extension("partial");
        std::fs::write(&partial, text).map_err(|e| format!("{}: {e}", partial.display()))?;
        std::fs::rename(&partial, path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// One strategy's step, run on a task of its own off the strategies' loop:
/// what it needs of the manager, cloned.
struct StepTask {
    name: String,
    slot: u64,
    /// When the strategies' loop woke for `slot`.
    woke: std::time::Instant,
    /// When this step was handed its task: what `step_ms` counts from.
    started: std::time::Instant,
    /// When the validator told of `slot`, and the feed applied it.
    clock: Option<SlotClock>,
    progress: FeedProgress,
    events: super::events::Events,
    /// The slot its market is read at.
    slot_cell: Arc<AtomicU64>,
    stats: Arc<Mutex<Stats>>,
    mandate: Mandate,
    close_on_breach: bool,
    instrument: InstrumentRef,
    shadow: bool,
}

/// What a step came to, for its strategy's stats and the event stream.
struct Stepped {
    answered: bool,
    took_ms: u64,
    timings: StepTimings,
    market: Option<mtx_kit::Market>,
}

impl StepTask {
    /// Step the component once, record what it did, and enforce the
    /// mandate on what it left: a breach pauses it, cancels its orders,
    /// and closes its position if its spec says so.
    async fn run(self, mut inner: tokio::sync::OwnedMutexGuard<Inner>) {
        let inner = &mut *inner;
        let Some(bot) = inner.bot.as_mut() else {
            return;
        };
        let k = inner.k;
        let stepped = self.turn(bot, inner.tape.as_mut(), k).await;
        inner.k += 1;
        if let Some(market) = &stepped.market {
            observe(&mut inner.observed, &self.instrument, market);
        }
        let breach = self.mandate.audit(&inner.observed);
        let paused = self.record(bot, k, &stepped, breach.as_ref());
        if paused {
            for failed in bot.cancel_all().await {
                tracing::warn!(name = self.name, failed, "an order couldn't be cancelled");
            }
            if self.close_on_breach
                && let Err(error) = bot.flatten(FLATTEN_SLIPPAGE).await
            {
                tracing::warn!(name = self.name, %error, "the position couldn't be closed");
            }
        }
    }

    /// The component's turn `k`, on the cache once the feed has applied
    /// what came before `slot` (or after [`SETTLE_WITHIN`], on what it holds).
    async fn turn(
        &self,
        bot: &mut WasmBot,
        tape: Option<&mut super::journal::TapeReader>,
        k: u64,
    ) -> Stepped {
        let settling = std::time::Instant::now();
        let settled = tokio::task::block_in_place(|| self.progress.settle(SETTLE_WITHIN));
        let settle_us = micros(settling);
        if !settled {
            tracing::warn!(
                name = self.name,
                slot = self.slot,
                "the feed is behind; the step reads what the cache holds"
            );
        }
        self.slot_cell.store(self.slot, Ordering::Release);
        let mut step = mtx_core::Step::new(k, self.slot, false);
        if let Some(tape) = tape {
            step.tape =
                super::journal::step_tape(&tape.trades(&self.name), &self.instrument.to_string());
        }
        let answered = bot.turn(&step, None).await;
        let ended = std::time::Instant::now();
        let took_ms = u64::try_from(self.started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let timings = step_timings(self.clock, self.woke, settle_us, bot.last_timings(), ended);
        let current = tracing::Span::current();
        current.record("answered", answered);
        current.record("step_ms", took_ms);
        Stepped {
            answered,
            took_ms,
            timings,
            market: bot.last_market().cloned(),
        }
    }

    /// Fold step `k` into the strategy's stats, stop or pause it if it must,
    /// and tell the metrics and the event stream. Whether the enforcer
    /// paused it for `breach`.
    fn record(
        &self,
        bot: &WasmBot,
        k: u64,
        stepped: &Stepped,
        breach: Option<&mtx_core::mandate::Breach>,
    ) -> bool {
        let name = &self.name;
        let mut stats = lock(&self.stats);
        stats.steps += u64::from(stepped.answered);
        stats.last_step_ms = Some(stepped.took_ms);
        if let Some(market) = &stepped.market {
            record_market(&mut stats, name, market);
        }
        if let Some(why) = &bot.stopped
            && !matches!(stats.state, StrategyState::Stopped { .. })
        {
            stats.state = StrategyState::Stopped { why: why.clone() };
            self.events.emit(Event::Stopped {
                strategy: name.clone(),
                why: why.clone(),
            });
        }
        let paused = match (breach, &stats.state) {
            (Some(breach), StrategyState::Running) => {
                self.pause_for_breach(&mut stats, breach);
                true
            }
            _ => false,
        };
        tracing::info!(
            counter.mtx_strategy_steps = 1_u64,
            histogram.mtx_strategy_step_ms = stepped.took_ms,
            gauge.mtx_strategy_position = stats.position.unwrap_or(0.0),
            gauge.mtx_strategy_pnl_usd = stats.pnl.unwrap_or(0.0),
            gauge.mtx_strategy_drawdown_usd = stats.drawdown.unwrap_or(0.0),
            gauge.mtx_strategy_running = u64::from(stats.state == StrategyState::Running),
            strategy = %name,
            shadow = self.shadow,
            answered = stepped.answered,
            "a strategy stepped"
        );
        self.events.emit(Event::Step {
            strategy: name.clone(),
            slot: self.slot,
            k,
            step_ms: stepped.took_ms,
            timings: stepped.timings,
            answered: stepped.answered,
            shadow: self.shadow,
            orders: bot.last_orders(),
            position: stats.position.unwrap_or(0.0),
            pnl_usd: stats.pnl.unwrap_or(0.0),
            drawdown_usd: stats.drawdown.unwrap_or(0.0),
            running: stats.state == StrategyState::Running,
        });
        paused
    }

    /// The enforcer pauses a running strategy whose step breached its
    /// mandate.
    fn pause_for_breach(&self, stats: &mut Stats, breach: &mtx_core::mandate::Breach) {
        assert_eq!(
            stats.state,
            StrategyState::Running,
            "only a running strategy is paused"
        );
        tracing::warn!(
            counter.mtx_enforcer_pauses = 1_u64,
            strategy = %self.name,
            reason = "mandate",
            breach = %breach,
            "the enforcer pauses a strategy"
        );
        self.events.emit(Event::Paused {
            strategy: self.name.clone(),
            reason: PauseReason::Mandate,
            why: breach.to_string(),
        });
        stats.state = StrategyState::Paused {
            why: format!("the enforcer: {breach}"),
        };
    }
}

/// What a step left, into what the enforcer has seen of its strategy: the
/// position, the worst drawdown, and which sides rest orders.
fn observe(observed: &mut Observed, instrument: &InstrumentRef, market: &mtx_kit::Market) {
    observed.record(instrument, market.position.size);
    observed.max_drawdown_usd = observed.max_drawdown_usd.max(market.drawdown);
    if market.resting_bids > 0.0 {
        observed.record_open_order(instrument, mtx_core::Side::Buy);
    }
    if market.resting_asks > 0.0 {
        observed.record_open_order(instrument, mtx_core::Side::Sell);
    }
}

/// What became of the step before's orders, as this step's market reports
/// them, and where the strategy stands: into its stats and the metrics.
fn record_market(stats: &mut Stats, strategy: &str, market: &mtx_kit::Market) {
    for send in &market.sends {
        tracing::info!(
            counter.mtx_strategy_sends = 1_u64,
            strategy,
            outcome = send_outcome(&send.state),
            "a send's fate"
        );
        match &send.state {
            mtx_kit::SendState::Landed(_) => stats.sends_landed += 1,
            mtx_kit::SendState::Failed { reason, .. } | mtx_kit::SendState::NotSent(reason) => {
                stats.sends_failed += 1;
                // The step's first failure, not the orders it stopped
                // after it.
                if reason != crate::desk::EARLIER_ORDER_FAILED {
                    stats.last_send_error = Some(reason.clone());
                }
            }
            mtx_kit::SendState::Delayed | mtx_kit::SendState::Expired => {}
        }
    }
    stats.last_slot = Some(market.slot);
    stats.position = Some(market.position.size);
    stats.pnl = Some(market.pnl);
    stats.drawdown = Some(market.drawdown);
}

/// A send's fate, as the metrics label it.
fn send_outcome(state: &mtx_kit::SendState) -> &'static str {
    match state {
        mtx_kit::SendState::Landed(_) => "landed",
        mtx_kit::SendState::Failed { .. } => "failed",
        mtx_kit::SendState::NotSent(_) => "not_sent",
        mtx_kit::SendState::Delayed => "delayed",
        mtx_kit::SendState::Expired => "expired",
    }
}

/// A held strategy, as status shows it: stopped, waiting for the node, or
/// not restored.
fn held_status(stored: &Stored, why: &Option<String>) -> StrategyStatus {
    let spec = &stored.spec;
    // A held strategy's mandate passed its add's check once; one that no
    // longer reads (a mandate format that changed since) shows no limits,
    // and it is held all the same, never run.
    let (cap, max_drawdown) = limits(&spec.mandate, &spec.symbol).unwrap_or((0.0, 0.0));
    StrategyStatus {
        name: spec.name.clone(),
        state: StrategyState::Stopped {
            why: why
                .clone()
                .unwrap_or_else(|| "waiting for the node to catch up".into()),
        },
        symbol: spec.symbol.clone(),
        authority: String::new(),
        cap,
        max_drawdown,
        shadow: spec.shadow,
        last_step_ms: None,
        sends_landed: 0,
        sends_failed: 0,
        last_send_error: None,
        steps: 0,
        skipped: 0,
        last_slot: None,
        position: None,
        pnl: None,
        drawdown: None,
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

/// The slot a strategy that stepped at `slot` steps next. Saturating: a
/// `slots_per_step` as large as a command may send would otherwise wrap in
/// a release build to a slot already past, and step it every slot.
fn next_due(slot: u64, slots_per_step: u64) -> u64 {
    assert!(slots_per_step > 0, "an add refuses a step of no slots");
    slot.saturating_add(slots_per_step)
}

fn micros(since: std::time::Instant) -> u64 {
    u64::try_from(since.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn between(from: std::time::Instant, to: std::time::Instant) -> u64 {
    u64::try_from(to.saturating_duration_since(from).as_micros()).unwrap_or(u64::MAX)
}

/// A step's timings: from the validator's notice of its slot (`clock`),
/// through the strategies waking (`woke`), the feed settling, the turn's
/// stages, to the turn's end. Without the slot's clock (a slot older than
/// the clocks keep), the feed's stages are 0 and the total starts at the
/// wake.
fn step_timings(
    clock: Option<SlotClock>,
    woke: std::time::Instant,
    settle_us: u64,
    turn: TurnTimings,
    ended: std::time::Instant,
) -> StepTimings {
    let (notified, applied) = match clock {
        Some(clock) => (clock.notified, clock.applied.unwrap_or(clock.notified)),
        None => (woke, woke),
    };
    let timings = StepTimings {
        feed_us: between(notified, applied),
        wake_us: between(applied, woke),
        settle_us,
        read_us: turn.read_us,
        call_us: turn.call_us,
        send_us: turn.send_us,
        total_us: between(notified, ended),
    };
    debug_assert!(
        timings.total_us + 1_000
            >= timings.feed_us
                + timings.wake_us
                + timings.read_us
                + timings.call_us
                + timings.send_us,
        "the stages fit in the total: {timings:?}"
    );
    timings
}

/// `symbol`'s perpetual, as mandates name it.
fn instrument_of(symbol: &str) -> InstrumentRef {
    InstrumentRef {
        venue: crate::venue::perps::VENUE.into(),
        kind: mtx_core::mandate::Kind::Perp,
        symbol: symbol.into(),
    }
}

/// Every cancel went through, or why not.
fn cancelled(failed: Vec<String>) -> Done {
    match failed.is_empty() {
        true => Ok(()),
        false => Err(ControlError::new(
            ErrorCode::OrdersNotCancelled,
            format!("orders not cancelled: {}", failed.join("; ")),
        )),
    }
}

/// What a command's work comes to: done, or refused with a code.
type Done = Result<(), ControlError>;

fn unknown(name: &str) -> ControlError {
    ControlError::new(ErrorCode::UnknownStrategy, format!("no strategy {name}"))
}

fn invalid(error: String) -> ControlError {
    ControlError::new(ErrorCode::Invalid, error)
}

fn unreadable(path: &Path, error: impl std::fmt::Display) -> ControlError {
    ControlError::new(
        ErrorCode::Unreadable,
        format!("{}: {error}", path.display()),
    )
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

    fn spec(name: &str) -> AddStrategy {
        AddStrategy {
            name: name.into(),
            component: "/bots/carry.wasm".into(),
            mandate: Mandate::perp("phoenix", "SOL", 1.0, 100.0),
            keypair: "/keys/carry.json".into(),
            exchange: "/data/exchange.json".into(),
            symbol: "SOL".into(),
            slots_per_step: 2,
            run: true,
            close_on_breach: false,
            shadow: false,
        }
    }

    fn manager() -> Manager {
        let config: ControlConfig = serde_json::from_value(serde_json::json!({
            "socket": "/nonexistent/control.sock",
            "state": "/nonexistent/strategies.json",
            "rpc_url": "http://127.0.0.1:1",
            "logs": "/nonexistent/logs",
        }))
        .unwrap();
        let (_, receiver) = super::super::feed::channel(Arc::default());
        let shared = Shared {
            cache: Arc::default(),
            watch: Arc::default(),
            progress: receiver.progress(),
            events: super::super::events::Events::default(),
            blockhash: Arc::default(),
            readings: Arc::default(),
            references: Arc::default(),
        };
        Manager::new(config, shared, None)
    }

    /// A strategy held because it couldn't be restored still takes a place:
    /// the desired state never holds more than a restart brings back.
    #[test]
    fn held_strategies_count_against_the_bound() {
        let mut manager = manager();
        assert_eq!(manager.check_add(&spec("new"), false), Ok(()));
        manager.held = (0..STRATEGIES_MAX)
            .map(|n| {
                let stored = Stored {
                    spec: spec(&format!("held-{n}")),
                    component_sha256: "ab".into(),
                };
                (stored, Some("not restored".into()))
            })
            .collect();
        let refused = manager.check_add(&spec("new"), false).unwrap_err();
        assert_eq!(refused.code, ErrorCode::TooMany);
        manager.held.pop();
        assert_eq!(manager.check_add(&spec("new"), false), Ok(()));
        // A held name is taken, unless it is the one being restored.
        let taken = manager.check_add(&spec("held-0"), false).unwrap_err();
        assert_eq!(taken.code, ErrorCode::Exists);
    }

    #[test]
    fn a_strategy_s_next_step_is_after_this_one_whatever_its_period() {
        assert_eq!(next_due(100, 2), 102);
        assert_eq!(next_due(100, u64::MAX), u64::MAX);
        assert!(next_due(u64::MAX - 1, 5) > u64::MAX - 1);
    }

    #[test]
    fn cancels_that_failed_say_which() {
        assert_eq!(cancelled(Vec::new()), Ok(()));
        let error = cancelled(vec!["phoenix:perp:SOL: refused".into()]).unwrap_err();
        assert_eq!(error.code, ErrorCode::OrdersNotCancelled);
        assert!(error.error.contains("phoenix:perp:SOL"), "{error}");
    }

    #[test]
    fn a_step_s_stages_fit_in_its_total() {
        let notified = std::time::Instant::now();
        let applied = notified + std::time::Duration::from_micros(300);
        let woke = applied + std::time::Duration::from_micros(200);
        let ended = woke + std::time::Duration::from_micros(2_000);
        let clock = SlotClock {
            slot: 9,
            notified,
            applied: Some(applied),
        };
        let turn = TurnTimings {
            read_us: 100,
            call_us: 1_200,
            send_us: 600,
        };
        let timings = step_timings(Some(clock), woke, 50, turn, ended);
        assert_eq!(
            (timings.feed_us, timings.wake_us, timings.total_us),
            (300, 200, 2_500)
        );
        assert_eq!(
            (timings.read_us, timings.call_us, timings.send_us),
            (100, 1_200, 600)
        );
        // A slot the clocks no longer keep: the step is timed from its wake.
        let timings = step_timings(None, woke, 0, turn, ended);
        assert_eq!(
            (timings.feed_us, timings.wake_us, timings.total_us),
            (0, 0, 2_000)
        );
    }
}
