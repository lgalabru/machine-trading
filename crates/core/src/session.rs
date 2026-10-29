//! One trading session: a bot against one scenario on a fresh offline world.
//!
//! 1. Restore the snapshot offline and put the RPC guard in front of it.
//! 2. Onboard a fresh trader with [`DEPOSIT_USDC`] of collateral.
//! 3. Start the bot (`run.sh` in its directory) with the [`BotEnv`] contract,
//!    then register the scenario, so its slots count from the bot's start.
//! 4. At the scenario's last slot, stop the bot and mark the account to market.
//!
//! Each session gets its own surfnet, so nothing a bot does carries over.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use mth_botkit::{
    BotEnv, QUOTE_LOTS_PER_USD,
    mandate::{InstrumentRef, Mandate, Observed, Term},
};
use serde::Serialize;
use serde_json::json;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use surfpool_sdk::CheatcodeFilter;
use tokio::process::{Child, Command};
use tracing::{info, warn};

use crate::{
    Error, Result, chain,
    error::io_error,
    flow::{FlowReport, Takers},
    guard::Guard,
    making::level_at,
    phoenix::{self, Phoenix, USDC_MINT},
    scenario::Scenario,
    snapshot,
};

/// Collateral each session's trader starts with.
pub const DEPOSIT_USDC: f64 = 10_000.0;
/// The unprivileged account bots run as when the grader runs as root.
/// Unix users bots run as when the grader runs as root, one per session that
/// can run at once (see [`SessionOptions::slot`]). Each session kills every
/// process its user owns when it ends, so two sessions never share one.
pub const BOT_USERS: [&str; 4] = ["mth-bot", "mth-bot-2", "mth-bot-3", "mth-bot-4"];
/// How long a bot gets to exit after SIGTERM before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(2);

pub struct SessionOptions<'a> {
    pub snapshot_dir: &'a Path,
    pub scenario: &'a Scenario,
    pub bot_dir: &'a Path,
    /// Where the bot's stdout and stderr go.
    pub bot_log: &'a Path,
    /// Which of [`BOT_USERS`] the bot runs as. Sessions that run at the same
    /// time must use different slots.
    pub slot: usize,
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
    let scenario = opts.scenario;
    let world = snapshot::start_offline(opts.snapshot_dir).await?;
    let surfnet = &world.surfnet;
    let rpc = chain::rpc_client(surfnet);
    let phoenix = Phoenix::new(world.exchange.clone(), surfnet.rpc_url());
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
    let takers = match &scenario.making {
        Some(making) => {
            Some(Takers::prepare(&world, &phoenix, scenario, making, start_mark_ticks).await?)
        }
        None => None,
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
    let start_slot = rpc.get_slot()?;
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
    // The scenario is the grader's last cheatcode. Lock them all before the
    // bot starts: a bot that finds the surfnet's own port, past the guard,
    // gets a plain validator RPC, with no cheatcodes, airdrops, or way to
    // stop the surfnet.
    let perp_asset_map = phoenix.perp_asset_map()?;
    chain::register_scenario(
        &rpc,
        &scenario.name,
        scenario.overrides(perp_asset_map, start_mark_ticks),
    )?;
    // Overrides count from the slot they were registered at; so does the
    // takers' flow.
    let base_slot = rpc.get_slot()?;
    surfnet.cheatcodes().lock(CheatcodeFilter::all())?;

    let late_before = surfnet.coalesced_slots();
    let mut child = spawn_bot(opts.bot_dir, &env, &workdir, opts.bot_log)?;
    info!(
        scenario = scenario.name,
        start_slot,
        end_slot = env.end_slot,
        "bot started"
    );

    let mut bot_exit = None;
    // Sampled twice a second, the loop's cadence: a fill persists until the
    // bot trades out of it, so a position over the cap is caught.
    let mut peak_position: f64 = 0.0;
    // The account's value, sampled at the same cadence: its running peak, and
    // the deepest it has fallen below it.
    let mut peak_value_usd = start_value_usd;
    let mut max_drawdown_usd: f64 = 0.0;
    // Phoenix positions (or orders) in markets other than the scenario's.
    let mut positions_outside_universe: u32 = 0;
    let sampling = async {
        loop {
            let slot = rpc.get_slot()?;
            if slot >= env.end_slot {
                break;
            }
            let asset = hawkeye
                .view_margin_for_asset(bot.pubkey(), 0, 0, market.asset_id)
                .await?
                .value;
            peak_position =
                peak_position.max(phoenix.lots_to_base_units(market, asset.base_lots).abs());
            let now = account(&rpc, &phoenix, &bot.pubkey(), trader).await?;
            positions_outside_universe =
                positions_outside_universe.max(now.positions_outside(asset.has_position_or_orders));
            let value_usd = value(now.value_usd, asset.base_lots, slot, base_slot).await?;
            peak_value_usd = peak_value_usd.max(value_usd);
            max_drawdown_usd = max_drawdown_usd.max(peak_value_usd - value_usd);
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
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Ok::<(), Error>(())
    };
    let flow = async {
        match (&takers, &scenario.making) {
            (Some(takers), Some(making)) => Some(
                takers
                    .run(&rpc, scenario, making, start_mark_usd, base_slot, end_slot)
                    .await,
            ),
            _ => None,
        }
    };
    let (sampled, flow) = futures::join!(sampling, flow);
    sampled?;
    stop_bot(&mut child, workdir.bot_user.map(|_| BOT_USERS[opts.slot])).await;
    let late_slots = surfnet.coalesced_slots() - late_before;
    if late_slots > 0 {
        warn!(
            scenario = scenario.name,
            late_slots, "the clock fell behind during the session"
        );
    }

    let end = account(&rpc, &phoenix, &bot.pubkey(), trader).await?;
    let asset = hawkeye
        .view_margin_for_asset(bot.pubkey(), 0, 0, market.asset_id)
        .await?
        .value;
    let end_value_usd = value(end.value_usd, asset.base_lots, end_slot, base_slot).await?;
    let positions_outside_universe =
        positions_outside_universe.max(end.positions_outside(asset.has_position_or_orders));
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
    };
    info!(
        scenario = report.scenario,
        pnl_usd = report.pnl_usd,
        refused = report.refused_calls.len(),
        "session finished"
    );
    Ok(report)
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

fn spawn_bot(bot_dir: &Path, env: &BotEnv, workdir: &SessionDir, log: &Path) -> Result<Child> {
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
    command.spawn().map_err(io_error(&run_sh))
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
