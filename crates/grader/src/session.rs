//! One trading session: a bot against one scenario on a fresh offline world,
//! the same loop whatever the scenario trades.
//!
//! 1. The session's kind sets it up ([`crate::perp`], [`crate::spot`]):
//!    restore the snapshot offline and put the RPC guard in front of it,
//!    fund the bot's account (a perp's trader with [`DEPOSIT_USDC`] of
//!    collateral, a spot wallet), ready the actors ([`crate::actors`]), step
//!    to the session's first slot, register the scenario and lock the
//!    cheatcodes.
//! 2. Start the bot (`just run`, from the `Justfile` in its directory, with
//!    the [`BotEnv`] contract), or load its component ([`crate::wasm`]).
//! 3. Move the world in steps of [`SLOTS_PER_STEP`] slots, and between them
//!    give the bot its turn (see [`mtx_core::turns`]): the takers trade;
//!    each step produces its blocks, which land the scenario's move; then
//!    the actors that react act; the session measures the account
//!    (its `Account`); then the bot gets the step and the world waits for it.
//! 4. After the last step, stop the bot and mark the account to market.
//!
//! Each session gets its own surfnet, so nothing a bot does carries over, and
//! nothing moves while the bot thinks, so the same bot on the same scenario
//! trades the same session every time. The surfnet has no clock at all,
//! setup included: a slot passes only when the grader produces one, so every
//! setup transaction and override lands on the same slot however long the
//! machine takes over it.
//!
//! A bot that never answers the hello runs the old way: the world moves at
//! the slot time, whatever the bot does.

use std::{
    collections::BTreeMap,
    os::{fd::AsRawFd, unix::net::UnixStream as StdUnixStream},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use mtx_core::{
    Decimal,
    mandate::{InstrumentRef, Mandate, Observed, PositionRange},
    turns::{self, Hello, Message, Step},
};
use mtx_kit::BotEnv;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;
use solana_keypair::Keypair;
use solana_rpc_client::rpc_client::RpcClient;
use surfpool_sdk::SimnetCommand;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, Command},
};
use tracing::{info, warn};

use crate::{
    Error, Result,
    actors::{ActorsReport, Cast, CompeteReport, FlowReport, Moment, TakerRecord},
    chain,
    error::io_error,
    guard::{CallLog, Guard},
    phoenix::SLOTS_BETWEEN_STEPS,
    scenario::Scenario,
    snapshot,
};

/// Slots each step moves the world: the scenario's own step, so every move
/// lands on a step.
pub const SLOTS_PER_STEP: u64 = SLOTS_BETWEEN_STEPS;
/// How long the world waits for the bot's `ready`, and for each `done`.
pub(crate) const HELLO_WALL: Duration = Duration::from_secs(10);
pub(crate) const TURN_WALL: Duration = Duration::from_secs(5);
/// A trusted process bot's walls, for its hello and each turn, with no
/// session budget (see [`SessionOptions::trusted`]): there only so a hung
/// anchor can't hang the grade.
pub(crate) const TRUSTED_WALL: Duration = Duration::from_secs(120);
/// What all of a bot's turns in a session may take together, per step: the
/// slot time, so a slow bot is no slower than the world used to be. Past it,
/// the world stops waiting.
pub(crate) const TURN_BUDGET_PER_STEP: Duration = Duration::from_millis(500);

/// How long each of the grader's own requests to its surfnet may take (the
/// clients' default is 30 s). The world doesn't move while the grader waits,
/// so a starved machine makes a request slow, never different: it is waited
/// out, not failed. The bound is only for a surfnet that died.
pub const INTERNAL_RPC_TIMEOUT: Duration = Duration::from_secs(600);
/// How many times the grader makes one of its reads through a client it
/// can't give a longer wait (Phoenix's Hawkeye simulations), before failing.
const READ_ATTEMPTS: u32 = 5;

/// Collateral each session's trader starts with.
pub const DEPOSIT_USDC: f64 = 10_000.0;
/// The unprivileged account bots run as when the grader runs as root.
/// Unix users bots run as when the grader runs as root, one per session that
/// can run at once (see [`SessionOptions::slot`]). Each session kills every
/// process its user owns when it ends, so two sessions never share one.
pub const BOT_USERS: [&str; 4] = ["mtx-bot", "mtx-bot-2", "mtx-bot-3", "mtx-bot-4"];
/// How long a bot gets to exit after SIGTERM before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(2);

pub struct SessionOptions<'a> {
    pub snapshot_dir: &'a Path,
    pub scenario: &'a Scenario,
    pub bot_dir: &'a Path,
    /// Where the bot's stdout and stderr go.
    pub bot_log: &'a Path,
    /// Which of [`BOT_USERS`] a process bot runs as. Sessions that run at the
    /// same time must use different slots. `None` for a strategy component:
    /// it runs inside the grader, with no files and no network, so it needs
    /// no user, and any number of its sessions can run at once.
    pub slot: Option<usize>,
    /// The session's regime, handed to the bot as `MTX_REGIME` (JSON): only
    /// for the ladder's regime oracle, never for a graded bot.
    pub regime: Option<&'a str>,
    /// Upkeep for the venues the session trades on, called every step
    /// (see [`crate::crank`]).
    pub cranks: Option<&'a crate::crank::CrankFactory>,
    /// A trusted process bot (an anchor, a ladder rung): the world waits for
    /// each of its turns, up to [`TRUSTED_WALL`], rather than the turn wall
    /// and the session's budget, so a loaded machine can't make it miss a
    /// turn. Components run on fuel either way.
    pub trusted: bool,
    /// The session's keys: the bot's, the takers' and the maker's (see
    /// [`crate::keys`]).
    pub keys: crate::keys::SessionKeyring<'a>,
    /// Watches a component's session step by step (`mtx bot run --debug`).
    pub observer: Option<std::sync::Arc<dyn crate::wasm::StepObserver>>,
}

