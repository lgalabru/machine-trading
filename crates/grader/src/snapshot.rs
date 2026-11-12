//! Pinned chain state for reproducible trading tasks.
//!
//! A snapshot directory holds:
//! - `surfnet.json.gz`: every account the surfnet loaded from mainnet,
//!   including programs, gzipped. Phoenix's books are mostly empty slots, so
//!   175 MB of JSON is under 3 MB: small enough for git. A surfnet starts from
//!   it decompressed once into a cache (see [`surfnet_file`]); a plain
//!   `surfnet.json` beside it, if there is one, is used as is.
//! - `exchange.json`: Phoenix's `/exchange` response taken at the same moment
//! - `manifest.json`: provenance and a few reference values
//! - `recipe.toml`: what the snapshot holds and how it is built
//!
//! A snapshot is built from its `recipe.toml` (see [`crate::recipe`]), which
//! forks mainnet and exercises the code paths a task will use, so the accounts
//! they touch get fetched. `verify` restarts from the files with no network and
//! checks that trading and scenarios still work.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use phoenix_rise::{core::LimitOrderTicket, math::Side, types::exchange::ExchangeResponse};
use serde::{Deserialize, Serialize};
use solana_keypair::Keypair;
use solana_signer::Signer;
use surfpool_sdk::{BlockProductionMode, Surfnet, SurfnetBuilder};
use tracing::{debug, warn};

use crate::{
    Result, chain,
    error::{Error, io_error},
    phoenix::{self, Phoenix},
};

const SURFNET_FILE: &str = "surfnet.json";
pub(crate) const SURFNET_GZ: &str = "surfnet.json.gz";
pub(crate) const EXCHANGE_FILE: &str = "exchange.json";
pub(crate) const MANIFEST_FILE: &str = "manifest.json";
/// Matches the CLI default, so scenario slot offsets mean the same thing everywhere.
pub(crate) const SLOT_TIME_MS: u64 = 250;
/// How long one surfnet start may take, and how many times to try. The SDK
/// picks its ports by binding and releasing them, so another process on the
/// machine can take one first: the runloop then exits on the bind error and
/// `start` waits for a ready event that never comes.
const SURFNET_START_TIMEOUT: Duration = Duration::from_secs(120);
const SURFNET_START_ATTEMPTS: u32 = 3;

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub created_at_unix: u64,
    /// Host only: datasource URLs often embed API keys.
    pub fork_host: String,
    pub fork_slot: u64,
    pub accounts: usize,
    pub symbol: String,
    pub mark_price_usd: f64,
    /// `spot` for a spot snapshot, which has no `exchange.json`; absent for
    /// Phoenix Eternal's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The SHA-256 of the `recipe.toml` it was built from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe_sha256: Option<String>,
    /// Every program and account the recipe declares.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<Source>,
    /// Whether the snapshot holds anything made rather than forked: a task
    /// that trades it says so.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub simulated: bool,
    /// What the recipe made (see [`crate::simulated`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created: Vec<Created>,
}

/// A program or account a recipe declares.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Source {
    /// `program`, or the declared kind: `market`, `mint`, `oracle`...
    pub role: String,
    pub name: String,
    pub address: String,
}

/// A mint or market a recipe made.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Created {
    /// `mint`, `phoenix-v1-market` or `openbook-v2-market`.
    pub kind: String,
    pub name: String,
    pub address: String,
    /// The program that owns it.
    pub program: String,
    /// Always true; says so where a reader looks at one entry.
    pub simulated: bool,
    pub detail: serde_json::Value,
}

#[derive(Debug, Serialize)]
pub struct VerifyReport {
    pub mark_before_usd: f64,
    pub mark_after_crash_usd: f64,
    pub bid_price_usd: f64,
    pub filled_base_lots: i64,
}

/// A surfnet restored from a snapshot directory, with no network access.
pub struct OfflineWorld {
    pub surfnet: Surfnet,
    pub manifest: Manifest,
    pub exchange: ExchangeResponse,
    pub exchange_path: PathBuf,
}

