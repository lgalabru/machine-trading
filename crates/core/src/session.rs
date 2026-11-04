//! One trading session: a bot against one scenario on a fresh offline world.
//!
//! 1. Restore the snapshot offline and put the RPC guard in front of it.
//! 2. Onboard a fresh trader with [`DEPOSIT_USDC`] of collateral.
//! 3. Stop the clock, register the scenario and lock the cheatcodes, then
//!    start the bot (`run.sh` in its directory) with the [`BotEnv`] contract.
//! 4. Move the world in steps of [`SLOTS_PER_STEP`] slots, and between them
//!    give the bot its turn (see [`mth_botkit::turns`]): each step produces
//!    its blocks, which land the scenario's move; then the grader's own
//!    traders act; then the bot gets the step and the world waits for it.
//! 5. After the last step, stop the bot and mark the account to market.
//!
//! Each session gets its own surfnet, so nothing a bot does carries over, and
//! nothing moves while the bot thinks, so the same bot on the same scenario
//! trades the same session every time.
//!
//! A bot that never answers the hello runs the old way: the world moves at
//! the slot time, whatever the bot does.

use std::{
    os::{fd::AsRawFd, unix::net::UnixStream as StdUnixStream},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use mth_botkit::{
    BotEnv, QUOTE_LOTS_PER_USD,
    mandate::{InstrumentRef, Mandate, Observed, Term},
    turns::{self, Hello, Message, Step},
};
use serde::Serialize;
use serde_json::json;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use surfpool_sdk::{BlockProductionMode, CheatcodeFilter, SimnetCommand};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::{Child, Command},
};
use tracing::{info, warn};

use crate::{
    Error, Result, chain,
    compete::{CompeteReport, Competitor},
    error::io_error,
    flow::{FlowReport, TakerRecord, Takers},
    guard::{CallLog, Guard},
    making::level_at,
    phoenix::{self, Phoenix, SLOTS_BETWEEN_STEPS, USDC_MINT},
    scenario::Scenario,
    snapshot,
};

/// Slots each step moves the world: the scenario's own step, so every move
/// lands on a step.
pub const SLOTS_PER_STEP: u64 = SLOTS_BETWEEN_STEPS;
/// How long the world waits for the bot's `ready`, and for each `done`.
const HELLO_WALL: Duration = Duration::from_secs(10);
const TURN_WALL: Duration = Duration::from_secs(5);
/// What all of a bot's turns in a session may take together, per step: the
/// slot time, so a slow bot is no slower than the world used to be. Past it,
/// the world stops waiting.
const TURN_BUDGET_PER_STEP: Duration = Duration::from_millis(500);

/// Collateral each session's trader starts with.
pub const DEPOSIT_USDC: f64 = 10_000.0;
/// The unprivileged account bots run as when the grader runs as root.
/// Unix users bots run as when the grader runs as root, one per session that
/// can run at once (see [`SessionOptions::slot`]). Each session kills every
/// process its user owns when it ends, so two sessions never share one.
pub const BOT_USERS: [&str; 4] = ["mth-bot", "mth-bot-2", "mth-bot-3", "mth-bot-4"];
/// How long a bot gets to exit after SIGTERM before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(2);
/// Slots from the surfnet's start to the session's: setup runs on the wall
/// clock and takes 10 to 15 of them, more on a loaded machine. The world
/// then steps to this one, so every session starts at the same slot however
/// long its setup took: state that depends on the slot (Phoenix's funding,
/// the oracle's age) replays the same.
const SETUP_SLOTS: u64 = 60;

pub struct SessionOptions<'a> {
    pub snapshot_dir: &'a Path,
    pub scenario: &'a Scenario,
    pub bot_dir: &'a Path,
    /// Where the bot's stdout and stderr go.
    pub bot_log: &'a Path,
    /// Which of [`BOT_USERS`] the bot runs as. Sessions that run at the same
    /// time must use different slots.
    pub slot: usize,
    /// The session's regime, handed to the bot as `MTH_REGIME` (JSON): only
    /// for the ladder's regime oracle, never for a graded bot.
    pub regime: Option<&'a str>,
}