/// A session's outcome, as the grade reports it. It reads back from the
/// report, so a grade can be scored again from what it wrote (see
/// [`crate::grade::rescore`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReport {
    pub scenario: String,
    pub start_mark_usd: f64,
    pub end_mark_usd: f64,
    /// Collateral plus unrealized PnL plus wallet USDC, at the start and end.
    pub start_value_usd: f64,
    pub end_value_usd: f64,
    pub pnl_usd: f64,
    /// Position held at the end, in base units (e.g. SPCX shares). Positive is long.
    pub end_position: f64,
    pub liquidatable: bool,
    /// Calls the RPC guard refused, in order.
    pub refused_calls: Vec<String>,
    /// How the bot process ended, if it exited on its own before the session did.
    pub bot_exit: Option<String>,
    /// Slots that arrived late while the bot ran: clock ticks Surfpool
    /// coalesced. A session's surfnet has no clock (see
    /// [`snapshot::start_offline_stepped`]), so this is always 0: kept so
    /// reports keep their shape.
    pub late_slots: u64,
    /// The mandate the session ran under, with its term: what the bot was
    /// handed, and what [`SessionReport::observed`] is audited against.
    pub mandate: Mandate,
    /// The scenario's market, and the largest position, long or short, in
    /// base units, the bot held in it while it ran.
    pub instrument: InstrumentRef,
    pub peak_position: f64,
    /// The lowest and highest position the bot held in each instrument the
    /// meter reported, what [`SessionReport::observed`] audits.
    pub positions: Vec<PositionRange>,
    /// The most positions or orders the bot held at once in other markets.
    pub positions_outside_universe: u32,
    /// The deepest the account's value fell below its running peak while the
    /// bot ran, in USD.
    pub max_drawdown_usd: f64,
    /// The price the account's position was valued at: the `mark`, or for
    /// market making the `oracle` the scenario sets, which a bot's own
    /// quotes can't move.
    #[serde(deserialize_with = "static_name")]
    pub valuation: StaticName,
    /// Market making: what the grader's takers sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow: Option<FlowReport>,
    /// Market making: what the grader's own maker did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub competitor: Option<CompeteReport>,
    /// What the scenario's library actors did (see
    /// [`crate::actors::library`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actors: Option<crate::actors::LibraryReport>,
    /// A spot session: the wallet at the start and the end, and every fill.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spot: Option<crate::spot::SpotReport>,
    /// Execution faults: the plan, and what fired.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub faults: Option<crate::faults::FaultsReport>,
    /// A spot session with an execution order: how the bot did against it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<crate::orders::OrderReport>,
    /// A spot session that prices participation: what the charge came to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub participation: Option<crate::orders::ParticipationReport>,
    /// A spot session scored net of the starting holding's drift.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub net: Option<crate::orders::NetReport>,
    /// Venue upkeep the venue refused on chain (see [`crate::crank`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crank_refusals: Vec<crate::crank::CrankRefusal>,
    /// Each anchored oracle feed's start, against its account's own price.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub oracle_starts: BTreeMap<String, crate::oracles::OracleStart>,
    /// How the bot's turns went.
    pub turns: TurnReport,
    /// The session's wall time, setup included, in milliseconds.
    pub wall_ms: u64,
    /// The session step by step, for review. Kept out of the report: the
    /// grader writes it apart, once no bot is left running (see
    /// [`crate::grade::GradeOptions::detail_dir`]).
    #[serde(skip)]
    pub timeline: Timeline,
}

/// A session step by step: the market and the account at every step, what
/// the bot sent and what became of it, and the grader's own orders.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Timeline {
    /// The session's slots: it starts at `start_slot`, steps every
    /// `slots_per_step` and ends at `end_slot`. Every `at` below counts slots
    /// from `start_slot`.
    pub start_slot: u64,
    pub end_slot: u64,
    pub slots_per_step: u64,
    pub start_mark_usd: f64,
    pub steps: Vec<StepRecord>,
    /// The bot's transactions, as the surfnet executed them.
    pub transactions: Vec<TxRecord>,
    /// What the bot asked the RPC guard for.
    pub calls: CallLog,
    /// Market making: the grader's taker orders, as sent.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub taker_orders: Vec<TakerRecord>,
}

/// The world and the account after a step's blocks landed, before the bot's
/// turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StepRecord {
    pub k: u64,
    pub at: u64,
    pub mark_usd: f64,
    /// Phoenix's index (oracle) price.
    pub index_usd: f64,
    /// The scenario's planned level: Phoenix's index.
    pub level_usd: f64,
    /// Where the scenario puts the book's makers, when it gives the book a
    /// premium over the level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub book_usd: Option<f64>,
    /// The scenario's fair value, for a scenario that reports one (see
    /// [`Scenario::fair_path`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fair_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_bid_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub best_ask_usd: Option<f64>,
    /// The bot's position, in base units; positive is long.
    pub position: f64,
    /// The account's value as the session values it, and its PnL so far.
    pub value_usd: f64,
    pub pnl_usd: f64,
    /// The bot's turn after this step: how long it took and whether it
    /// answered in time. Absent when there was no turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answered: Option<bool>,
}

/// One of the bot's transactions, looked up once the session ended.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TxRecord {
    /// The turn the bot sent it in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    /// Slots into the session it executed at, if it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<u64>,
    pub ok: bool,
    /// The same signature as an earlier send: it didn't run again.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub duplicate: bool,
    /// Why it failed: the surfnet refusing it, or its execution error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Its instructions, decoded where they're Phoenix's.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub instructions: Vec<IxRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_units: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fee_lamports: Option<u64>,
    /// Its program logs, for failed transactions only.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<String>,
}

/// How a session's turns went.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct TurnReport {
    /// The bot never answered the hello: the world moved at the slot time.
    pub legacy: bool,
    pub steps: u64,
    /// Turns the world stopped waiting for, past the turn's wall or the
    /// session's budget.
    pub overran: u64,
    /// The bot's turns, all together, in milliseconds.
    pub wall_ms: u64,
    /// Sends refused because they came while the world moved.
    pub late_sends: usize,
    /// A component's fuel: what the session spent, and the most one call
    /// spent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fuel_used: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fuel_max_call: Option<u64>,
    /// Why a component played no more of the session, if it stopped: it
    /// didn't load, or it trapped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stopped: Option<String>,
    /// A component's lines that carried transaction options, which the
    /// session accepted and didn't apply (no session does yet).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_options: Option<u64>,
}