/// Who may reach an offline surfnet's RPC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Only at a secret path the grader alone knows. A graded bot shares the
    /// machine and can reach the port, but with no path it gets nothing: the
    /// guard (see [`crate::guard`]) is its only way in.
    Private,
    /// At the root, for trusted tools: `mtx scenario play`, whose authoring
    /// agent reaches the surfnet by its port through Surfpool's MCP server.
    Open,
}

/// Starts the surfnet `build` describes, trying again when a start fails or
/// hangs (see [`SURFNET_START_TIMEOUT`]). A hung start is left on its thread.
pub(crate) async fn start_surfnet(build: impl Fn() -> SurfnetBuilder) -> Result<Surfnet> {
    let mut failure = None;
    for attempt in 1..=SURFNET_START_ATTEMPTS {
        let builder = build();
        let (tx, rx) = tokio::sync::oneshot::channel();
        // `start` blocks its thread while it waits, so it gets one of its own.
        std::thread::Builder::new()
            .name("surfnet-start".into())
            .spawn(move || {
                let _ = tx.send(futures::executor::block_on(builder.start()));
            })
            .map_err(|e| Error::Timeout(format!("a surfnet start thread: {e}")))?;
        let error = match tokio::time::timeout(SURFNET_START_TIMEOUT, rx).await {
            Ok(Ok(Ok(surfnet))) => return Ok(surfnet),
            Ok(Ok(Err(e))) => Error::from(e),
            Ok(Err(_)) => Error::Timeout("a surfnet start that panicked".into()),
            Err(_) => Error::Timeout(format!(
                "a surfnet to start ({}s)",
                SURFNET_START_TIMEOUT.as_secs()
            )),
        };
        warn!(attempt, error = %error, "surfnet didn't start");
        failure = Some(error);
    }
    Err(failure.expect("at least one attempt"))
}

pub async fn start_offline(dir: &Path, access: Access) -> Result<OfflineWorld> {
    // Absolute, because bots get `exchange_path` and run from their own directory.
    let dir = &dir.canonicalize().map_err(io_error(dir))?;
    let exchange_path = dir.join(EXCHANGE_FILE);
    let exchange = phoenix::load_exchange(&exchange_path)?;
    let (surfnet, manifest) = start_snapshot(dir, access).await?;
    Ok(OfflineWorld {
        surfnet,
        manifest,
        exchange,
        exchange_path,
    })
}

/// Restore the snapshot in `dir` offline, of any kind: its surfnet, at the
/// snapshot's slot and time.
pub async fn start_snapshot(dir: &Path, access: Access) -> Result<(Surfnet, Manifest)> {
    let manifest: Manifest = read_json(&dir.join(MANIFEST_FILE))?;
    let accounts_file = surfnet_file(dir, &manifest)?;
    let surfnet = start_surfnet(|| {
        let builder = Surfnet::builder().offline(true);
        let builder = match access {
            Access::Private => builder.secret_path(format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            )),
            Access::Open => builder,
        };
        builder
            .snapshot_file(accounts_file.clone())
            .block_production_mode(BlockProductionMode::Clock)
            .slot_time_ms(SLOT_TIME_MS)
            // Profiles re-execute every transaction instruction by instruction and
            // keep the accounts it touched, megabytes each for Phoenix's: a
            // market-making session's hundreds of orders and Hawkeye reads ran a
            // grader past 8 GB. Nothing here reads profiles.
            .instruction_profiling(false)
            .max_profiles(1)
    })
    .await?;
    chain::forward_surfnet_logs(&surfnet);
    // Phoenix reads both the slot and the unix time: it compares oracle and mark
    // update slots against the clock, and its oracle update walks the time since
    // the last update, running out of compute when that is long. Restore both
    // to when the snapshot was taken, shortly after the fork slot.
    surfnet
        .cheatcodes()
        .time_travel_to_slot_at(manifest.fork_slot, manifest.created_at_unix * 1_000)?;
    Ok((surfnet, manifest))
}