#[derive(Debug, Clone, Serialize)]
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
    /// Slots that arrived late while the bot ran: the surfnet's clock fell
    /// behind (a starved machine) and stretched those slots. A bot polls on
    /// the wall clock, so a session with many is not comparable to one
    /// without; [`crate::grade`] refuses to score it.
    pub late_slots: u64,
    /// The mandate the session ran under, with its term: what the bot was
    /// handed, and what [`SessionReport::observed`] is audited against.
    pub mandate: Mandate,
    /// The scenario's market, and the largest position, long or short, in
    /// base units, the bot held in it while it ran.
    pub instrument: InstrumentRef,
    pub peak_position: f64,
    /// The most positions or orders the bot held at once in other markets.
    pub positions_outside_universe: u32,
    /// The deepest the account's value fell below its running peak while the
    /// bot ran, in USD.
    pub max_drawdown_usd: f64,
    /// The price the account's position was valued at: the `mark`, or for
    /// market making the `oracle` the scenario sets, which a bot's own
    /// quotes can't move.
    pub valuation: &'static str,
    /// Market making: what the grader's takers sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flow: Option<FlowReport>,
    /// Market making: what the grader's own maker did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub competitor: Option<CompeteReport>,
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
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct StepRecord {
    pub k: u64,
    pub at: u64,
    pub mark_usd: f64,
    /// Phoenix's index (oracle) price.
    pub index_usd: f64,
    /// The scenario's planned level.
    pub level_usd: f64,
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
#[derive(Debug, Clone, Default, Serialize, PartialEq)]
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
}

impl SessionReport {
    /// What the session measured, in the terms [`Mandate::audit`] checks.
    pub fn observed(&self) -> Observed {
        Observed {
            peak_positions: vec![(self.instrument.clone(), self.peak_position)],
            positions_outside_universe: self.positions_outside_universe,
            max_drawdown_usd: self.max_drawdown_usd,
        }
    }
}