impl SessionReport {
    /// What the session measured, in the terms [`Mandate::audit`] checks.
    pub fn observed(&self) -> Observed {
        Observed {
            positions: self.positions.clone(),
            positions_outside_universe: self.positions_outside_universe,
            max_drawdown_usd: self.max_drawdown_usd,
            fills: self
                .spot
                .as_ref()
                .map(|spot| spot.observed_fills())
                .unwrap_or_default(),
            open_orders: self
                .spot
                .as_ref()
                .map(|spot| spot.open_orders.clone())
                .unwrap_or_default(),
        }
    }
}

/// Run one session. An error before the bot starts comes back as
/// [`Error::Setup`]: the bot hasn't seen anything of the session yet, so
/// the grader may run it again from scratch.
pub async fn run(opts: SessionOptions<'_>) -> Result<SessionReport> {
    let mut bot_started = false;
    let session = run_session(opts, &mut bot_started).await;
    match session {
        // Already wrapped: a crank's grader failure, after the bot started.
        Err(error @ Error::Setup(_)) => Err(error),
        Err(error) if !bot_started => Err(Error::Setup(Box::new(error))),
        done => done,
    }
}

/// The world a session runs on, kept for as long as it runs.
pub(crate) enum World {
    /// A Phoenix Eternal snapshot, with its exchange metadata.
    Offline(Box<snapshot::OfflineWorld>),
    /// A snapshot of other markets.
    Snapshot(Box<surfpool_sdk::Surfnet>),
}

impl World {
    fn surfnet(&self) -> &surfpool_sdk::Surfnet {
        match self {
            World::Offline(world) => &world.surfnet,
            World::Snapshot(surfnet) => surfnet,
        }
    }
}

/// A session set up by its kind, up to the bot's start: the world locked
/// at the session's first slot, the bot's endpoint, the actors, and the
/// account the session measures.
pub(crate) struct Prepared {
    pub world: World,
    /// The grader's own endpoint, and a client on it.
    pub internal: Guard,
    pub rpc: RpcClient,
    /// The bot's endpoint.
    pub guard: Guard,
    pub workdir: SessionDir,
    pub bot: Keypair,
    /// The slot the session starts at (its steps count from it) and ends at.
    pub base_slot: u64,
    pub end_slot: u64,
    /// The mandate with its term: what the bot is handed, and what the
    /// session's report is audited against.
    pub mandate: Mandate,
    /// The bot's environment, as a process bot gets it.
    pub env: BotEnv,
    /// What the steps carry beyond the clock, if anything.
    pub reports: Option<turns::Reports>,
    /// Off hours: the slot the external market reopens at.
    pub reopen_slot: Option<u64>,
    pub cast: Cast,
    pub account: Box<dyn Account>,
}

/// The bot's account as a session measures it, and what the session's
/// kind adds to the loop: a perp's Phoenix trader account
/// ([`crate::perp`]), or a wallet across spot venues ([`crate::spot`]).
#[async_trait]
pub(crate) trait Account: Send + Sync {
    /// Where the session stands `at` slots in, for the actors.
    fn moment(&self, at: u64) -> Moment;

    /// The account's value when the session starts.
    fn start_value_usd(&self) -> f64;

    /// Once the bot has started, before the first step.
    async fn begin(&mut self) -> Result<()> {
        Ok(())
    }

    /// After step `k`'s blocks landed, at `slot` (`at` slots in): measure
    /// the account and record the step.
    async fn step(&mut self, k: u64, slot: u64, at: u64) -> Result<StepRecord>;

    /// What the step hands the bot beyond the clock (a spot scenario's turn
    /// data).
    async fn turn_data(&mut self, step: &mut Step) -> Result<()>;

    /// The last step's reading of a measured wallet, for a component.
    fn wallet(&self) -> Option<&crate::spot::Wallet>;

    /// The markets a component trades through the grader, its seats signed
    /// with `bot` through `host_url` (the bot's guard, on its behalf).
    async fn desk(
        &self,
        host_url: &str,
        bot: &Keypair,
        env: &BotEnv,
        mandate: &Mandate,
    ) -> Result<crate::wasm::Desk>;

    /// One instruction of a bot's transaction, as the report shows it.
    fn decode(&self, ix: &serde_json::Value, keys: &[serde_json::Value]) -> IxRecord;

    /// The session is over: measure the account a last time and report.
    async fn finish(self: Box<Self>, done: Done) -> Result<SessionReport>;
}

/// What the session loop hands the account's report.
pub(crate) struct Done {
    pub began: Instant,
    pub start_slot: u64,
    pub end_slot: u64,
    pub mandate: Mandate,
    pub refused_calls: Vec<String>,
    pub bot_exit: Option<String>,
    pub late_slots: u64,
    pub faults: Option<crate::faults::FaultsReport>,
    pub turns: TurnReport,
    pub steps: Vec<StepRecord>,
    pub transactions: Vec<TxRecord>,
    pub calls: CallLog,
    pub actors: ActorsReport,
    pub crank_refusals: Vec<crate::crank::CrankRefusal>,
}

/// The one session loop, for every kind of session: perp, spot, and
/// whatever venue its kind trades.
/// A name the grader writes from its own set (a valuation, a fill's role, a
/// fault), as a report field: read back with [`static_name`]. An alias, so
/// that serde doesn't take the field for one borrowed from its input.
pub type StaticName = &'static str;

