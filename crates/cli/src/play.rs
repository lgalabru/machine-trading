//! `mth scenario play`: register a scenario on a surfnet and chart the
//! market as it plays out, in a small web UI.
//!
//! The page is served by mth itself. It shows the planned path as soon as it
//! loads; pressing Play (or `--autoplay`) registers the scenario at the
//! current slot, and Pause stops the surfnet's clock, which holds the
//! scenario where it is. Samples of the market's BBO are taken every slot.

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::{get, post},
};
use mth_core::{
    chain,
    phoenix::Phoenix,
    snapshot::{self, OfflineWorld},
};
use serde::Serialize;
use serde_json::{Value, json};
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;
use solana_signer::Signer;
use surfpool_types::Scenario;
use tracing::{debug, info, warn};

use crate::{
    author::{self, Market, SLOTS_PER_SECOND},
    ui,
};

const PAGE: &str = include_str!("viewer.html");

/// A surfnet restored from a snapshot, and what a scenario on it starts from.
pub struct World {
    pub offline: OfflineWorld,
    pub market: Market,
    payer: Pubkey,
    snapshot_name: String,
}

impl World {
    /// Restore `snapshot_dir` offline. With `announce`, the restore is a
    /// step on the terminal.
    pub async fn start(
        snapshot_dir: &std::path::Path,
        symbol: Option<&str>,
        announce: bool,
    ) -> Result<Self> {
        let snapshot_name = snapshot_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| snapshot_dir.display().to_string());
        if announce {
            ui::start(format!("Restoring {}", ui::bold(&snapshot_name)));
        }
        let world = snapshot::start_offline(snapshot_dir)
            .await
            .inspect_err(|_| {
                if announce {
                    ui::fail(format!("Couldn't restore {snapshot_name}"));
                }
            })?;
        let symbol = symbol.unwrap_or(&world.manifest.symbol).to_string();
        let rpc = chain::rpc_client(&world.surfnet);
        // Hawkeye's views are simulated transactions: they need a fee payer
        // that exists.
        let payer = Keypair::new().pubkey();
        let airdrop = rpc.request_airdrop(&payer, 1_000_000_000)?;
        chain::wait_for(
            "the fee payer's airdrop",
            Duration::from_secs(10),
            async || Ok(rpc.confirm_transaction(&airdrop)?.then_some(())),
        )
        .await?;
        let phoenix = Phoenix::new(world.exchange.clone(), world.surfnet.rpc_url());
        let (mark_ticks, ..) = bbo(&phoenix, payer, &symbol).await?;
        let mark_usd = phoenix.ticks_to_usd(phoenix.market(&symbol)?, mark_ticks);
        let market = Market {
            perp_asset_map: phoenix.perp_asset_map()?.to_string(),
            surfnet_port: port_of(world.surfnet.rpc_url())?,
            symbol,
            mark_ticks,
            mark_usd,
        };
        info!(
            rpc = world.surfnet.rpc_url(),
            symbol = market.symbol,
            mark_ticks = market.mark_ticks,
            mark_usd,
            "surfnet ready"
        );
        if announce {
            ui::done(format!(
                "Surfnet ready {} {}",
                ui::bold(format!("{} ${:.2}", market.symbol, market.mark_usd)),
                ui::dim(format!("offline · {snapshot_name}"))
            ));
        }
        Ok(Self {
            offline: world,
            market,
            payer,
            snapshot_name,
        })
    }

    fn phoenix(&self) -> Phoenix {
        Phoenix::new(
            self.offline.exchange.clone(),
            self.offline.surfnet.rpc_url(),
        )
    }

    fn rpc_url(&self) -> String {
        self.offline.surfnet.rpc_url().to_string()
    }

    fn ws_url(&self) -> String {
        self.offline.surfnet.ws_url().to_string()
    }

    /// The accounts the workbench subscribes to: the one the scenario's
    /// overrides write, and the market they move.
    fn watched(&self) -> Vec<Watch> {
        let exchange = serde_json::to_value(&self.offline.exchange).unwrap_or_default();
        let market = exchange["markets"]
            .as_array()
            .and_then(|markets| markets.iter().find(|m| m["symbol"] == self.market.symbol));
        let mut watched = vec![Watch {
            label: "PerpAssetMap".into(),
            pubkey: self.market.perp_asset_map.clone(),
            note: "the oracle and mark prices the overrides write".into(),
        }];
        for (key, label, note) in [
            ("marketPubkey", "market", "the order book"),
            ("splinePubkey", "spline", "the market makers' quotes"),
        ] {
            if let Some(pubkey) = market.and_then(|m| m[key].as_str()) {
                watched.push(Watch {
                    label: format!("{} {label}", self.market.symbol),
                    pubkey: pubkey.into(),
                    note: note.into(),
                });
            }
        }
        watched
    }
}