pub async fn run(opts: SessionOptions<'_>) -> Result<SessionReport> {
    let began = Instant::now();
    let scenario = opts.scenario;
    let world = snapshot::start_offline(opts.snapshot_dir).await?;
    let surfnet = &world.surfnet;
    // The grader's own clients, served the way a stepped world needs: a
    // transaction is final once it executed (see [`Guard::internal`]).
    let internal = Guard::internal(surfnet.rpc_url()).await?;
    let rpc = mth_botkit::rpc_client(internal.url());
    let setup_from = rpc.get_slot()?;
    let phoenix = Phoenix::new(world.exchange.clone(), internal.url());
    let market = phoenix.market(&scenario.symbol)?;

    let bot = Keypair::new();
    let trader = phoenix::onboard_trader(surfnet, &phoenix, &bot, DEPOSIT_USDC).await?;
    let hawkeye = phoenix.hawkeye(bot.pubkey());
    let start_mark_ticks = hawkeye
        .view_bbo(&scenario.symbol)
        .await?
        .value
        .mark_price_ticks;
    let start_mark_usd = phoenix.ticks_to_usd(market, start_mark_ticks);
    let start_value_usd = account(&rpc, &phoenix, &bot.pubkey(), trader)
        .await?
        .value_usd;
    // Market making: the takers and the widened makers, set up while
    // cheatcodes still work.
    let (takers, competitor) = match &scenario.making {
        Some(making) => (
            Some(
                Takers::prepare(
                    &world,
                    &phoenix,
                    scenario,
                    making,
                    start_mark_ticks,
                    internal.url(),
                )
                .await?,
            ),
            Some(Competitor::prepare(&world, &phoenix, scenario, internal.url()).await?),
        ),
        None => (None, None),
    };
    // What a market-making account's position is worth: the scenario's own
    // oracle level at `slot`, which quoting can't push the way it can push
    // Phoenix's mark. Otherwise the mark, as Phoenix reports it.
    let value = async |value_usd: f64, base_lots: i64, slot: u64, base_slot: u64| -> Result<f64> {
        if scenario.making.is_none() {
            return Ok(value_usd);
        }
        let mark = hawkeye
            .view_bbo(&scenario.symbol)
            .await?
            .value
            .mark_price_ticks;
        let oracle = start_mark_usd
            * (1.0 + level_at(&scenario.path, slot.saturating_sub(base_slot)) / 100.0);
        let units = phoenix.lots_to_base_units(market, base_lots);
        Ok(value_usd + units * (oracle - phoenix.ticks_to_usd(market, mark)))
    };

    let guard = Guard::start(surfnet.rpc_url()).await?;
    let workdir = SessionDir::create(&bot, BOT_USERS[opts.slot])?;
    // From here on, a slot passes only when the grader produces one.
    surfnet.send_command(SimnetCommand::UpdateBlockProductionMode(
        BlockProductionMode::Manual,
    ))?;
    let mut slot = rpc.get_slot()?;
    let first_slot = setup_from + SETUP_SLOTS;
    if slot > first_slot {
        warn!(
            slot,
            first_slot, "setup ran past the session's first slot: this session won't replay"
        );
    }
    while slot < first_slot {
        surfnet.send_command(SimnetCommand::SlotForward(None))?;
        slot = wait_for_slot(&rpc, slot + 1).await?;
    }
    // The scenario is the grader's last cheatcode. Lock them all before the
    // bot starts: the bot's only way to the surfnet is the guard, and even
    // the guard's upstream has no cheatcodes, airdrops, or way to stop it.
    // Overrides count from the slot they were registered at; so does the
    // takers' flow.
    let perp_asset_map = phoenix.perp_asset_map()?;
    chain::register_scenario(
        &rpc,
        &scenario.name,
        scenario.overrides(perp_asset_map, start_mark_ticks),
    )?;
    let base_slot = rpc.get_slot()?;
    surfnet.cheatcodes().lock(CheatcodeFilter::all())?;
    let start_slot = base_slot;
    let end_slot = start_slot + scenario.duration_slots;
    // The bot gets a copy to plan by; the audit uses this one, which the bot
    // can't reach.
    let mandate = Mandate {
        term: Some(Term {
            until_slot: end_slot,
        }),
        ..scenario.mandate.clone()
    };
    let instrument = scenario.instrument();
    let env = BotEnv {
        rpc_url: guard.url().to_string(),
        keypair_path: workdir.keypair_path(),
        exchange_path: world.exchange_path.clone(),
        symbol: scenario.symbol.clone(),
        end_slot,
        max_position: scenario.max_position(),
        max_drawdown_usd: mandate.limits.max_drawdown_usd,
        mandate_path: Some(workdir.write_mandate(&mandate)?),
    };

    let late_before = surfnet.coalesced_slots();
    let (ours, theirs) = StdUnixStream::pair().map_err(io_error("turn socket"))?;
    let mut child = spawn_bot(
        opts.bot_dir,
        &env,
        &workdir,
        opts.bot_log,
        theirs,
        opts.regime,
    )?;
    ours.set_nonblocking(true)
        .map_err(io_error("turn socket"))?;
    let socket = UnixStream::from_std(ours).map_err(io_error("turn socket"))?;
    let (reader, mut writer) = socket.into_split();
    let mut reader = BufReader::new(reader).lines();
    let steps = scenario.duration_slots.div_ceil(SLOTS_PER_STEP);
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
        turn_wall_ms: TURN_WALL.as_millis() as u64,
    };
    let mut turns = TurnReport {
        steps,
        ..TurnReport::default()
    };
    let legacy = !(send(&mut writer, &Message::Hello(hello)).await
        && matches!(
            tokio::time::timeout(HELLO_WALL, next_message(&mut reader)).await,
            Ok(Some(Message::Ready))
        ));
    if legacy {
        info!("the bot didn't answer the hello: the world moves at the slot time");
    }
    turns.legacy = legacy;
    let budget = TURN_BUDGET_PER_STEP * steps as u32;
    let mut spent = Duration::ZERO;

    let mut flow = takers
        .as_ref()
        .zip(scenario.making.as_ref())
        .map(|(takers, making)| takers.flow(making));
    let mut quoting = competitor
        .as_ref()
        .zip(scenario.making.as_ref())
        .map(|(competitor, making)| competitor.quoting(making));
    let mut bot_exit = None;
    // Sampled every step: a fill persists until the bot trades out of it, so
    // a position over the cap is caught.
    let mut peak_position: f64 = 0.0;
    // The account's value, sampled at the same cadence: its running peak, and
    // the deepest it has fallen below it.
    let mut peak_value_usd = start_value_usd;
    let mut max_drawdown_usd: f64 = 0.0;
    // Phoenix positions (or orders) in markets other than the scenario's.
    let mut positions_outside_universe: u32 = 0;
    // The takers' orders due by this many slots into the session: sent after
    // the bot's turn, against the quotes it left (see the step's order
    // below).
    let mut flow_due: Option<u64> = None;
    let send_flow = async |flow: &mut Option<crate::flow::Flow>, at: Option<u64>| {
        if let (Some(flow), Some(takers), Some(making), Some(at)) =
            (flow.as_mut(), takers.as_ref(), scenario.making.as_ref(), at)
        {
            flow.step(takers, scenario, making, start_mark_usd, at)
                .await;
        }
    };
    let mut steps_seen: Vec<StepRecord> = Vec::with_capacity(steps as usize);
    let mut slot = start_slot;
    for k in 0..steps {
        // A step: the last step's takers trade against the quotes the bot
        // left; then the world's blocks land (and with them the scenario's
        // move); the grader's maker looks and requotes; then the bot's turn.
        // Takers trading right after a move, before any maker could requote,
        // would pick off every quote: a sniper the task doesn't describe.
        // Here the toxic flow is the informed flow, ahead of the jumps.
        guard.set_turn(legacy);
        send_flow(&mut flow, flow_due.take()).await;
        let target = (slot + SLOTS_PER_STEP).min(end_slot);
        while slot < target {
            surfnet.send_command(SimnetCommand::SlotForward(None))?;
            slot = wait_for_slot(&rpc, slot + 1).await?;
        }
        let at = slot - base_slot;
        flow_due = Some(at);
        if let (Some(quoting), Some(competitor), Some(making)) = (
            quoting.as_mut(),
            competitor.as_ref(),
            scenario.making.as_ref(),
        ) {
            quoting
                .step(competitor, scenario, making, start_mark_usd, at)
                .await;
        }

        let asset = hawkeye
            .view_margin_for_asset(bot.pubkey(), 0, 0, market.asset_id)
            .await?
            .value;
        peak_position =
            peak_position.max(phoenix.lots_to_base_units(market, asset.base_lots).abs());
        let now = account(&rpc, &phoenix, &bot.pubkey(), trader).await?;
        let outside = match now.positions_outside(asset.has_position_or_orders) {
            0 => 0,
            seen => seen.min(outside_again(&rpc, &phoenix, &hawkeye, &bot, trader, market).await?),
        };
        positions_outside_universe = positions_outside_universe.max(outside);
        let value_usd = value(now.value_usd, asset.base_lots, slot, base_slot).await?;
        peak_value_usd = peak_value_usd.max(value_usd);
        max_drawdown_usd = max_drawdown_usd.max(peak_value_usd - value_usd);
        let bbo = hawkeye.view_bbo(&scenario.symbol).await?.value;
        let usd = |ticks: u64| phoenix.ticks_to_usd(market, ticks);
        steps_seen.push(StepRecord {
            k,
            at,
            mark_usd: usd(bbo.mark_price_ticks),
            index_usd: usd(bbo.index_price_ticks),
            level_usd: start_mark_usd * (1.0 + level_at(&scenario.path, at) / 100.0),
            best_bid_usd: bbo.best_bid_ticks().map(usd),
            best_ask_usd: bbo.best_ask_ticks().map(usd),
            position: phoenix.lots_to_base_units(market, asset.base_lots),
            value_usd,
            pnl_usd: value_usd - start_value_usd,
            turn_ms: None,
            answered: None,
        });
        if bot_exit.is_none()
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
        if legacy {
            tokio::time::sleep(TURN_BUDGET_PER_STEP).await;
            continue;
        }
        let started = Instant::now();
        let step = Step {
            k,
            slot,
            last: k + 1 == steps,
        };
        let wall = TURN_WALL.min(budget.saturating_sub(spent));
        let answered = send(&mut writer, &Message::Step(step)).await
            && tokio::time::timeout(wall, async {
                loop {
                    match next_message(&mut reader).await {
                        Some(Message::Done { k: done }) if done == k => return true,
                        Some(_) => continue,
                        None => return false,
                    }
                }
            })
            .await
            .unwrap_or(false);
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
    send_flow(&mut flow, flow_due.take()).await;
    let _ = send(&mut writer, &Message::End).await;
    stop_bot(&mut child, workdir.bot_user.map(|_| BOT_USERS[opts.slot])).await;
    turns.late_sends = guard.late_sends();
    let late_slots = surfnet.coalesced_slots() - late_before;
    let (flow, taker_orders) = match flow {
        Some(flow) => {
            let (report, orders) = flow.finish();
            (Some(report), orders)
        }
        None => (None, Vec::new()),
    };
    let calls = guard.call_log();
    let transactions = transactions(&rpc, &calls, base_slot, &phoenix, market);
    let competitor = match (quoting, competitor.as_ref()) {
        (Some(quoting), Some(competitor)) => Some(quoting.report(competitor).await),
        _ => None,
    };

    let end = account(&rpc, &phoenix, &bot.pubkey(), trader).await?;
    let asset = hawkeye
        .view_margin_for_asset(bot.pubkey(), 0, 0, market.asset_id)
        .await?
        .value;
    let end_value_usd = value(end.value_usd, asset.base_lots, end_slot, base_slot).await?;
    let outside = match end.positions_outside(asset.has_position_or_orders) {
        0 => 0,
        seen => seen.min(outside_again(&rpc, &phoenix, &hawkeye, &bot, trader, market).await?),
    };
    let positions_outside_universe = positions_outside_universe.max(outside);
    let margin = hawkeye.view_margin(bot.pubkey(), 0, 0).await?.value;
    let report = SessionReport {
        scenario: scenario.name.clone(),
        start_mark_usd,
        end_mark_usd: phoenix.ticks_to_usd(
            market,
            hawkeye
                .view_bbo(&scenario.symbol)
                .await?
                .value
                .mark_price_ticks,
        ),
        start_value_usd,
        end_value_usd,
        pnl_usd: end_value_usd - start_value_usd,
        end_position: phoenix.lots_to_base_units(market, asset.base_lots),
        liquidatable: margin.is_liquidatable != 0,
        refused_calls: guard.refused(),
        bot_exit,
        late_slots,
        mandate,
        instrument,
        peak_position: peak_position.max(phoenix.lots_to_base_units(market, asset.base_lots).abs()),
        positions_outside_universe,
        max_drawdown_usd: max_drawdown_usd.max(peak_value_usd.max(end_value_usd) - end_value_usd),
        valuation: if scenario.making.is_some() {
            "oracle"
        } else {
            "mark"
        },
        flow,
        competitor,
        turns,
        wall_ms: began.elapsed().as_millis() as u64,
        timeline: Timeline {
            start_slot,
            end_slot,
            slots_per_step: SLOTS_PER_STEP,
            start_mark_usd,
            steps: steps_seen,
            transactions,
            calls,
            taker_orders,
        },
    };
    info!(
        scenario = report.scenario,
        pnl_usd = report.pnl_usd,
        refused = report.refused_calls.len(),
        overran = report.turns.overran,
        "session finished"
    );
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

const COMPUTE_BUDGET: &str = "ComputeBudget111111111111111111111111111111";

/// Decode one instruction of a `getTransaction` (`json` encoding).
fn decode_instruction(
    ix: &serde_json::Value,
    keys: &[serde_json::Value],
    phoenix: &Phoenix,
    market: &phoenix_rise::types::exchange::ExchangeMarketConfig,
) -> IxRecord {
    use phoenix_rise::ix::{PhoenixInstruction, order_packet::OrderPacket};
    let program = ix["programIdIndex"]
        .as_u64()
        .and_then(|i| keys.get(i as usize))
        .and_then(|k| k.as_str())
        .unwrap_or_default()
        .to_string();
    let data = ix["data"]
        .as_str()
        .and_then(|d| bs58::decode(d).into_vec().ok())
        .unwrap_or_default();
    if program == COMPUTE_BUDGET {
        return IxRecord {
            program: "compute_budget".into(),
            name: None,
            order: None,
        };
    }
    let found = data
        .get(..8)
        .and_then(|tag| PhoenixInstruction::from_discriminant(tag.try_into().ok()?));
    let Some(instruction) = found else {
        return IxRecord {
            program,
            name: None,
            order: None,
        };
    };
    let order = matches!(
        instruction,
        PhoenixInstruction::PlaceLimitOrder | PhoenixInstruction::PlaceMarketOrder
    )
    .then(|| {
        let packet = <OrderPacket as borsh::BorshDeserialize>::deserialize(&mut &data[8..]).ok()?;
        // The packet's kind is private to phoenix-rise; its serde form isn't.
        let value = serde_json::to_value(&packet).ok()?;
        let (kind, fields) = value["kind"].as_object()?.iter().next()?;
        let side = fields["side"].as_str().unwrap_or_default().to_lowercase();
        Some(OrderRecord {
            kind: kind.clone(),
            side,
            price_usd: fields["price_in_ticks"]
                .as_u64()
                .map(|ticks| phoenix.ticks_to_usd(market, ticks)),
            size: phoenix.lots_to_base_units(market, fields["num_base_lots"].as_i64()?),
        })
    })
    .flatten();
    IxRecord {
        program: "phoenix".into(),
        name: Some(format!("{instruction:?}")),
        order,
    }
}

/// Look up each of the bot's sends: where it executed and how it went.
fn transactions(
    rpc: &RpcClient,
    calls: &CallLog,
    base_slot: u64,
    phoenix: &Phoenix,
    market: &phoenix_rise::types::exchange::ExchangeMarketConfig,
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
            let keys = message["accountKeys"].as_array().cloned().unwrap_or_default();
            record.instructions = message["instructions"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|ix| decode_instruction(ix, &keys, phoenix, market))
                .collect();
            if !record.ok {
                record.logs = logs;
            }
            seen.insert(signature.clone(), record.instructions.clone());
            record
        })
        .collect()
}