/// Reads back a [`StaticName`]: each distinct name is allocated once and
/// kept for the life of the process.
pub(crate) fn static_name<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<&'static str, D::Error> {
    static NAMES: std::sync::Mutex<std::collections::BTreeSet<&'static str>> =
        std::sync::Mutex::new(std::collections::BTreeSet::new());
    let name = String::deserialize(deserializer)?;
    let mut names = NAMES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(known) = names.get(name.as_str()) {
        return Ok(known);
    }
    let name: &'static str = Box::leak(name.into_boxed_str());
    names.insert(name);
    Ok(name)
}

async fn run_session(opts: SessionOptions<'_>, bot_started: &mut bool) -> Result<SessionReport> {
    let began = Instant::now();
    let scenario = opts.scenario;
    let Prepared {
        world,
        internal,
        rpc,
        guard,
        workdir,
        bot: bot_key,
        base_slot,
        end_slot,
        mandate,
        env,
        reports,
        reopen_slot,
        mut cast,
        mut account,
    } = if scenario.spot.is_some() {
        crate::spot::prepare(&opts).await?
    } else {
        crate::perp::prepare(&opts).await?
    };
    let surfnet = world.surfnet();
    if let Some(observer) = &opts.observer {
        observer.rpc(surfnet.rpc_url());
    }
    let start_slot = base_slot;
    guard.set_faults(scenario.faults.as_ref(), base_slot, SLOTS_PER_STEP);
    let hooks = cast.send_hooks();
    if !hooks.is_empty() {
        guard.set_send_hook(std::sync::Arc::new(crate::actors::Hooks(hooks)));
    }

    let late_before = surfnet.coalesced_slots();
    *bot_started = true;
    let steps = scenario.duration_slots.div_ceil(SLOTS_PER_STEP);
    let walls = Walls::new(opts.trusted, steps);
    let user = opts.slot.map(|slot| BOT_USERS[slot]);
    let mut turns = TurnReport {
        steps,
        ..TurnReport::default()
    };
    // A bot is a process speaking the turn protocol, or a component the
    // grader calls (`crate::wasm`, RFD 0002).
    let mut process = None;
    let mut wasm_bot = None;
    let legacy;
    if let Some(component) = crate::wasm::component_in(opts.bot_dir) {
        let log = std::fs::File::create(opts.bot_log).map_err(io_error(opts.bot_log))?;
        // A scenario that enables raw sends lists their programs: checked
        // when it loaded.
        let raw_programs = match &scenario.raw_sends {
            Some(raw) => Some(raw.programs().map_err(|reason| Error::InvalidScenario {
                path: scenario.name.clone().into(),
                reason: format!("raw_sends: {reason}"),
            })?),
            None => None,
        };
        let desk = account
            .desk(&guard.host_url(), &bot_key, &env, &mandate)
            .await?;
        wasm_bot = Some(
            crate::wasm::WasmBot::start(
                &component,
                &env.rpc_url,
                crate::wasm::Clock {
                    steps,
                    slots_per_step: SLOTS_PER_STEP,
                    start_slot,
                    scenario: &scenario.name,
                    vars: opts
                        .regime
                        .map(|r| ("MTX_REGIME".to_string(), r.to_string()))
                        .into_iter()
                        .chain(reopen_slot.map(|s| ("MTX_REOPEN_SLOT".to_string(), s.to_string())))
                        .chain(raw_programs.as_ref().map(|programs| {
                            let programs: Vec<String> =
                                programs.iter().map(ToString::to_string).collect();
                            ("MTX_RAW_PROGRAMS".to_string(), programs.join(","))
                        }))
                        .collect(),
                    reports: reports.into(),
                },
                crate::wasm::Terms {
                    end_slot,
                    mandate: &mandate,
                    cap: env.max_position,
                    max_drawdown: env.max_drawdown_usd,
                },
                desk,
                Box::new(log),
                opts.observer.clone(),
                raw_programs.map(|programs| crate::raw::RawConfig {
                    programs,
                    fresh: opts.keys.fresh(),
                    internal_url: internal.url().to_string(),
                    mandate: mandate.clone(),
                    instrument: scenario.instrument(),
                    unsettled_funding: scenario.funding.is_some(),
                }),
            )
            .await?,
        );
        legacy = false;
        info!(
            scenario = scenario.name,
            start_slot, end_slot, steps, "strategy component loaded"
        );
    } else {
        if user.is_none() {
            return Err(Error::NoBotUser);
        }
        let (ours, theirs) = StdUnixStream::pair().map_err(io_error("turn socket"))?;
        let child = spawn_bot(
            opts.bot_dir,
            &env,
            &workdir,
            opts.bot_log,
            theirs,
            opts.regime,
            reopen_slot,
        )?;
        ours.set_nonblocking(true)
            .map_err(io_error("turn socket"))?;
        let socket = UnixStream::from_std(ours).map_err(io_error("turn socket"))?;
        let (reader, mut writer) = socket.into_split();
        let mut reader = BufReader::new(reader).lines();
        info!(
            scenario = scenario.name,
            start_slot, end_slot, steps, "bot started"
        );
        let hello = Hello {
            protocol: turns::PROTOCOL.into(),
            steps,
            slots_per_step: SLOTS_PER_STEP,
            start_slot,
            end_slot,
            turn_wall_ms: walls.announced().as_millis() as u64,
            seed: Some(turns::scenario_seed(&scenario.name)),
            reports,
        };
        legacy = !(send(&mut writer, &Message::Hello(hello)).await
            && matches!(
                tokio::time::timeout(walls.hello(), next_message(&mut reader)).await,
                Ok(Some(Message::Ready))
            ));
        if legacy {
            info!("the bot didn't answer the hello: the world moves at the slot time");
        }
        process = Some((child, writer, reader));
    }
    turns.legacy = legacy;
    let mut spent = Duration::ZERO;
    account.begin().await?;

    let mut bot_exit = None;
    let mut steps_seen: Vec<StepRecord> = Vec::with_capacity(steps as usize);
    let mut cranks = crate::crank::Cranks::new(opts.cranks);
    let mut slot = start_slot;
    for k in 0..steps {
        // A step: the takers trade against the quotes the bot left; then the
        // world's blocks land (and with them the scenario's move); the
        // actors that react look and act; then the bot's turn. Takers
        // trading right after a move, before any maker could requote, would
        // pick off every quote: a sniper the task doesn't describe. Here the
        // toxic flow is the informed flow, ahead of the jumps.
        guard.set_turn(legacy);
        cast.before_blocks().await;
        let target = (slot + SLOTS_PER_STEP).min(end_slot);
        while slot < target {
            surfnet.send_command(SimnetCommand::SlotForward(None))?;
            slot = wait_for_slot(&rpc, slot + 1).await?;
        }
        let at = slot - base_slot;
        // The bot's held sends land in this step's blocks.
        guard.release(k).await;
        cranks
            .step(
                &rpc,
                &crate::crank::CrankStep {
                    k,
                    slot,
                    at,
                    last: k + 1 == steps,
                },
            )
            .await?;
        cast.after_blocks(account.moment(at)).await;
        steps_seen.push(account.step(k, slot, at).await?);
        if bot_exit.is_none()
            && let Some((child, _, _)) = process.as_mut()
            && let Some(status) = child.try_wait().map_err(io_error(opts.bot_dir))?
        {
            if status.success() {
                info!(%status, "bot exited before the session ended");
            } else {
                warn!(%status, "bot exited before the session ended");
            }
            bot_exit = Some(status.to_string());
        }
        if bot_exit.is_some() {
            continue;
        }

        // The bot's turn.
        guard.set_step(k);
        guard.set_turn(true);
        let mut step = Step::new(k, slot, k + 1 == steps);
        if let Some(bot) = wasm_bot.as_mut() {
            if let Some(now) = steps_seen.last() {
                let peak = steps_seen
                    .iter()
                    .map(|r| r.value_usd)
                    .fold(account.start_value_usd(), f64::max);
                let before = steps_seen
                    .len()
                    .checked_sub(2)
                    .map_or(0.0, |i| steps_seen[i].position);
                bot.measured(crate::wasm::Account {
                    value_usd: now.value_usd,
                    pnl_usd: now.pnl_usd,
                    drawdown_usd: peak - now.value_usd,
                    position: now.position,
                    position_change: now.position - before,
                    level_usd: now.level_usd,
                    valued_at_usd: match scenario.valuation() {
                        "oracle" => Some(now.level_usd),
                        "book" => now.book_usd,
                        _ => None,
                    },
                });
            }
            let started = Instant::now();
            account.turn_data(&mut step).await?;
            let answered = bot.turn(&step, account.wallet()).await;
            let took = started.elapsed();
            if !answered {
                turns.overran += 1;
            }
            turns.wall_ms += took.as_millis() as u64;
            if let Some(record) = steps_seen.last_mut() {
                record.turn_ms = Some(took.as_millis() as u64);
                record.answered = Some(answered);
            }
            continue;
        }
        let Some((_, writer, reader)) = process.as_mut() else {
            continue;
        };
        if legacy {
            tokio::time::sleep(TURN_BUDGET_PER_STEP).await;
            continue;
        }
        let started = Instant::now();
        account.turn_data(&mut step).await?;
        let answered = take_turn(writer, reader, step, walls.turn(spent)).await;
        let took = started.elapsed();
        spent += took;
        if !answered {
            turns.overran += 1;
        }
        turns.wall_ms += took.as_millis() as u64;
        if let Some(record) = steps_seen.last_mut() {
            record.turn_ms = Some(took.as_millis() as u64);
            record.answered = Some(answered);
        }
    }
    guard.set_turn(false);
    cast.end().await;
    if let Some((mut child, mut writer, _)) = process {
        let _ = send(&mut writer, &Message::End).await;
        stop_bot(&mut child, workdir.bot_user.and(user)).await;
    }
    if let Some(bot) = wasm_bot.as_mut() {
        bot.end().await;
        let (used, max_call) = bot.fuel();
        turns.fuel_used = Some(used);
        turns.fuel_max_call = Some(max_call);
        turns.stopped = bot.stopped.clone();
        turns.tx_options = (bot.tx_options > 0).then_some(bot.tx_options);
    }
    turns.late_sends = guard.late_sends();
    let late_slots = surfnet.coalesced_slots() - late_before;
    let calls = guard.call_log();
    let transactions = transactions(&rpc, &calls, base_slot, &|ix, keys| {
        account.decode(ix, keys)
    });
    let actors = cast.report().await;
    let report = account
        .finish(Done {
            began,
            start_slot,
            end_slot,
            mandate,
            refused_calls: guard.refused(),
            bot_exit,
            late_slots,
            faults: guard.faults_report(),
            turns,
            steps: steps_seen,
            transactions,
            calls,
            actors,
            crank_refusals: cranks.into_refusals(),
        })
        .await?;
    info!(
        scenario = report.scenario,
        pnl_usd = report.pnl_usd,
        refused = report.refused_calls.len(),
        overran = report.turns.overran,
        "session finished"
    );
    if let Some(observer) = &opts.observer {
        observer.keep(Box::new(world));
    }
    Ok(report)
}