/// An account the workbench subscribes to.
#[derive(Debug, Clone, Serialize)]
struct Watch {
    label: String,
    pubkey: String,
    note: String,
}

/// `(mark, best bid, best ask)` in ticks.
async fn bbo(phoenix: &Phoenix, payer: Pubkey, symbol: &str) -> Result<(u64, u64, u64)> {
    let bbo = phoenix.hawkeye(payer).view_bbo(symbol).await?.value;
    Ok((bbo.mark_price_ticks, bbo.best_bid_ticks, bbo.best_ask_ticks))
}

fn port_of(url: &str) -> Result<u16> {
    url.rsplit(':')
        .next()
        .and_then(|port| port.trim_end_matches('/').parse().ok())
        .with_context(|| format!("no port in {url}"))
}

pub struct PlayOptions {
    pub port: u16,
    pub open: bool,
    pub autoplay: bool,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct Sample {
    slot: u64,
    mark: f64,
    bid: Option<f64>,
    ask: Option<f64>,
}

#[derive(Serialize)]
struct Plan {
    name: String,
    description: String,
    symbol: String,
    snapshot: String,
    start_mark_usd: f64,
    /// The market-move targets, `[relative slot, usd]`, starting at 0.
    path: Vec<(u64, f64)>,
    overrides: Vec<Value>,
    end_slot: u64,
    /// Accounts the workbench streams over the surfnet's websocket.
    watch: Vec<Watch>,
}

/// One run of the scenario, from a fresh surfnet to wherever it got.
#[derive(Default)]
struct Live {
    /// Bumped on every reset, so the page knows to drop the old samples.
    generation: u64,
    slot: u64,
    base_slot: Option<u64>,
    paused: bool,
    resetting: bool,
    stopped: bool,
    rpc_url: String,
    ws_url: String,
    samples: Vec<Sample>,
    error: Option<String>,
}

/// The surfnet a run plays on, and the task sampling it.
struct Session {
    world: World,
    sampler: tokio::task::JoinHandle<()>,
}

struct AppState {
    plan: Plan,
    scenario: Scenario,
    snapshot_dir: PathBuf,
    symbol: String,
    live: Mutex<Live>,
    session: tokio::sync::Mutex<Option<Session>>,
    /// Signalled by Stop: shuts the server, and with it the surfnet, down.
    stop: tokio::sync::Notify,
}

impl AppState {
    fn live(&self) -> std::sync::MutexGuard<'_, Live> {
        self.live.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn rpc(&self) -> Result<RpcClient> {
        let live = self.live();
        if live.resetting || live.stopped {
            bail!(
                "the surfnet is {}",
                if live.stopped { "stopped" } else { "resetting" }
            );
        }
        Ok(mth_botkit::rpc_client(live.rpc_url.clone()))
    }

    /// Start the scenario: register it at the current slot the first time,
    /// resume the clock after a pause. Returns the base slot.
    fn play(&self) -> Result<u64> {
        let rpc = self.rpc()?;
        let (base, paused) = {
            let live = self.live();
            (live.base_slot, live.paused)
        };
        if let Some(base) = base {
            if paused {
                chain::call(&rpc, "surfnet_resumeClock", json!([]))?;
                self.live().paused = false;
                info!("clock resumed");
            }
            return Ok(base);
        }
        // Pin the base slot rather than reading it back, so slot 0 on the
        // chart is exactly where the scenario's offsets count from. A slot
        // ahead, so it can't pass while the request is in flight.
        let base = rpc.get_slot()? + 1;
        chain::call(
            &rpc,
            "surfnet_registerScenario",
            json!([self.scenario, base]),
        )?;
        self.live().base_slot = Some(base);
        info!(
            name = self.scenario.name,
            base_slot = base,
            "scenario registered"
        );
        Ok(base)
    }