/// Start offline from the snapshot, rest a bid below the market, crash the
/// market through it with a scenario, and check the bid filled.
pub async fn verify(dir: &Path, crash_pct: f64) -> Result<VerifyReport> {
    let world = start_offline(dir, Access::Open).await?;
    crash_test(
        &world.surfnet,
        world.exchange,
        &world.manifest.symbol,
        crash_pct,
    )
    .await
}

/// Same crash test against a live mainnet fork, to tell snapshot problems
/// apart from scenario or program problems.
pub async fn verify_fork(
    fork_rpc_url: &str,
    phoenix_api_url: &str,
    symbol: &str,
    crash_pct: f64,
) -> Result<VerifyReport> {
    let exchange = phoenix::fetch_exchange(phoenix_api_url).await?;
    let surfnet = Surfnet::builder()
        .remote_rpc_url(fork_rpc_url)
        .block_production_mode(BlockProductionMode::Clock)
        .slot_time_ms(SLOT_TIME_MS)
        // Profiles re-execute every transaction instruction by instruction and
        // keep the accounts it touched, megabytes each for Phoenix's: a
        // market-making session's hundreds of orders and Hawkeye reads ran a
        // grader past 8 GB. Nothing here reads profiles.
        .instruction_profiling(false)
        .max_profiles(1)
        .start()
        .await?;
    chain::forward_surfnet_logs(&surfnet);
    crash_test(&surfnet, exchange, symbol, crash_pct).await
}

async fn crash_test(
    surfnet: &Surfnet,
    exchange: ExchangeResponse,
    symbol: &str,
    crash_pct: f64,
) -> Result<VerifyReport> {
    let rpc = chain::rpc_client(surfnet);
    let phoenix = Phoenix::new(exchange, surfnet.rpc_url());
    let market = phoenix.market(symbol)?;

    debug!(
        slot = rpc.get_slot()?,
        perp_asset_map_loaded = rpc.get_account(&phoenix.perp_asset_map()?).is_ok(),
        "offline surfnet started"
    );

    let bot = Keypair::new();
    let trader = phoenix::onboard_trader(surfnet, &phoenix, &bot, 5_000.0).await?;
    let hawkeye = phoenix.hawkeye(bot.pubkey());
    let mark_ticks = hawkeye.view_bbo(symbol).await?.value.mark_price_ticks;
    let mark_before_usd = phoenix.ticks_to_usd(market, mark_ticks);

    let bid_price_usd = mark_before_usd * (1.0 - crash_pct / 200.0);
    let bid = LimitOrderTicket::builder()
        .authority(bot.pubkey())
        .trader_account(trader)
        .symbol(symbol)
        .side(Side::Bid)
        .price(bid_price_usd)
        .num_base_lots(phoenix.base_units_to_lots(market, 5.0))
        .build()?;
    mtx_kit::send(
        &rpc,
        &bot,
        phoenix.tx().place_limit_order(bid).await?,
        "resting bid",
    )?;

    let crash_ticks = (mark_ticks as f64 * (1.0 - crash_pct / 100.0)).round() as u64;
    let perp_asset_map = phoenix.perp_asset_map()?;
    let (overrides, _) = phoenix::market_move(perp_asset_map, symbol, mark_ticks, crash_ticks, 4);
    chain::register_scenario(&rpc, "verify crash", overrides)?;

    let filled_base_lots =
        chain::wait_for("the bid to fill", Duration::from_secs(30), async || {
            let asset = hawkeye
                .view_margin_for_asset(bot.pubkey(), 0, 0, market.asset_id)
                .await?
                .value;
            Ok((asset.base_lots > 0).then_some(asset.base_lots))
        })
        .await?;
    let mark_after = hawkeye.view_bbo(symbol).await?.value.mark_price_ticks;
    Ok(VerifyReport {
        mark_before_usd,
        mark_after_crash_usd: phoenix.ticks_to_usd(market, mark_after),
        bid_price_usd,
        filled_base_lots,
    })
}