/// One instruction of a bot's transaction.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct IxRecord {
    /// `phoenix`, `compute_budget`, or the program's address.
    pub program: String,
    /// The instruction, for the programs it knows (`PlaceLimitOrder`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// The order it places, for Phoenix's order instructions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<OrderRecord>,
}

/// An order as a Phoenix instruction carries it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct OrderRecord {
    /// `PostOnly`, `Limit` or `ImmediateOrCancel`.
    pub kind: String,
    /// `bid` or `ask`.
    pub side: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price_usd: Option<f64>,
    /// In base units.
    pub size: f64,
}

/// Look up each of the bot's sends: where it executed and how it went.
pub(crate) fn transactions(
    rpc: &RpcClient,
    calls: &CallLog,
    base_slot: u64,
    decode: &dyn Fn(&serde_json::Value, &[serde_json::Value]) -> IxRecord,
) -> Vec<TxRecord> {
    // The chain runs a transaction once: a second send of the same signed
    // transaction (the same instructions on the same blockhash) gets its
    // signature back and does nothing.
    let mut seen: std::collections::HashMap<String, Vec<IxRecord>> =
        std::collections::HashMap::new();
    calls
        .sends
        .iter()
        .map(|send| {
            let mut record = TxRecord {
                step: send.step,
                signature: send.signature.clone(),
                at: None,
                ok: false,
                duplicate: false,
                error: send.error.clone(),
                instructions: Vec::new(),
                compute_units: None,
                fee_lamports: None,
                logs: Vec::new(),
            };
            let Some(signature) = &send.signature else {
                return record;
            };
            if let Some(instructions) = seen.get(signature) {
                record.instructions = instructions.clone();
                record.error = Some(
                    "a duplicate of an earlier send: the same transaction doesn't run twice".into(),
                );
                record.duplicate = true;
                return record;
            }
            let found = chain::call(
                rpc,
                "getTransaction",
                json!([signature, { "encoding": "json", "commitment": "confirmed", "maxSupportedTransactionVersion": 0 }]),
            );
            let transaction = match found {
                Ok(transaction) if transaction.is_object() => transaction,
                Ok(_) => {
                    record.error = Some("not found: it never executed".into());
                    return record;
                }
                Err(error) => {
                    record.error = Some(format!("lookup failed: {error}"));
                    return record;
                }
            };
            let meta = &transaction["meta"];
            record.at = transaction["slot"].as_u64().map(|s| s.saturating_sub(base_slot));
            record.ok = meta["err"].is_null();
            if !record.ok {
                record.error = Some(meta["err"].to_string());
            }
            record.compute_units = meta["computeUnitsConsumed"].as_u64();
            record.fee_lamports = meta["fee"].as_u64();
            let logs: Vec<String> = meta["logMessages"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|line| line.as_str().map(str::to_string))
                .collect();
            let message = &transaction["transaction"]["message"];
            // A v0 transaction's keys go on with those its lookup tables
            // loaded, writable then read-only, as its indices count them.
            let mut keys = message["accountKeys"].as_array().cloned().unwrap_or_default();
            for loaded in ["writable", "readonly"] {
                keys.extend(
                    meta["loadedAddresses"][loaded]
                        .as_array()
                        .cloned()
                        .unwrap_or_default(),
                );
            }
            record.instructions = message["instructions"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|ix| decode(ix, &keys))
                .collect();
            if !record.ok {
                record.logs = logs;
            }
            seen.insert(signature.clone(), record.instructions.clone());
            record
        })
        .collect()
}