/// Wait until the surfnet has produced `slot`.
async fn wait_for_slot(rpc: &RpcClient, slot: u64) -> Result<u64> {
    let deadline = Instant::now() + Duration::from_secs(10);
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

/// Send one turn message; false if the bot's end is gone.
async fn send(writer: &mut tokio::net::unix::OwnedWriteHalf, message: &Message) -> bool {
    let mut line = serde_json::to_string(message).expect("turn messages serialize");
    line.push('\n');
    writer.write_all(line.as_bytes()).await.is_ok()
}

/// The bot's next turn message; `None` once its end is gone. Lines that
/// aren't messages are skipped.
async fn next_message(
    reader: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
) -> Option<Message> {
    loop {
        let line = reader.next_line().await.ok()??;
        if let Ok(message) = serde_json::from_str(&line) {
            return Some(message);
        }
    }
}

/// Markets held other than the scenario's, read again. The market's view and
/// the account's are two reads: an order the bot places or cancels between
/// them shows up in one and not the other, which looks like a position
/// elsewhere. A position that really is elsewhere shows up on every read, so
/// a count is only kept if a second pair of reads agrees.
async fn outside_again(
    rpc: &RpcClient,
    phoenix: &Phoenix,
    hawkeye: &phoenix_rise::core::PhoenixHawkeyeClient<'_>,
    bot: &Keypair,
    trader: Pubkey,
    market: &phoenix_rise::types::exchange::ExchangeMarketConfig,
) -> Result<u32> {
    let asset = hawkeye
        .view_margin_for_asset(bot.pubkey(), 0, 0, market.asset_id)
        .await?
        .value;
    let now = account(rpc, phoenix, &bot.pubkey(), trader).await?;
    Ok(now.positions_outside(asset.has_position_or_orders))
}

/// The trader account, from one margin view.
struct Account {
    /// What the account is worth at mark: Phoenix collateral plus unrealized
    /// PnL, plus any USDC withdrawn back to the wallet.
    value_usd: f64,
    /// Markets the trader holds a position or orders in.
    position_count: u16,
}

impl Account {
    /// Markets held other than the scenario's, given whether the scenario's
    /// market is one of them.
    fn positions_outside(&self, held_in_market: u8) -> u32 {
        u32::from(
            self.position_count
                .saturating_sub(u16::from(held_in_market != 0)),
        )
    }
}

async fn account(
    rpc: &RpcClient,
    phoenix: &Phoenix,
    owner: &Pubkey,
    trader: Pubkey,
) -> Result<Account> {
    let margin = phoenix
        .hawkeye(*owner)
        .view_margin_for_trader(trader)
        .await?
        .value;
    let phoenix_quote_lots = margin.collateral_quote_lots + margin.unrealized_pnl_quote_lots;
    Ok(Account {
        value_usd: phoenix_quote_lots as f64 / QUOTE_LOTS_PER_USD + wallet_usdc(rpc, owner)?,
        position_count: margin.position_count,
    })
}

fn wallet_usdc(rpc: &RpcClient, owner: &Pubkey) -> Result<f64> {
    let accounts = chain::call(
        rpc,
        "getTokenAccountsByOwner",
        json!([owner.to_string(), { "mint": USDC_MINT.to_string() }, { "encoding": "jsonParsed" }]),
    )?;
    Ok(accounts["value"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|account| {
            account["account"]["data"]["parsed"]["info"]["tokenAmount"]["uiAmountString"]
                .as_str()?
                .parse::<f64>()
                .ok()
        })
        .sum())
}

/// A private directory for the bot: its keypair and its `HOME`.
struct SessionDir {
    path: PathBuf,
    bot_user: Option<(u32, u32)>,
}

impl SessionDir {
    fn create(bot: &Keypair, user: &str) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("mth-session-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).map_err(io_error(&path))?;
        let dir = Self {
            path,
            bot_user: bot_user(user),
        };
        let keypair = serde_json::to_string(&bot.to_bytes().to_vec())?;
        std::fs::write(dir.keypair_path(), keypair).map_err(io_error(dir.keypair_path()))?;
        if let Some((uid, gid)) = dir.bot_user {
            for path in [dir.path.clone(), dir.keypair_path()] {
                std::os::unix::fs::chown(&path, Some(uid), Some(gid)).map_err(io_error(&path))?;
            }
        }
        Ok(dir)
    }

    fn keypair_path(&self) -> PathBuf {
        self.path.join("keypair.json")
    }

    fn write_mandate(&self, mandate: &Mandate) -> Result<PathBuf> {
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

/// Start the bot, with `turns` as its file descriptor [`turns::TURN_FD`].
fn spawn_bot(
    bot_dir: &Path,
    env: &BotEnv,
    workdir: &SessionDir,
    log: &Path,
    turns: StdUnixStream,
    regime: Option<&str>,
) -> Result<Child> {
    let run_sh = bot_dir.join("run.sh");
    if !run_sh.is_file() {
        return Err(Error::MissingBot(run_sh));
    }
    // The bot runs from its own directory, so a relative path would resolve twice.
    let bot_dir = bot_dir.canonicalize().map_err(io_error(bot_dir))?;
    let run_sh = bot_dir.join("run.sh");
    let stdout = std::fs::File::create(log).map_err(io_error(log))?;
    let stderr = stdout.try_clone().map_err(io_error(log))?;
    let mut command = Command::new("bash");
    command
        .arg(&run_sh)
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
            std::env::var("MTH_BOT_LOG").unwrap_or_else(|_| "info".into()),
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
        command.env("MTH_REGIME", regime);
    }
    let fd = turns.as_raw_fd();
    // SAFETY: between fork and exec, dup2 is async-signal-safe. The copy at
    // TURN_FD has no close-on-exec flag, so it survives into run.sh; the
    // original (close-on-exec, as Rust opens it) doesn't.
    unsafe {
        command.pre_exec(move || {
            if libc::dup2(fd, turns::TURN_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn().map_err(io_error(&run_sh));
    drop(turns);
    child
}

async fn stop_bot(child: &mut Child, bot_user: Option<&str>) {
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