    /// Stop the surfnet's clock: no slot passes, so no override lands.
    fn pause(&self) -> Result<()> {
        if self.live().base_slot.is_none() {
            return Ok(());
        }
        chain::call(&self.rpc()?, "surfnet_pauseClock", json!([]))?;
        self.live().paused = true;
        info!("clock paused");
        Ok(())
    }

    /// Put the market back where the scenario starts: restore the snapshot
    /// on a fresh surfnet and stage the scenario again. The chain can't be
    /// rewound in place, and the scenario's prices only make sense from its
    /// starting mark.
    async fn reset(self: &Arc<Self>) -> Result<()> {
        let mut session = self.session.lock().await;
        {
            let mut live = self.live();
            if live.stopped {
                bail!("the surfnet is stopped");
            }
            live.resetting = true;
        }
        if let Some(old) = session.take() {
            old.sampler.abort();
            drop(old.world);
        }
        // On its own thread: the restore's future isn't `Send`, which a
        // request handler's must be.
        let (dir, symbol) = (self.snapshot_dir.clone(), self.symbol.clone());
        let restored = tokio::task::spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(World::start(&dir, Some(&symbol), false))
        })
        .await
        .context("restoring the snapshot")
        .and_then(|restored| restored);
        match restored {
            Ok(world) => {
                {
                    let mut live = self.live();
                    *live = Live {
                        generation: live.generation + 1,
                        rpc_url: world.rpc_url(),
                        ws_url: world.ws_url(),
                        ..Live::default()
                    };
                }
                info!(rpc = world.rpc_url(), "reset to the snapshot");
                *session = Some(self.spawn_session(world));
                Ok(())
            }
            Err(error) => {
                self.live().resetting = false;
                Err(error)
            }
        }
    }

    fn spawn_session(self: &Arc<Self>, world: World) -> Session {
        let generation = self.live().generation;
        let sampler = tokio::spawn(sample(
            Arc::clone(self),
            generation,
            world.rpc_url(),
            world.phoenix(),
            world.payer,
            self.symbol.clone(),
        ));
        Session { world, sampler }
    }

    /// Where the scenario is, for the terminal.
    fn status(&self) -> Status {
        let live = self.live();
        let last = live.samples.last().map(|s| s.mark);
        if live.resetting {
            return Status::Resetting;
        }
        let Some(base) = live.base_slot else {
            return Status::Staged;
        };
        let at = live.slot.saturating_sub(base);
        if at >= self.plan.end_slot {
            return Status::Done {
                generation: live.generation,
                last,
            };
        }
        let next = self
            .plan
            .overrides
            .iter()
            .find(|o| o["slot"].as_u64() > Some(at));
        Status::Playing {
            at,
            paused: live.paused,
            last,
            next: next.and_then(|o| o["label"].as_str()).map(str::to_string),
        }
    }
}

enum Status {
    Staged,
    Playing {
        at: u64,
        paused: bool,
        last: Option<f64>,
        next: Option<String>,
    },
    Done {
        generation: u64,
        last: Option<f64>,
    },
    Resetting,
}

