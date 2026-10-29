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

use mt_botkit::{BotEnv, QUOTE_LOTS_PER_USD};
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
    guard::Guard,
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
pub const BOT_USERS: [&str; 4] = ["mt-bot", "mt-bot-2", "mt-bot-3", "mt-bot-4"];
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
    /// The largest position, long or short, in base units, the bot held while
    /// it ran, and the scenario's cap on it.
    pub peak_position: f64,
    pub max_position: f64,
    /// The deepest the account's value fell below its running peak while the
    /// bot ran, in USD, and the scenario's limit on it.
    pub max_drawdown_usd: f64,
    pub drawdown_limit_usd: f64,
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
    let start_value_usd = account_value(&rpc, &phoenix, &bot.pubkey(), trader).await?;

    let guard = Guard::start(surfnet.rpc_url()).await?;
    let workdir = SessionDir::create(&bot, BOT_USERS[opts.slot])?;
    let start_slot = rpc.get_slot()?;
    let env = BotEnv {
        rpc_url: guard.url().to_string(),
        keypair_path: workdir.keypair_path(),
        exchange_path: world.exchange_path.clone(),
        symbol: scenario.symbol.clone(),
        end_slot: start_slot + scenario.duration_slots,
        max_position: scenario.max_position,
        max_drawdown_usd: scenario.max_drawdown_usd,
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
    loop {
        if rpc.get_slot()? >= env.end_slot {
            break;
        }
        let held = hawkeye
            .view_margin_for_asset(bot.pubkey(), 0, 0, market.asset_id)
            .await?
            .value
            .base_lots;
        peak_position = peak_position.max(phoenix.lots_to_base_units(market, held).abs());
        let value_usd = account_value(&rpc, &phoenix, &bot.pubkey(), trader).await?;
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
    stop_bot(&mut child, workdir.bot_user.map(|_| BOT_USERS[opts.slot])).await;
    let late_slots = surfnet.coalesced_slots() - late_before;
    if late_slots > 0 {
        warn!(
            scenario = scenario.name,
            late_slots, "the clock fell behind during the session"
        );
    }

    let end_value_usd = account_value(&rpc, &phoenix, &bot.pubkey(), trader).await?;
    let asset = hawkeye
        .view_margin_for_asset(bot.pubkey(), 0, 0, market.asset_id)
        .await?
        .value;
    let margin = hawkeye.view_margin(bot.pubkey(), 0, 0).await?.value;
    let report = SessionReport {
        scenario: scenario.name.clone(),
        start_mark_usd: phoenix.ticks_to_usd(market, start_mark_ticks),
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
        peak_position: peak_position.max(phoenix.lots_to_base_units(market, asset.base_lots).abs()),
        max_position: scenario.max_position,
        max_drawdown_usd: max_drawdown_usd.max(peak_value_usd.max(end_value_usd) - end_value_usd),
        drawdown_limit_usd: scenario.max_drawdown_usd,
    };
    info!(
        scenario = report.scenario,
        pnl_usd = report.pnl_usd,
        refused = report.refused_calls.len(),
        "session finished"
    );
    Ok(report)
}

/// What the account is worth at mark: Phoenix collateral plus unrealized
/// PnL, plus any USDC withdrawn back to the wallet.
async fn account_value(
    rpc: &RpcClient,
    phoenix: &Phoenix,
    owner: &Pubkey,
    trader: Pubkey,
) -> Result<f64> {
    let margin = phoenix
        .hawkeye(*owner)
        .view_margin_for_trader(trader)
        .await?
        .value;
    let phoenix_quote_lots = margin.collateral_quote_lots + margin.unrealized_pnl_quote_lots;
    Ok(phoenix_quote_lots as f64 / QUOTE_LOTS_PER_USD + wallet_usdc(rpc, owner)?)
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
        let path = std::env::temp_dir().join(format!("mt-session-{}", uuid::Uuid::new_v4()));
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
            std::env::var("MT_BOT_LOG").unwrap_or_else(|_| "info".into()),
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