pub(crate) fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .and_then(|rest| rest.split(['/', '?']).next())
        .unwrap_or("unknown")
        .to_string()
}

/// The surfnet's accounts as a plain file, which is what Surfpool loads: the
/// snapshot's `surfnet.json` if there is one, else its `surfnet.json.gz`
/// decompressed into the system's temp directory, once per snapshot (keyed by
/// its fork slot and the compressed file's size), and written atomically so
/// concurrent sessions share it.
pub fn surfnet_file(dir: &Path, manifest: &Manifest) -> Result<PathBuf> {
    let plain = dir.join(SURFNET_FILE);
    if plain.is_file() {
        return Ok(plain);
    }
    let gz = dir.join(SURFNET_GZ);
    let size = std::fs::metadata(&gz).map_err(io_error(&gz))?.len();
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let cache = std::env::temp_dir().join("mtx-snapshots");
    let out = cache.join(format!("{name}-{}-{size}.json", manifest.fork_slot));
    if out.is_file() {
        return Ok(out);
    }
    std::fs::create_dir_all(&cache).map_err(io_error(&cache))?;
    let partial = cache.join(format!(
        ".{}.{}",
        uuid::Uuid::new_v4().simple(),
        SURFNET_FILE
    ));
    let unpack = || -> std::io::Result<()> {
        let mut input =
            flate2::read::GzDecoder::new(std::io::BufReader::new(std::fs::File::open(&gz)?));
        let mut output = std::io::BufWriter::new(std::fs::File::create(&partial)?);
        std::io::copy(&mut input, &mut output)?;
        std::io::Write::flush(&mut output)?;
        std::fs::rename(&partial, &out)
    };
    if let Err(error) = unpack() {
        let _ = std::fs::remove_file(&partial);
        return Err(io_error(&gz)(error));
    }
    debug!(snapshot = %gz.display(), file = %out.display(), "snapshot decompressed");
    Ok(out)
}

pub(crate) fn write_gz(path: &Path, value: &impl Serialize) -> Result<()> {
    let file = std::fs::File::create(path).map_err(io_error(path))?;
    let mut gz =
        flate2::write::GzEncoder::new(std::io::BufWriter::new(file), flate2::Compression::best());
    serde_json::to_writer(&mut gz, value)?;
    gz.finish()
        .and_then(|mut w| std::io::Write::flush(&mut w))
        .map_err(io_error(path))
}

pub(crate) fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let contents = serde_json::to_string_pretty(value)?;
    std::fs::write(path, contents).map_err(io_error(path))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let contents = std::fs::read_to_string(path).map_err(io_error(path))?;
    Ok(serde_json::from_str(&contents)?)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_gzipped_snapshot_is_decompressed_once_and_a_plain_one_wins() {
        let dir = std::env::temp_dir().join(format!("mtx-snap-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = Manifest {
            created_at_unix: 0,
            fork_host: String::new(),
            fork_slot: 42,
            accounts: 1,
            symbol: "SPCX".into(),
            mark_price_usd: 1.0,
            kind: None,
            recipe_sha256: None,
            sources: Vec::new(),
            simulated: false,
            created: Vec::new(),
        };
        write_gz(&dir.join(SURFNET_GZ), &json!({ "a": { "lamports": 1 } })).unwrap();
        let first = surfnet_file(&dir, &manifest).unwrap();
        let text = std::fs::read_to_string(&first).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&text).unwrap()["a"]["lamports"],
            1
        );
        assert_eq!(surfnet_file(&dir, &manifest).unwrap(), first, "cached");
        std::fs::write(dir.join(SURFNET_FILE), "{}").unwrap();
        assert_eq!(
            surfnet_file(&dir, &manifest).unwrap(),
            dir.join(SURFNET_FILE)
        );
        let _ = std::fs::remove_file(&first);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