/// Serve the viewer until Ctrl-C or Stop. Returns a summary of what played.
pub async fn play(
    world: World,
    scenario: Scenario,
    snapshot_dir: PathBuf,
    opts: PlayOptions,
) -> Result<Value> {
    let phoenix = world.phoenix();
    let symbol = world.market.symbol.clone();
    let market = phoenix.market(&symbol)?;
    let usd = |ticks: u64| phoenix.ticks_to_usd(market, ticks);

    let mut path = vec![(0, world.market.mark_usd)];
    let mut moves: Vec<_> = scenario
        .overrides
        .iter()
        .filter(|o| {
            o.enabled
                && o.template_id == "phoenix-market-move"
                && o.values.get("symbol").and_then(Value::as_str) == Some(&symbol)
        })
        .filter_map(|o| Some((o.scenario_relative_slot, author::target_ticks(o)?)))
        .collect();
    moves.sort_by_key(|(slot, _)| *slot);
    path.extend(moves.into_iter().map(|(slot, ticks)| (slot, usd(ticks))));
    let mut overrides: Vec<Value> = scenario
        .overrides
        .iter()
        .filter(|o| o.enabled)
        .map(|o| {
            json!({
                "slot": o.scenario_relative_slot,
                "label": o.label.clone().unwrap_or_else(|| o.template_id.clone()),
                "template": o.template_id,
                "values": o.values,
            })
        })
        .collect();
    overrides.sort_by_key(|o| o["slot"].as_u64());
    let plan = Plan {
        name: scenario.name.clone(),
        description: scenario.description.clone(),
        symbol: symbol.clone(),
        snapshot: world.snapshot_name.clone(),
        start_mark_usd: world.market.mark_usd,
        end_slot: scenario
            .overrides
            .iter()
            .map(|o| o.scenario_relative_slot)
            .max()
            .unwrap_or(0),
        path,
        overrides,
        // The same accounts after a reset: it restores the same snapshot.
        watch: world.watched(),
    };

    let state = Arc::new(AppState {
        plan,
        scenario: scenario.clone(),
        snapshot_dir,
        symbol,
        live: Mutex::new(Live {
            rpc_url: world.rpc_url(),
            ws_url: world.ws_url(),
            ..Live::default()
        }),
        session: tokio::sync::Mutex::new(None),
        stop: tokio::sync::Notify::new(),
    });
    *state.session.lock().await = Some(state.spawn_session(world));

    let app = Router::new()
        .route("/", get(|| async { Html(PAGE) }))
        .route("/api/plan", get(get_plan))
        .route("/api/live/{from}", get(get_live))
        .route("/api/play", post(post_play))
        .route("/api/pause", post(post_pause))
        .route("/api/reset", post(post_reset))
        .route("/api/stop", post(post_stop))
        .with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], opts.port)))
        .await
        .context("binding the viewer")?;
    let url = format!("http://{}", listener.local_addr()?);
    info!(%url, "viewer ready; Ctrl-C to stop");
    ui::done(format!("Viewer at {}", ui::accent(&url)));
    if opts.open {
        open_browser(&url);
    }
    if opts.autoplay {
        state.play()?;
    }
    let reporter = tokio::spawn(report(Arc::clone(&state)));

    let stopping = Arc::clone(&state);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                () = stopping.stop.notified() => {}
            }
        })
        .await?;
    reporter.abort();
    if let Some(session) = state.session.lock().await.take() {
        session.sampler.abort();
    }
    let live = state.live();
    if live.stopped {
        ui::done("Stopped the surfnet from the viewer");
    } else {
        ui::clear();
    }
    Ok(json!({
        "scenario": scenario.name,
        "url": url,
        "runs": live.generation + 1,
        "base_slot": live.base_slot,
        "last_slot": live.slot,
        "samples": live.samples.len(),
        "start_mark_usd": state.plan.start_mark_usd,
        "last_mark_usd": live.samples.last().map(|s| s.mark),
    }))
}