/// The grader's own read, made again if it fails for want of an answer (a
/// timeout, a lost connection: see [`is_transport`]), up to
/// [`READ_ATTEMPTS`] times. On a starved machine a read can time out on the
/// client's side, and reading again changes nothing, since nothing moves
/// while the grader reads. Any other error is the answer, and comes back at
/// once.
pub(crate) async fn patiently<T, E: std::error::Error + 'static, F>(
    what: &str,
    mut read: impl FnMut() -> F,
) -> std::result::Result<T, E>
where
    F: std::future::Future<Output = std::result::Result<T, E>>,
{
    let mut attempt = 1;
    loop {
        match read().await {
            Ok(value) => return Ok(value),
            Err(error) if attempt < READ_ATTEMPTS && is_transport(&error) => {
                warn!(what, attempt, %error, "a grader read got no answer; reading again");
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Whether `error` is a request that got no answer (it timed out, or its
/// connection failed) rather than an answer. Errors that keep their source
/// say so through reqwest's; some clients flatten theirs into text.
pub(crate) fn is_transport(error: &(dyn std::error::Error + 'static)) -> bool {
    const NO_ANSWER: [&str; 5] = [
        "timed out",
        "error sending request",
        "connection refused",
        "connection reset",
        "connection closed",
    ];
    let mut next = Some(error);
    while let Some(error) = next {
        if let Some(http) = error.downcast_ref::<reqwest::Error>()
            && (http.is_timeout() || http.is_connect() || http.is_request())
        {
            return true;
        }
        let text = error.to_string().to_lowercase();
        if NO_ANSWER.iter().any(|sign| text.contains(sign)) {
            return true;
        }
        next = error.source();
    }
    false
}

/// How long the world waits for a process bot: its hello, and each turn.
pub(crate) struct Walls {
    trusted: bool,
    /// What all of an untrusted bot's turns may take together.
    budget: Duration,
}

impl Walls {
    pub(crate) fn new(trusted: bool, steps: u64) -> Self {
        Self {
            trusted,
            budget: TURN_BUDGET_PER_STEP * steps as u32,
        }
    }

    /// The turn wall the hello announces.
    pub(crate) fn announced(&self) -> Duration {
        if self.trusted {
            TRUSTED_WALL
        } else {
            TURN_WALL
        }
    }

    pub(crate) fn hello(&self) -> Duration {
        if self.trusted {
            TRUSTED_WALL
        } else {
            HELLO_WALL
        }
    }

    /// The next turn's wall, after the bot's turns took `spent` so far.
    pub(crate) fn turn(&self, spent: Duration) -> Duration {
        if self.trusted {
            TRUSTED_WALL
        } else {
            TURN_WALL.min(self.budget.saturating_sub(spent))
        }
    }
}

/// Hand the bot `step` and wait up to `wall` for its `done`; false if it
/// didn't answer in time or its end is gone.
pub(crate) async fn take_turn(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    reader: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    step: Step,
    wall: Duration,
) -> bool {
    let k = step.k;
    send(writer, &Message::Step(step)).await
        && tokio::time::timeout(wall, async {
            loop {
                match next_message(reader).await {
                    Some(Message::Done { k: done }) if done == k => return true,
                    Some(_) => continue,
                    None => return false,
                }
            }
        })
        .await
        .unwrap_or(false)
}

/// Wait until the surfnet has produced `slot`: the grader asked for it,
/// so it comes however loaded the machine; the wall is only for a surfnet
/// that died.
pub(crate) async fn wait_for_slot(rpc: &RpcClient, slot: u64) -> Result<u64> {
    let deadline = Instant::now() + SLOT_WALL;
    loop {
        let now = rpc.get_slot()?;
        if now >= slot {
            return Ok(now);
        }
        if Instant::now() > deadline {
            return Err(Error::Timeout(format!("slot {slot} (at {now})")));
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

const SLOT_WALL: Duration = Duration::from_secs(60);

/// Send one turn message; false if the bot's end is gone.
pub(crate) async fn send(writer: &mut tokio::net::unix::OwnedWriteHalf, message: &Message) -> bool {
    let mut line = serde_json::to_string(message).expect("turn messages serialize");
    line.push('\n');
    writer.write_all(line.as_bytes()).await.is_ok()
}

/// The bot's next turn message; `None` once its end is gone. Lines that
/// aren't messages are skipped.
pub(crate) async fn next_message(
    reader: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
) -> Option<Message> {
    loop {
        let line = reader.next_line().await.ok()??;
        if let Ok(message) = serde_json::from_str(&line) {
            return Some(message);
        }
    }
}

/// A meter's amount as the grader's `f64`: the nearest double to the exact
/// decimal, which is what dividing Phoenix's integer lots in `f64` gave.
pub(crate) fn grader_f64(amount: Decimal) -> f64 {
    amount
        .to_string()
        .parse()
        .expect("a decimal's digits parse as f64")
}

/// A private directory for the bot: its keypair and its `HOME`.
pub(crate) struct SessionDir {
    pub(crate) path: PathBuf,
    pub(crate) bot_user: Option<(u32, u32)>,
}

impl SessionDir {
    /// A directory for a bot that runs as `user` (when the grader runs as
    /// root), or that only the grader enters.
    pub(crate) fn create(bot: &Keypair, user: Option<&str>) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("mtx-session-{}", uuid::Uuid::new_v4()));
        // Only the bot's own user may enter: sessions graded at once run
        // their bots as different users in one container, and a bot must not
        // read another's keypair or mandate.
        std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700)
            .create(&path)
            .map_err(io_error(&path))?;
        let dir = Self {
            path,
            bot_user: user.and_then(bot_user),
        };
        let keypair = serde_json::to_string(&bot.to_bytes().to_vec())?;
        let mut file = std::fs::OpenOptions::new();
        std::os::unix::fs::OpenOptionsExt::mode(file.write(true).create_new(true), 0o600);
        std::io::Write::write_all(
            &mut file
                .open(dir.keypair_path())
                .map_err(io_error(dir.keypair_path()))?,
            keypair.as_bytes(),
        )
        .map_err(io_error(dir.keypair_path()))?;
        if let Some((uid, gid)) = dir.bot_user {
            for path in [dir.path.clone(), dir.keypair_path()] {
                std::os::unix::fs::chown(&path, Some(uid), Some(gid)).map_err(io_error(&path))?;
            }
        }
        Ok(dir)
    }

    pub(crate) fn keypair_path(&self) -> PathBuf {
        self.path.join("keypair.json")
    }

    pub(crate) fn write_mandate(&self, mandate: &Mandate) -> Result<PathBuf> {
        let path = self.path.join("mandate.json");
        mandate.save(&path)?;
        if let Some((uid, gid)) = self.bot_user {
            std::os::unix::fs::chown(&path, Some(uid), Some(gid)).map_err(io_error(&path))?;
        }
        Ok(path)
    }
}

impl Drop for SessionDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// When the grader runs as root (as in the task's verifier), bots run as one
/// of [`BOT_USERS`] so they can't read the hidden scenarios or signal the
/// grader. Run unprivileged, bots run as the current user.
fn bot_user(user: &str) -> Option<(u32, u32)> {
    // SAFETY: getuid has no preconditions; getpwnam is given a valid C string
    // and its result is read before any other passwd call.
    unsafe {
        if libc::getuid() != 0 {
            return None;
        }
        let name = std::ffi::CString::new(user).expect("no NUL in user name");
        let entry = libc::getpwnam(name.as_ptr());
        if entry.is_null() {
            warn!(
                user,
                "running as root without a bot user; the bot runs as root"
            );
            return None;
        }
        Some(((*entry).pw_uid, (*entry).pw_gid))
    }
}

/// Where graders on a machine hold the bot users their sessions run as:
/// a fixed path, not `TMPDIR`, so that every grader finds it.
const BOT_USER_LOCKS: &str = "/tmp/mtx-bot-users";

/// One of [`BOT_USERS`], held for a session against every grader on the
/// machine. A session's end kills every process its bot user owns (a bot can
/// leave its process group, not its user), so two grades running at once
/// must never run bots as the same user at the same time: a session that
/// wants a user another grade holds waits for it. Only a grader running as
/// root runs bots as these users; otherwise holding one is a no-op.
pub struct BotUserLock {
    /// The locked file, closed (and so let go) on drop.
    _held: Option<std::fs::File>,
}

impl BotUserLock {
    pub async fn acquire(user: &str) -> Result<Self> {
        // SAFETY: getuid has no preconditions.
        if unsafe { libc::getuid() } != 0 {
            return Ok(Self { _held: None });
        }
        let dir = Path::new(BOT_USER_LOCKS);
        std::os::unix::fs::DirBuilderExt::mode(std::fs::DirBuilder::new().recursive(true), 0o700)
            .create(dir)
            .map_err(io_error(dir))?;
        Ok(Self {
            _held: Some(lock(&dir.join(format!("{user}.lock"))).await?),
        })
    }
}

/// An exclusive lock on the file at `path`, waited for: released when the
/// file is closed, by the process ending too.
async fn lock(path: &Path) -> Result<std::fs::File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(io_error(path))?;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::os::fd::AsRawFd;
        // SAFETY: flock on a descriptor this file owns.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } == 0 {
            Ok(file)
        } else {
            Err(io_error(&path)(std::io::Error::last_os_error()))
        }
    })
    .await
    .map_err(|e| Error::Timeout(format!("a bot user's lock: {e}")))?
}

/// How a process bot starts: `just run` from its `Justfile`, or `bash
/// run.sh` (the bots kept from earlier releases, under `bench/checks`).
fn entry_point(bot_dir: &Path) -> Result<Command> {
    if let Some(justfile) = ["Justfile", "justfile"]
        .iter()
        .map(|name| bot_dir.join(name))
        .find(|path| path.is_file())
    {
        // The bot's PATH is the grader's fixed one: find `just` on ours.
        let just = std::env::var_os("PATH")
            .iter()
            .flat_map(std::env::split_paths)
            .map(|dir| dir.join("just"))
            .find(|path| path.is_file())
            .ok_or(Error::NoJust)?;
        let mut command = Command::new(just);
        command.arg("--justfile").arg(justfile).arg("run");
        return Ok(command);
    }
    let run_sh = bot_dir.join("run.sh");
    if !run_sh.is_file() {
        return Err(Error::MissingBot(bot_dir.to_path_buf()));
    }
    let mut command = Command::new("bash");
    command.arg(run_sh);
    Ok(command)
}

/// Start the bot, with `turns` as its file descriptor [`turns::TURN_FD`].
pub(crate) fn spawn_bot(
    bot_dir: &Path,
    env: &BotEnv,
    workdir: &SessionDir,
    log: &Path,
    turns: StdUnixStream,
    regime: Option<&str>,
    reopen_slot: Option<u64>,
) -> Result<Child> {
    // The bot runs from its own directory, so a relative path would resolve twice.
    let bot_dir = bot_dir
        .canonicalize()
        .map_err(|_| Error::MissingBot(bot_dir.to_path_buf()))?;
    let stdout = std::fs::File::create(log).map_err(io_error(log))?;
    let stderr = stdout.try_clone().map_err(io_error(log))?;
    let mut command = entry_point(&bot_dir)?;
    command
        .current_dir(&bot_dir)
        .env_clear()
        .envs(env.vars())
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        )
        .env("HOME", &workdir.path)
        .env(turns::TURN_FD_VAR, turns::TURN_FD.to_string())
        .env(
            "RUST_LOG",
            std::env::var("MTX_BOT_LOG").unwrap_or_else(|_| "info".into()),
        )
        .stdin(Stdio::null())
        .stdout(stdout)
        .stderr(stderr)
        // Its own process group, so stopping the bot stops everything it spawned.
        .process_group(0)
        .kill_on_drop(true);
    if let Some((uid, gid)) = workdir.bot_user {
        command.uid(uid).gid(gid);
    }
    if let Some(regime) = regime {
        command.env("MTX_REGIME", regime);
    }
    if let Some(reopen_slot) = reopen_slot {
        command.env("MTX_REOPEN_SLOT", reopen_slot.to_string());
    }
    let fd = turns.as_raw_fd();
    // SAFETY: between fork and exec, dup2 is async-signal-safe. The copy at
    // TURN_FD has no close-on-exec flag, so it survives into the bot; the
    // original (close-on-exec, as Rust opens it) doesn't.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(fd, turns::TURN_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(io_error(&bot_dir));
    drop(turns);
    child
}

pub(crate) async fn stop_bot(child: &mut Child, bot_user: Option<&str>) {
    if let Some(pid) = child.id() {
        let group = -(pid as i32);
        // SAFETY: signalling a process group we created; a stale group is harmless.
        unsafe { libc::kill(group, libc::SIGTERM) };
        if tokio::time::timeout(STOP_GRACE, child.wait())
            .await
            .is_err()
        {
            warn!("bot ignored SIGTERM; killing it");
        }
        unsafe { libc::kill(group, libc::SIGKILL) };
    }
    let _ = child.wait().await;
    if let Some(user) = bot_user {
        // A bot can leave its process group with setsid; running as its own
        // user, it can't escape this.
        let _ = std::process::Command::new("pkill")
            .args(["-KILL", "-u", user])
            .status();
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[derive(Debug, thiserror::Error)]
    #[error("{0}")]
    struct Failed(String);

    #[tokio::test]
    async fn only_unanswered_reads_are_made_again() {
        let mut calls = 0;
        let timed_out = patiently("x", || {
            calls += 1;
            std::future::ready(Err::<(), _>(Failed(
                "error sending request: operation timed out".into(),
            )))
        })
        .await;
        assert!(timed_out.is_err());
        assert_eq!(calls, READ_ATTEMPTS);

        let mut calls = 0;
        let answered = patiently("x", || {
            calls += 1;
            std::future::ready(Err::<(), _>(Failed("no market SPCX".into())))
        })
        .await;
        assert!(answered.is_err());
        assert_eq!(calls, 1);

        let mut calls = 0;
        let second = patiently("x", || {
            calls += 1;
            std::future::ready(if calls == 1 {
                Err(Failed("Connection refused (os error 111)".into()))
            } else {
                Ok(7)
            })
        })
        .await;
        assert_eq!(second.unwrap(), 7);
    }

    /// Two grades on one machine never hold a bot user at once: the second
    /// waits until the first lets go.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_bot_user_is_held_by_one_session_at_a_time() {
        let path = std::env::temp_dir().join(format!("mtx-user-{}.lock", uuid::Uuid::new_v4()));
        let first = lock(&path).await.unwrap();
        let second = tokio::spawn({
            let path = path.clone();
            async move { lock(&path).await.unwrap() }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(!second.is_finished(), "both held the user");
        drop(first);
        let second = tokio::time::timeout(Duration::from_secs(10), second)
            .await
            .expect("the user was let go")
            .unwrap();
        drop(second);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn session_dir_and_its_files_are_private() {
        let dir = SessionDir::create(&Keypair::new(), Some("mtx-bot-none")).unwrap();
        assert_eq!(mode(&dir.path), 0o700);
        assert_eq!(mode(&dir.keypair_path()), 0o600);
        let mandate = dir
            .write_mandate(&Mandate::perp("phoenix", "SOL-PERP", 1.0, 1.0))
            .unwrap();
        assert_eq!(mode(mandate.parent().unwrap()), 0o700);
    }
}