/// Read the BBO once per slot until the task is aborted. Samples go to
/// `generation` only: a sampler outliving its reset writes nothing.
async fn sample(
    state: Arc<AppState>,
    generation: u64,
    rpc_url: String,
    phoenix: Phoenix,
    payer: Pubkey,
    symbol: String,
) {
    let rpc = mth_botkit::rpc_client(rpc_url);
    let Ok(market) = phoenix.market(&symbol) else {
        return;
    };
    let usd =
        |ticks: u64| (ticks > 0 && ticks != u64::MAX).then(|| phoenix.ticks_to_usd(market, ticks));
    let mut last = None;
    loop {
        let result: Result<()> = async {
            let slot = rpc.get_slot()?;
            if last != Some(slot) {
                let (mark, bid, ask) = bbo(&phoenix, payer, &symbol).await?;
                let mut live = state.live();
                if live.generation != generation {
                    return Ok(());
                }
                live.slot = slot;
                live.error = None;
                live.samples.push(Sample {
                    slot,
                    mark: phoenix.ticks_to_usd(market, mark),
                    bid: usd(bid),
                    ask: usd(ask),
                });
                last = Some(slot);
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            // Reads can fail while an override rewrites the market; the page
            // shows the last error, and the next slot usually reads fine.
            debug!("sampling the market: {error:#}");
            let mut live = state.live();
            if live.generation == generation {
                live.error = Some(format!("{error:#}"));
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Keep the terminal's status line in step with the scenario.
async fn report(state: Arc<AppState>) {
    let start = state.plan.start_mark_usd;
    let symbol = state.plan.symbol.clone();
    let end = state.plan.end_slot;
    let price = |mark: Option<f64>| match mark {
        Some(mark) => format!(
            "{symbol} ${mark:.2} {}",
            ui::dim(format!("{:+.2}%", (mark / start - 1.0) * 100.0))
        ),
        None => format!("{symbol} –"),
    };
    // The run whose end has been reported, and the run last seen.
    let mut finished = None;
    let mut seen = 0;
    ui::start_untimed("Ready · press Play in the browser");
    loop {
        let status = state.status();
        // A new run, however briefly it sat staged: Play may follow Reset
        // within one tick.
        let generation = state.live().generation;
        if generation != seen && !matches!(status, Status::Resetting) {
            seen = generation;
            ui::done(format!(
                "Reset {}",
                ui::dim(format!("{symbol} ${start:.2} · run {}", generation + 1))
            ));
            ui::start_untimed("Ready · press Play in the browser");
        }
        match status {
            Status::Resetting => ui::update("Resetting · restoring the snapshot"),
            Status::Staged => ui::update("Ready · press Play in the browser"),
            Status::Playing {
                at,
                paused,
                last,
                next,
            } => {
                let clock = format!(
                    "{} / {}",
                    ui::clock(at / SLOTS_PER_SECOND),
                    ui::clock(end / SLOTS_PER_SECOND)
                );
                let next = next
                    .map(|n| ui::dim(format!("· next: {n}")))
                    .unwrap_or_default();
                let verb = if paused { "Paused" } else { "Playing" };
                ui::update(format!("{verb} {clock} · {} {next}", price(last)));
            }
            Status::Done { generation, last } if finished != Some(generation) => {
                finished = Some(generation);
                let to = last.map_or_else(
                    || "–".to_string(),
                    |m| format!("${m:.2} ({:+.2}%)", (m / start - 1.0) * 100.0),
                );
                ui::done(format!(
                    "Played {} {}",
                    ui::bold(&state.plan.name),
                    ui::dim(format!("{symbol} ${start:.2} → {to}"))
                ));
                ui::start_untimed("Serving the viewer · Ctrl-C to quit");
            }
            Status::Done { .. } => {}
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn get_plan(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!(state.plan))
}

async fn get_live(
    State(state): State<Arc<AppState>>,
    Path(from): Path<usize>,
) -> impl IntoResponse {
    let live = state.live();
    Json(json!({
        "generation": live.generation,
        "slot": live.slot,
        "base_slot": live.base_slot,
        "paused": live.paused,
        "resetting": live.resetting,
        "stopped": live.stopped,
        "rpc_url": live.rpc_url,
        "ws_url": live.ws_url,
        "from": from.min(live.samples.len()),
        "samples": live.samples.get(from..).unwrap_or_default(),
        "error": live.error,
    }))
}

async fn post_play(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    blocking(move || state.play().map(|base| json!({ "base_slot": base }))).await
}

async fn post_pause(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    blocking(move || state.pause().map(|()| json!({ "paused": true }))).await
}

async fn post_reset(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match state.reset().await {
        Ok(()) => (
            StatusCode::OK,
            Json(json!({ "generation": state.live().generation })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{error:#}") })),
        ),
    }
}

/// Stop the session: answer, then shut the server down, which drops the
/// surfnet and ends `mth scenario play`.
async fn post_stop(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    state.live().stopped = true;
    state.stop.notify_one();
    (StatusCode::OK, Json(json!({ "stopped": true })))
}

/// Run a blocking RPC call off the runtime and answer with its result.
async fn blocking(
    call: impl FnOnce() -> Result<Value> + Send + 'static,
) -> (StatusCode, Json<Value>) {
    match tokio::task::spawn_blocking(call).await {
        Ok(Ok(value)) => (StatusCode::OK, Json(value)),
        Ok(Err(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("{error:#}") })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error.to_string() })),
        ),
    }
}

fn open_browser(url: &str) {
    let program = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    if let Err(error) = std::process::Command::new(program).arg(url).spawn() {
        warn!("couldn't open a browser ({error}); open {url} yourself");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(live: Live) -> AppState {
        let scenario: Scenario = serde_json::from_value(json!({
            "id": "s", "name": "crash", "description": "", "tags": [], "overrides": [],
        }))
        .unwrap();
        AppState {
            plan: Plan {
                name: "crash".into(),
                description: String::new(),
                symbol: "SPCX".into(),
                snapshot: "snap".into(),
                start_mark_usd: 100.0,
                path: vec![(0, 100.0)],
                overrides: vec![
                    json!({ "slot": 10, "label": "first leg" }),
                    json!({ "slot": 30, "label": "second leg" }),
                ],
                end_slot: 40,
                watch: vec![],
            },
            scenario,
            snapshot_dir: PathBuf::new(),
            symbol: "SPCX".into(),
            live: Mutex::new(live),
            session: tokio::sync::Mutex::new(None),
            stop: tokio::sync::Notify::new(),
        }
    }

    fn sample(slot: u64, mark: f64) -> Sample {
        Sample {
            slot,
            mark,
            bid: None,
            ask: None,
        }
    }

    #[test]
    fn status_follows_the_run() {
        let staged = state(Live {
            generation: 2,
            slot: 500,
            ..Live::default()
        });
        assert!(matches!(staged.status(), Status::Staged));

        let playing = state(Live {
            slot: 115,
            base_slot: Some(100),
            samples: vec![sample(115, 97.5)],
            ..Live::default()
        });
        match playing.status() {
            Status::Playing {
                at,
                paused,
                last,
                next,
            } => {
                assert_eq!((at, paused, last), (15, false, Some(97.5)));
                assert_eq!(next.as_deref(), Some("second leg"));
            }
            _ => panic!("expected Playing"),
        }

        let done = state(Live {
            slot: 140,
            base_slot: Some(100),
            ..Live::default()
        });
        assert!(matches!(done.status(), Status::Done { generation: 0, .. }));

        let resetting = state(Live {
            slot: 115,
            base_slot: Some(100),
            resetting: true,
            ..Live::default()
        });
        assert!(matches!(resetting.status(), Status::Resetting));
    }

    #[test]
    fn controls_refuse_a_surfnet_that_is_down() {
        for live in [
            Live {
                stopped: true,
                ..Live::default()
            },
            Live {
                resetting: true,
                ..Live::default()
            },
        ] {
            let state = state(live);
            assert!(state.play().is_err());
        }
        // Pausing before the scenario starts is a no-op, not an RPC call.
        assert!(state(Live::default()).pause().is_ok());
    }

    #[test]
    fn ports_come_from_rpc_urls() {
        assert_eq!(port_of("http://127.0.0.1:8899").unwrap(), 8899);
        assert_eq!(port_of("http://127.0.0.1:62048/").unwrap(), 62048);
        assert!(port_of("http://localhost").is_err());
    }

    #[test]
    fn the_page_is_embedded_with_its_controls() {
        for id in [
            "id=\"play\"",
            "id=\"reset\"",
            "id=\"stop\"",
            "id=\"chart\"",
            "id=\"strip\"",
            "id=\"workbench\"",
            "id=\"wb-toggle\"",
        ] {
            assert!(PAGE.contains(id), "viewer.html lost {id}");
        }
    }
}
