//! Strategies as WebAssembly components (`docs/rfd/0002-wasm-strategies.md`):
//! a bot directory with a `bot.wasm` is a component of the `mtx:strategy`
//! world (`crates/kit/wit`), which the grader calls instead of starting
//! a process.
//!
//! The component gets no files, no environment and no network. Its WASI
//! clocks are fixed and its randomness is seeded, so a step's result depends
//! only on what the grader hands it and what `rpc` reads, which is the
//! paused chain. Each call runs on a fuel budget, and the session on another:
//! a strategy that runs out of either traps, and Wasmtime won't enter it
//! again, so it plays no more of the session. Its memory is capped. A
//! component that doesn't load plays the session without trading.
//!
//! A component is built on `mtx:strategy@0.2.0` (`wit/strategy.wit`) or on
//! 0.1.0 (`wit/0.1.0/strategy.wit`). Both play the same way: the grader
//! reads the market into `mtx_kit::Market`, hands each version its own
//! view of it, and plans either's sheet with `mtx_kit::plan_sheet`. A 0.1.0
//! sheet is a 0.2.0 sheet whose lines say nothing beyond their intent.

use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use mtx_kit::{Action, Intent, LineOptions, Planned, SendState, SendStatus};
use rand::SeedableRng;
use wasmtime::{
    Config, Engine, Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, HasSelf, Linker, ResourceTable},
};
use wasmtime_wasi::{
    HostMonotonicClock, HostWallClock, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView,
};

use crate::Error;

/// `mtx:strategy@0.2.0`, the kit's.
pub mod v2 {
    wasmtime::component::bindgen!({
        world: "strategy",
        path: "../kit/wit/strategy.wit",
        imports: { default: async },
        exports: { default: async },
    });
}

/// `mtx:strategy@0.1.0`, which the tasks released before 0.2.0 build on.
pub mod v1 {
    wasmtime::component::bindgen!({
        world: "strategy",
        path: "../kit/wit/0.1.0/strategy.wit",
        imports: { default: async },
        exports: { default: async },
    });
}

/// The file a bot directory holds when its bot is a component.
pub const BOT_WASM: &str = "bot.wasm";

/// Fuel for one call to `start`, `step` or `end`: Wasmtime spends about one
/// unit per instruction.
pub const FUEL_PER_CALL: u64 = 10_000_000_000;

/// Fuel for a session, per step: a session may spend `steps + 2` times this
/// in all, whatever it spends in each call. About 0.1 s of strategy a step
/// on an M-series core (20 billion units a second), against the 75 million
/// the funding-carry reference spends, the most of the references.
pub const FUEL_PER_STEP: u64 = 2_000_000_000;

/// The most linear memory a strategy may grow to, in bytes.
pub const MEMORY_BYTES: usize = 1 << 30;

/// The most `rpc` calls one step may make.
const RPC_CALLS_PER_STEP: u32 = 256;

/// RPC methods a strategy may call: the kit's list, which `mtx_kit::rpc`
/// checks on its side too.
use mtx_kit::RPC_METHODS;

/// The bot directory's component, if it has one.
pub fn component_in(bot_dir: &Path) -> Option<PathBuf> {
    let path = bot_dir.join(BOT_WASM);
    path.is_file().then_some(path)
}

/// What the strategy's host side holds.
struct Host {
    wasi: WasiCtx,
    table: ResourceTable,
    log: Box<dyn Write + Send>,
    rpc_url: String,
    http: reqwest::Client,
    rpc_calls: u32,
    limits: StoreLimits,
}

impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

impl Host {
    fn log(&mut self, line: String) {
        let _ = writeln!(self.log, "{line}");
    }

    async fn rpc(&mut self, method: String, params: String) -> std::result::Result<String, String> {
        if !RPC_METHODS.contains(&method.as_str()) {
            return Err(format!("{method} is not a read a strategy may make"));
        }
        self.rpc_calls += 1;
        if self.rpc_calls > RPC_CALLS_PER_STEP {
            return Err(format!(
                "more than {RPC_CALLS_PER_STEP} rpc calls this step"
            ));
        }
        let params: serde_json::Value =
            serde_json::from_str(&params).map_err(|e| format!("params are not JSON: {e}"))?;
        let body =
            serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let response = self
            .http
            .post(&self.rpc_url)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| e.to_string())?
            .text()
            .await
            .map_err(|e| e.to_string())?;
        let mut answer: serde_json::Value = serde_json::from_str(&response)
            .map_err(|e| format!("the node's answer is not JSON: {e}"))?;
        match answer.get("error") {
            Some(error) if !error.is_null() => Err(error.to_string()),
            _ => Ok(answer["result"].take().to_string()),
        }
    }
}

impl v2::mtx::strategy::types::Host for Host {}

impl v2::StrategyImports for Host {
    async fn log(&mut self, line: String) {
        Host::log(self, line)
    }

    async fn rpc(&mut self, method: String, params: String) -> std::result::Result<String, String> {
        Host::rpc(self, method, params).await
    }
}

impl v1::mtx::strategy::types::Host for Host {}

impl v1::StrategyImports for Host {
    async fn log(&mut self, line: String) {
        Host::log(self, line)
    }

    async fn rpc(&mut self, method: String, params: String) -> std::result::Result<String, String> {
        Host::rpc(self, method, params).await
    }
}

/// A clock that never moves: a strategy can't measure time, so its result
/// can't depend on how fast the machine is.
struct Stopped;

impl HostWallClock for Stopped {
    fn resolution(&self) -> Duration {
        Duration::from_secs(1)
    }

    fn now(&self) -> Duration {
        Duration::ZERO
    }
}

impl HostMonotonicClock for Stopped {
    fn resolution(&self) -> u64 {
        1_000_000_000
    }

    fn now(&self) -> u64 {
        0
    }
}

/// What one call to the strategy came to.
#[derive(Debug)]
pub enum Outcome<T> {
    /// It returned.
    Done(T),
    /// It returned an error of its own.
    Failed(String),
    /// It trapped: a panic, or out of fuel.
    Trapped(String),
}

/// The world a component was built on, and its instance.
enum Bindings {
    V2(v2::Strategy),
    V1(v1::Strategy),
}

/// A strategy component, instantiated for one session.
pub struct WasmStrategy {
    store: Store<Host>,
    strategy: Bindings,
    /// The session's fuel left, what the call in progress was given, what
    /// the session has spent, and the most one call spent.
    fuel_left: u64,
    fuel_given: u64,
    pub fuel_used: u64,
    pub fuel_max_call: u64,
}

impl WasmStrategy {
    /// Instantiates the component at `path`, of either world. `rpc_url`
    /// answers its `rpc` calls, its log lines go to `log`, `seed` seeds its
    /// randomness, and `session_fuel` is all it may spend.
    pub async fn load(
        path: &Path,
        rpc_url: &str,
        log: Box<dyn Write + Send>,
        seed: u64,
        session_fuel: u64,
    ) -> crate::Result<Self> {
        let wasm_error = |e: wasmtime::Error| Error::Wasm(format!("{}: {e:#}", path.display()));
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(wasm_error)?;
        let component = Component::from_file(&engine, path).map_err(wasm_error)?;
        let linker = |add: fn(&mut Linker<Host>) -> wasmtime::Result<()>| {
            let mut linker: Linker<Host> = Linker::new(&engine);
            wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;
            add(&mut linker)?;
            Ok::<_, wasmtime::Error>(linker)
        };
        let v2_linker = linker(|l| v2::Strategy::add_to_linker::<_, HasSelf<_>>(l, |host| host))
            .map_err(wasm_error)?;
        let v1_linker = linker(|l| v1::Strategy::add_to_linker::<_, HasSelf<_>>(l, |host| host))
            .map_err(wasm_error)?;
        // Which world it was built on: the one whose imports and exports
        // type-check, before any of its code runs.
        let pre = match v2_linker
            .instantiate_pre(&component)
            .and_then(v2::StrategyPre::new)
        {
            Ok(pre) => Ok(pre),
            Err(v2_error) => Err(v1_linker
                .instantiate_pre(&component)
                .and_then(v1::StrategyPre::new)
                .map_err(|_| wasm_error(v2_error))?),
        };
        let wasi = WasiCtxBuilder::new()
            .wall_clock(Stopped)
            .monotonic_clock(Stopped)
            .secure_random(rand::rngs::StdRng::seed_from_u64(seed))
            .insecure_random(rand::rngs::StdRng::seed_from_u64(seed.rotate_left(32)))
            .insecure_random_seed(u128::from(seed))
            .allow_tcp(false)
            .allow_udp(false)
            .allow_ip_name_lookup(false)
            .build();
        let host = Host {
            wasi,
            table: ResourceTable::new(),
            log,
            rpc_url: rpc_url.to_string(),
            http: reqwest::Client::new(),
            rpc_calls: 0,
            limits: StoreLimitsBuilder::new()
                .memory_size(MEMORY_BYTES)
                .trap_on_grow_failure(true)
                .build(),
        };
        let mut store = Store::new(&engine, host);
        store.limiter(|host| &mut host.limits);
        // Instantiating runs no strategy code worth metering apart: it comes
        // out of the session's fuel like a call.
        let mut this_fuel = session_fuel.min(FUEL_PER_CALL);
        store.set_fuel(this_fuel).map_err(wasm_error)?;
        let strategy = match pre {
            Ok(v2) => Bindings::V2(v2.instantiate_async(&mut store).await.map_err(wasm_error)?),
            Err(v1) => Bindings::V1(v1.instantiate_async(&mut store).await.map_err(wasm_error)?),
        };
        let used = this_fuel - store.get_fuel().unwrap_or(0);
        this_fuel = session_fuel.saturating_sub(used);
        Ok(Self {
            store,
            strategy,
            fuel_left: this_fuel,
            fuel_given: 0,
            fuel_used: used,
            fuel_max_call: used,
        })
    }

    /// The world the component was built on: `0.2.0` or `0.1.0`.
    pub fn interface(&self) -> &'static str {
        match self.strategy {
            Bindings::V2(_) => "0.2.0",
            Bindings::V1(_) => "0.1.0",
        }
    }

    /// Fuel for the next call, from what the session has left: `false` if
    /// none is left.
    fn refuel(&mut self) -> bool {
        self.store.data_mut().rpc_calls = 0;
        self.fuel_given = self.fuel_left.min(FUEL_PER_CALL);
        self.fuel_given > 0 && self.store.set_fuel(self.fuel_given).is_ok()
    }

    /// What the last call spent, out of the session's fuel.
    fn charge(&mut self) {
        let used = self.fuel_given - self.store.get_fuel().unwrap_or(0).min(self.fuel_given);
        self.fuel_left -= used;
        self.fuel_used += used;
        self.fuel_max_call = self.fuel_max_call.max(used);
    }

    fn spent() -> String {
        "the session's fuel is spent".to_string()
    }

    pub async fn start(&mut self, session: &mtx_kit::Session) -> Outcome<()> {
        if !self.refuel() {
            return Outcome::Trapped(Self::spent());
        }
        let mandate = match serde_json::to_string(&session.mandate) {
            Ok(mandate) => mandate,
            Err(e) => return Outcome::Failed(format!("the mandate: {e}")),
        };
        let result = match &self.strategy {
            Bindings::V2(s) => {
                s.call_start(&mut self.store, &wit2::session(session, mandate))
                    .await
            }
            Bindings::V1(s) => {
                s.call_start(&mut self.store, &wit1::session(session, mandate))
                    .await
            }
        };
        self.charge();
        match result {
            Ok(Ok(())) => Outcome::Done(()),
            Ok(Err(e)) => Outcome::Failed(e),
            Err(trap) => Outcome::Trapped(format!("{trap:#}")),
        }
    }

    /// The strategy's sheet for `market`: its lines, and their options.
    pub async fn step(&mut self, market: &mtx_kit::Market) -> Outcome<Vec<(Intent, LineOptions)>> {
        if !self.refuel() {
            return Outcome::Trapped(Self::spent());
        }
        let result = match &self.strategy {
            Bindings::V2(s) => s
                .call_step(&mut self.store, &wit2::market(market))
                .await
                .map(|r| r.map(|intents| intents.into_iter().map(wit2::line).collect())),
            Bindings::V1(s) => s
                .call_step(&mut self.store, wit1::market(market))
                .await
                .map(|r| {
                    r.map(|intents| {
                        intents
                            .into_iter()
                            .map(|i| (wit1::intent(i), LineOptions::default()))
                            .collect()
                    })
                }),
        };
        self.charge();
        match result {
            Ok(Ok(lines)) => Outcome::Done(lines),
            Ok(Err(e)) => Outcome::Failed(e),
            Err(trap) => Outcome::Trapped(format!("{trap:#}")),
        }
    }

    pub async fn end(&mut self, last: Option<&mtx_kit::Market>) -> Outcome<()> {
        if !self.refuel() {
            return Outcome::Trapped(Self::spent());
        }
        let result = match &self.strategy {
            Bindings::V2(s) => {
                let last = last.map(wit2::market);
                s.call_end(&mut self.store, last.as_ref()).await
            }
            Bindings::V1(s) => s.call_end(&mut self.store, last.map(wit1::market)).await,
        };
        self.charge();
        match result {
            Ok(()) => Outcome::Done(()),
            Err(trap) => Outcome::Trapped(format!("{trap:#}")),
        }
    }
}

/// A session's clock, as a component's `start` sees it, and its scenario.
pub struct Clock<'a> {
    pub steps: u64,
    pub slots_per_step: u64,
    pub start_slot: u64,
    pub scenario: &'a str,
    /// The task's variables for the session ([`mtx_kit::SESSION_VARS`]).
    pub vars: Vec<(String, String)>,
}

/// A component playing one session: the strategy, and the venue the grader
/// trades for it, with the bot's key, through the bot's guarded endpoint.
/// Reads and sends as `mtx_kit::run` does for a process bot.
pub struct WasmBot {
    /// `None` if the component didn't load: the session plays without it.
    strategy: Option<WasmStrategy>,
    trader: mtx_kit::Trader,
    session: mtx_kit::Session,
    pnl: f64,
    peak_pnl: f64,
    last_mark: Option<f64>,
    resting: (f64, f64),
    last: Option<mtx_kit::Market>,
    /// What became of the last sheet, and the planner's notes on it: the
    /// next step's market carries them.
    sends: Vec<SendStatus>,
    notes: Vec<String>,
    /// Lines that carried transaction options, which no session applies
    /// yet.
    pub tx_options: u64,
    /// Why the strategy plays no more of the session, if it doesn't: it
    /// didn't load, or it trapped (Wasmtime won't enter its instance again).
    pub stopped: Option<String>,
}

impl WasmBot {
    /// Loads the component and calls its `start`. `env` is the bot's
    /// environment, as a process bot would get it.
    pub async fn start(
        path: &Path,
        env: &mtx_kit::BotEnv,
        mandate: &mtx_core::Mandate,
        clock: Clock<'_>,
        log: Box<dyn Write + Send>,
    ) -> crate::Result<Self> {
        let Clock {
            steps,
            slots_per_step,
            start_slot,
            scenario,
            vars,
        } = clock;
        let trader = mtx_kit::Trader::connect(env)?;
        let start_mark = trader.mark_usd().await?;
        let start_oracle = trader.oracle_usd().await?;
        let seed = seed_of(scenario);
        let session_fuel = (steps + 2).saturating_mul(FUEL_PER_STEP);
        let mut stopped = None;
        let mut strategy = match WasmStrategy::load(path, &env.rpc_url, log, seed, session_fuel)
            .await
        {
            Ok(strategy) => {
                tracing::info!(interface = strategy.interface(), "strategy component");
                Some(strategy)
            }
            Err(error) => {
                tracing::warn!(%error, "the component didn't load; the session plays without it");
                stopped = Some(format!("didn't load: {error}"));
                None
            }
        };
        let session = mtx_kit::Session {
            steps,
            slots_per_step,
            start_slot,
            end_slot: env.end_slot,
            symbol: env.symbol.clone(),
            start_mark,
            start_oracle,
            cap: env.max_position,
            max_drawdown: env.max_drawdown_usd,
            mandate: mandate.clone(),
            venue: std::fs::read_to_string(&env.exchange_path)
                .map_err(crate::error::io_error(&env.exchange_path))?,
            authority: trader.authority().to_string(),
            vars,
            seed,
            venues: mtx_kit::VenueInfo::of_perp_session(mandate, &env.symbol),
            applies_tx_options: false,
        };
        if let Some(strategy) = strategy.as_mut() {
            match strategy.start(&session).await {
                Outcome::Done(()) => {}
                Outcome::Failed(e) => {
                    tracing::warn!(error = %e, "the strategy's start failed; it plays on");
                }
                Outcome::Trapped(e) => {
                    tracing::warn!(error = %e, "the strategy's start trapped; it plays no more of the session");
                    stopped = Some(format!("trapped in start: {e}"));
                }
            }
        }
        Ok(Self {
            strategy,
            trader,
            session,
            pnl: 0.0,
            peak_pnl: 0.0,
            last_mark: None,
            resting: (0.0, 0.0),
            last: None,
            sends: Vec::new(),
            notes: Vec::new(),
            tx_options: 0,
            stopped,
        })
    }

    /// The strategy's turn at `step`: read the market, call `step`, cut the
    /// sheet to the mandate's cap and send it, in order, stopping at the
    /// first order that fails. The step's turn data, if the scenario reports
    /// any, goes into the market. Returns whether the strategy answered.
    pub async fn turn(&mut self, step: &mtx_core::Step) -> bool {
        let (k, slot, last) = (step.k, step.slot, step.last);
        if self.stopped.is_some() {
            return false;
        }
        let (top, oracle, position) = match (
            self.trader.top().await,
            self.trader.oracle_usd().await,
            self.trader.position().await,
        ) {
            (Ok(top), Ok(oracle), Ok(position)) => (top, oracle, position),
            (top, oracle, position) => {
                tracing::warn!(step = k, mark = ?top.err(), oracle = ?oracle.err(), position = ?position.err(), "couldn't read the market; skipping the step");
                return false;
            }
        };
        let mark = top.mark;
        if let Some(previous) = self.last_mark {
            self.pnl += position.size * (mark - previous);
        }
        self.last_mark = Some(mark);
        self.peak_pnl = self.peak_pnl.max(self.pnl);
        let (venues, fills) = mtx_kit::venue_views(
            &self.session.venues,
            top.best_bid,
            top.best_ask,
            position,
            &step.tape,
            &step.depth,
        );
        let market = mtx_kit::Market {
            k,
            slot,
            last,
            steps_left: self.session.steps.saturating_sub(k + 1),
            slots_left: self.session.end_slot.saturating_sub(slot),
            mark,
            oracle,
            index: top.index,
            best_bid: top.best_bid,
            best_ask: top.best_ask,
            position,
            cap: self.session.cap,
            max_drawdown: self.session.max_drawdown,
            resting_bids: self.resting.0,
            resting_asks: self.resting.1,
            pnl: self.pnl,
            peak_pnl: self.peak_pnl,
            drawdown: self.peak_pnl - self.pnl,
            tape: step.tape.clone(),
            depth: step.depth.clone(),
            unix_timestamp: step.unix_timestamp,
            venues,
            sends: std::mem::take(&mut self.sends),
            fills,
            rfqs: Vec::new(),
            notes: std::mem::take(&mut self.notes),
        };
        let Some(strategy) = self.strategy.as_mut() else {
            self.last = Some(market);
            return false;
        };
        let lines = match strategy.step(&market).await {
            Outcome::Done(lines) => lines,
            Outcome::Failed(e) => {
                tracing::warn!(step = k, error = %e, "the strategy failed; no orders this step");
                self.last = Some(market);
                return true;
            }
            Outcome::Trapped(e) => {
                tracing::warn!(step = k, error = %e, "the strategy trapped; it plays no more of the session");
                self.stopped = Some(format!("trapped at step {k}: {e}"));
                self.last = Some(market);
                return false;
            }
        };
        let mut orders = mtx_kit::Orders::new();
        for (intent, options) in lines {
            if options.tx.is_some() {
                self.tx_options += 1;
            }
            orders.push(intent, options);
        }
        let (planned, notes) = mtx_kit::plan_sheet(&orders, &market);
        for note in &notes {
            tracing::warn!(step = k, "{note}");
        }
        self.notes = notes;
        self.last = Some(market);
        let own = self.session.venues[0].id.clone();
        // The orders sent to the session's own market, the one that failed
        // included: the ledger of what rests only over-counts.
        let mut tried = Vec::new();
        let mut failed = false;
        for Planned {
            line,
            venue,
            action,
            ..
        } in planned
        {
            let state = if failed {
                SendState::NotSent("an earlier order of the step failed".into())
            } else if let Some(venue) = &venue {
                SendState::NotSent(format!("{venue} isn't traded in this session"))
            } else if let Action::Respond { .. } = action {
                SendState::NotSent("no requests for quotes in this session".into())
            } else {
                tried.push(action.clone());
                match self.send(&action).await {
                    Ok(signature) => SendState::Landed(signature),
                    Err(error) => {
                        tracing::warn!(step = k, ?action, %error, "order failed; the step's other orders aren't sent");
                        failed = true;
                        SendState::Failed(error)
                    }
                }
            };
            self.sends.push(SendStatus {
                step: k,
                line: line as u32,
                venue: venue.unwrap_or_else(|| own.clone()),
                order: action,
                state,
            });
        }
        self.resting = mtx_kit::resting_after(self.resting.0, self.resting.1, &tried);
        true
    }

    /// Send one order to the session's own market: its signature, or why it
    /// failed.
    async fn send(&self, action: &Action) -> std::result::Result<String, String> {
        let result = match *action {
            Action::Market { side, size, limit } => {
                self.trader.place_market(side, size, limit).await
            }
            Action::Limit { side, price, size } => self.trader.place_limit(side, price, size).await,
            Action::PostOnly { side, price, size } => {
                self.trader.place_post_only(side, price, size).await
            }
            Action::CancelAll => self.trader.cancel_all(),
            Action::Respond { .. } => return Err("no requests for quotes".into()),
        };
        result.map(|s| s.to_string()).map_err(|e| e.to_string())
    }

    /// The session is over: the strategy's `end`.
    pub async fn end(&mut self) {
        let last = self.last.take();
        if self.stopped.is_some() {
            return;
        }
        if let Some(strategy) = self.strategy.as_mut()
            && let Outcome::Trapped(e) = strategy.end(last.as_ref()).await
        {
            tracing::warn!(error = %e, "the strategy's end trapped");
        }
    }

    /// The fuel the session spent, and the most one call spent.
    pub fn fuel(&self) -> (u64, u64) {
        self.strategy
            .as_ref()
            .map_or((0, 0), |s| (s.fuel_used, s.fuel_max_call))
    }
}

/// `mtx:strategy@0.2.0`'s side of the kit's types.
mod wit2 {
    use mtx_kit::{
        Action, Intent, LineOptions, Market, SendState, Session, Side, TimeInForce, TxOptions,
    };

    use super::v2::mtx::strategy::types as w;

    pub fn session(s: &Session, mandate: String) -> w::Session {
        w::Session {
            steps: s.steps,
            slots_per_step: s.slots_per_step,
            start_slot: s.start_slot,
            end_slot: s.end_slot,
            symbol: s.symbol.clone(),
            start_mark: s.start_mark,
            start_oracle: s.start_oracle,
            cap: s.cap,
            max_drawdown: s.max_drawdown,
            mandate,
            venue: s.venue.clone(),
            authority: s.authority.clone(),
            vars: s.vars.clone(),
            seed: s.seed,
            venues: s
                .venues
                .iter()
                .map(|v| w::VenueInfo {
                    id: v.id.clone(),
                    kind: match v.kind {
                        mtx_core::mandate::Kind::Perp => w::MarketKind::Perp,
                        mtx_core::mandate::Kind::Spot => w::MarketKind::Spot,
                    },
                    tradable: v.tradable,
                })
                .collect(),
            applies_tx_options: s.applies_tx_options,
        }
    }

    fn side(side: Side) -> w::Side {
        match side {
            Side::Bid => w::Side::Bid,
            Side::Ask => w::Side::Ask,
        }
    }

    fn kit_side(side: w::Side) -> Side {
        match side {
            w::Side::Bid => Side::Bid,
            w::Side::Ask => Side::Ask,
        }
    }

    fn position(p: mtx_kit::Position) -> w::Position {
        w::Position {
            size: p.size,
            entry_usd: p.entry_usd,
        }
    }

    fn role(role: mtx_kit::Role) -> w::Role {
        match role {
            mtx_kit::Role::Taker => w::Role::Taker,
            mtx_kit::Role::Maker => w::Role::Maker,
        }
    }

    fn levels(levels: &[mtx_core::turns::Level]) -> Vec<w::Level> {
        levels
            .iter()
            .map(|l| w::Level {
                price: l.price,
                size: l.base,
            })
            .collect()
    }

    fn order(action: &Action) -> w::Order {
        match action {
            &Action::Market {
                side: s,
                size,
                limit,
            } => w::Order::Market(w::MarketOrder {
                side: side(s),
                size,
                limit,
            }),
            &Action::Limit {
                side: s,
                price,
                size,
            } => w::Order::Limit(w::LimitOrder {
                side: side(s),
                price,
                size,
                time_in_force: w::TimeInForce::Gtc,
            }),
            &Action::PostOnly {
                side: s,
                price,
                size,
            } => w::Order::Limit(w::LimitOrder {
                side: side(s),
                price,
                size,
                time_in_force: w::TimeInForce::PostOnly,
            }),
            Action::CancelAll => w::Order::CancelAll,
            Action::Respond { id, price, size } => w::Order::Respond(w::RfqResponse {
                id: id.clone(),
                price: *price,
                size: *size,
            }),
        }
    }

    pub fn market(m: &Market) -> w::Market {
        w::Market {
            k: m.k,
            slot: m.slot,
            last: m.last,
            steps_left: m.steps_left,
            slots_left: m.slots_left,
            mark: m.mark,
            oracle: m.oracle,
            index: m.index,
            best_bid: m.best_bid,
            best_ask: m.best_ask,
            position: position(m.position),
            cap: m.cap,
            max_drawdown: m.max_drawdown,
            resting_bids: m.resting_bids,
            resting_asks: m.resting_asks,
            pnl: m.pnl,
            peak_pnl: m.peak_pnl,
            drawdown: m.drawdown,
            unix_timestamp: m.unix_timestamp,
            venues: m
                .venues
                .iter()
                .map(|v| w::VenueView {
                    id: v.id.clone(),
                    best_bid: v.best_bid,
                    best_ask: v.best_ask,
                    depth: v.depth.as_ref().map(|book| w::Book {
                        bids: levels(&book.bids),
                        asks: levels(&book.asks),
                    }),
                    tape: v.tape.as_ref().map(|tape| {
                        tape.iter()
                            .map(|t| w::Trade {
                                slot: t.slot,
                                side: match t.side {
                                    mtx_core::Side::Buy => w::Side::Bid,
                                    mtx_core::Side::Sell => w::Side::Ask,
                                },
                                price: t.price,
                                size: t.base,
                                taker: t.taker.clone(),
                                maker: t.maker.clone(),
                                role: t.own.map(role),
                            })
                            .collect()
                    }),
                    position: v.position.map(position),
                })
                .collect(),
            sends: m
                .sends
                .iter()
                .map(|s| w::SendStatus {
                    step: s.step,
                    line: s.line,
                    venue: s.venue.clone(),
                    order: order(&s.order),
                    state: match &s.state {
                        SendState::Landed(signature) => w::SendState::Landed(signature.clone()),
                        SendState::Failed(why) => w::SendState::Failed(why.clone()),
                        SendState::NotSent(why) => w::SendState::NotSent(why.clone()),
                        SendState::Delayed => w::SendState::Delayed,
                        SendState::Expired => w::SendState::Expired,
                    },
                })
                .collect(),
            fills: m
                .fills
                .iter()
                .map(|f| w::Fill {
                    venue: f.venue.clone(),
                    slot: f.slot,
                    side: side(f.side),
                    price: f.price,
                    size: f.size,
                    role: role(f.role),
                })
                .collect(),
            rfqs: m
                .rfqs
                .iter()
                .map(|r| w::Rfq {
                    id: r.id.clone(),
                    venue: r.venue.clone(),
                    side: side(r.side),
                    size: r.size,
                    deadline_slot: r.deadline_slot,
                })
                .collect(),
            notes: m.notes.clone(),
        }
    }

    /// A line of the sheet, as the kit plans it.
    pub fn line(intent: w::Intent) -> (Intent, LineOptions) {
        let mut options = LineOptions {
            venue: intent.venue,
            reduce_only: intent.reduce_only,
            tx: intent.tx.map(|tx| TxOptions {
                cu_limit: tx.cu_limit,
                cu_price: tx.cu_price,
                tip: tx.tip,
                bundle: tx.bundle,
            }),
            ..LineOptions::default()
        };
        let intent = match intent.action {
            w::Action::Target(t) => {
                options.participation = t.execution.participation;
                Intent::Target {
                    position: t.position,
                    execution: mtx_kit::Execution {
                        max_per_step: t.execution.max_per_step,
                        slice: t.execution.slice,
                        slippage_bps: t.execution.slippage_bps,
                        min_trade: t.execution.min_trade,
                    },
                }
            }
            w::Action::Quote(q) => {
                if q.post_only {
                    options.time_in_force = TimeInForce::PostOnly;
                }
                Intent::Quote {
                    bid: q.bid,
                    ask: q.ask,
                    size: q.size,
                }
            }
            w::Action::Market(o) => Intent::Market {
                side: kit_side(o.side),
                size: o.size,
                limit: o.limit,
            },
            w::Action::Limit(o) => {
                options.time_in_force = match o.time_in_force {
                    w::TimeInForce::Gtc => TimeInForce::Gtc,
                    w::TimeInForce::PostOnly => TimeInForce::PostOnly,
                    w::TimeInForce::Ioc => TimeInForce::Ioc,
                };
                Intent::Limit {
                    side: kit_side(o.side),
                    price: o.price,
                    size: o.size,
                }
            }
            w::Action::CancelAll => Intent::CancelAll,
            w::Action::Respond(r) => Intent::Respond {
                id: r.id,
                price: r.price,
                size: r.size,
            },
        };
        (intent, options)
    }
}

/// `mtx:strategy@0.1.0`'s side of the kit's types: 0.2.0 without venues,
/// order options, sends, fills or requests for quotes.
mod wit1 {
    use mtx_kit::{Intent, Market, Session, Side};

    use super::v1::mtx::strategy::types as w;

    pub fn session(s: &Session, mandate: String) -> w::Session {
        w::Session {
            steps: s.steps,
            slots_per_step: s.slots_per_step,
            start_slot: s.start_slot,
            end_slot: s.end_slot,
            symbol: s.symbol.clone(),
            start_mark: s.start_mark,
            start_oracle: s.start_oracle,
            cap: s.cap,
            max_drawdown: s.max_drawdown,
            mandate,
            venue: s.venue.clone(),
            authority: s.authority.clone(),
            vars: s.vars.clone(),
            seed: s.seed,
        }
    }

    pub fn market(m: &Market) -> w::Market {
        w::Market {
            k: m.k,
            slot: m.slot,
            last: m.last,
            steps_left: m.steps_left,
            slots_left: m.slots_left,
            mark: m.mark,
            oracle: m.oracle,
            index: m.index,
            best_bid: m.best_bid,
            best_ask: m.best_ask,
            position: w::Position {
                size: m.position.size,
                entry_usd: m.position.entry_usd,
            },
            cap: m.cap,
            max_drawdown: m.max_drawdown,
            resting_bids: m.resting_bids,
            resting_asks: m.resting_asks,
            pnl: m.pnl,
            peak_pnl: m.peak_pnl,
            drawdown: m.drawdown,
        }
    }

    fn side(side: w::Side) -> Side {
        match side {
            w::Side::Bid => Side::Bid,
            w::Side::Ask => Side::Ask,
        }
    }

    pub fn intent(intent: w::Intent) -> Intent {
        match intent {
            w::Intent::Target((position, e)) => Intent::Target {
                position,
                execution: mtx_kit::Execution {
                    max_per_step: e.max_per_step,
                    slice: e.slice,
                    slippage_bps: e.slippage_bps,
                    min_trade: e.min_trade,
                },
            },
            w::Intent::Quote((bid, ask, size)) => Intent::Quote { bid, ask, size },
            w::Intent::Market((s, size, limit)) => Intent::Market {
                side: side(s),
                size,
                limit,
            },
            w::Intent::Limit((s, price, size)) => Intent::Limit {
                side: side(s),
                price,
                size,
            },
            w::Intent::CancelAll => Intent::CancelAll,
        }
    }
}

/// A scenario's seed for the strategy: the same in every run of it (FNV-1a
/// of its name).
fn seed_of(scenario: &str) -> u64 {
    scenario.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use mtx_core::turns::{BookLevels, Level, Role, Trade};
    use mtx_kit::{Market, Orders, Position, Session, Side, TxOptions, VenueInfo};

    use super::*;

    fn market(k: u64, mark: f64, size: f64) -> Market {
        Market {
            k,
            slot: 100 + 2 * k,
            steps_left: 100 - k,
            slots_left: 200 - 2 * k,
            mark,
            oracle: mark,
            index: mark,
            position: Position {
                size,
                entry_usd: 100.0,
            },
            cap: 70.0,
            max_drawdown: 1_000.0,
            ..Market::default()
        }
    }

    fn session(mandate: mtx_core::Mandate) -> Session {
        Session {
            steps: 100,
            slots_per_step: 2,
            start_slot: 100,
            end_slot: 300,
            symbol: "SPCX".into(),
            start_mark: 100.0,
            start_oracle: 100.0,
            cap: 70.0,
            max_drawdown: 1_000.0,
            venues: VenueInfo::of_perp_session(&mandate, "SPCX"),
            mandate,
            venue: "{}".into(),
            authority: String::new(),
            vars: Vec::new(),
            seed: 7,
            applies_tx_options: false,
        }
    }

    /// A log both the strategy and the test hold.
    #[derive(Clone, Default)]
    struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    async fn load(path: &Path, log: &Shared) -> WasmStrategy {
        WasmStrategy::load(
            path,
            "http://127.0.0.1:1",
            Box::new(log.clone()),
            7,
            100 * FUEL_PER_CALL,
        )
        .await
        .unwrap()
    }

    /// The reference as a component: set `MTX_TEST_WASM` to `bots/reference`'s
    /// component build (`just build`), and `MTX_TEST_WASM_V1` to one built
    /// on `mtx:strategy@0.1.0` to check that it plays the same.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a built component"]
    async fn the_reference_component_rides_a_move() {
        let builds = ["MTX_TEST_WASM", "MTX_TEST_WASM_V1"]
            .into_iter()
            .filter_map(|var| Some((var, PathBuf::from(std::env::var(var).ok()?))));
        for (var, path) in builds {
            let log = Shared::default();
            let mut strategy = load(&path, &log).await;
            let interface = if var == "MTX_TEST_WASM_V1" {
                "0.1.0"
            } else {
                "0.2.0"
            };
            assert_eq!(strategy.interface(), interface, "{var}");
            let mandate = mtx_core::Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0);
            assert!(matches!(
                strategy.start(&session(mandate)).await,
                Outcome::Done(())
            ));
            // Calm: no orders.
            let Outcome::Done(calm) = strategy.step(&market(0, 100.0, 0.0)).await else {
                panic!()
            };
            assert!(calm.is_empty());
            // A 3% jump starts a move: ride it, up to 35 a step in slices of 10.
            let Outcome::Done(ride) = strategy.step(&market(1, 103.0, 0.0)).await else {
                panic!()
            };
            let bought: f64 = ride
                .iter()
                .map(|(intent, options)| {
                    assert_eq!(*options, LineOptions::default());
                    match intent {
                        Intent::Market {
                            side: Side::Bid,
                            size,
                            limit,
                        } => {
                            assert_eq!(*limit, 105.06);
                            *size
                        }
                        other => panic!("{other:?}"),
                    }
                })
                .sum();
            assert_eq!(bought, 35.0);
            // A drawdown past its halt: it logs and closes.
            let Outcome::Done(_) = strategy.step(&market(2, 80.0, 35.0)).await else {
                panic!()
            };
            assert!(log.text().contains("halt"));
            assert!(matches!(strategy.end(None).await, Outcome::Done(())));
        }
    }

    /// The market a 0.2.0 component is handed at a step with turn data: two
    /// venues, a tape with one of the strategy's fills, depth, a clock, and
    /// what became of the last sheet.
    fn rich_market(venues: &[VenueInfo]) -> Market {
        let tape = vec![
            Trade {
                venue: "phoenix".into(),
                side: mtx_core::Side::Sell,
                price: 99.95,
                base: 4.0,
                slot: 101,
                taker: Some("taker".into()),
                maker: Some("me".into()),
                own: Some(Role::Maker),
            },
            Trade {
                venue: "phoenix".into(),
                side: mtx_core::Side::Buy,
                price: 100.05,
                base: 1.5,
                slot: 102,
                taker: None,
                maker: None,
                own: None,
            },
        ];
        let depth = vec![
            BookLevels {
                venue: "phoenix".into(),
                bids: vec![Level {
                    price: 99.9,
                    base: 12.0,
                }],
                asks: vec![Level {
                    price: 100.1,
                    base: 8.0,
                }],
            },
            BookLevels {
                venue: "openbook".into(),
                bids: vec![Level {
                    price: 150.25,
                    base: 3.0,
                }],
                asks: vec![],
            },
        ];
        let position = Position {
            size: 4.0,
            entry_usd: 99.95,
        };
        let (views, fills) =
            mtx_kit::venue_views(venues, Some(99.9), Some(100.1), position, &tape, &depth);
        Market {
            best_bid: Some(99.9),
            best_ask: Some(100.1),
            resting_bids: 6.0,
            unix_timestamp: Some(1_767_225_600),
            tape,
            depth,
            venues: views,
            fills,
            sends: vec![
                SendStatus {
                    step: 0,
                    line: 2,
                    venue: venues[0].id.clone(),
                    order: Action::PostOnly {
                        side: Side::Bid,
                        price: 99.5,
                        size: 10.0,
                    },
                    state: SendState::Landed("sig".into()),
                },
                SendStatus {
                    step: 0,
                    line: 3,
                    venue: venues[1].id.clone(),
                    order: Action::Respond {
                        id: "rfq-0".into(),
                        price: 150.0,
                        size: Some(2.0),
                    },
                    state: SendState::NotSent("no requests for quotes".into()),
                },
            ],
            rfqs: vec![mtx_kit::Rfq {
                id: "rfq-1".into(),
                venue: venues[0].id.clone(),
                side: Side::Ask,
                size: 5.0,
                deadline_slot: 110,
            }],
            notes: vec!["bid of 20 cut to 10, to stay within the cap".into()],
            ..market(1, 100.0, 4.0)
        }
    }

    /// The probe (`bots/wit-probe`, `just build`) as `MTX_TEST_PROBE_WASM`:
    /// the session and the market it logs are what the grader handed it,
    /// and the sheet it returns is the one it wrote, every option included.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a built component"]
    async fn the_wit_probe_round_trips() {
        let path = PathBuf::from(std::env::var("MTX_TEST_PROBE_WASM").unwrap());
        let log = Shared::default();
        let mut strategy = load(&path, &log).await;
        assert_eq!(strategy.interface(), "0.2.0");
        let mut mandate = mtx_core::Mandate::perp("phoenix", "SPCX", 70.0, 1_000.0);
        mandate
            .universe
            .extend(mtx_core::Mandate::spot(&["openbook"], "SOL-USDC", 10.0).universe);
        let session = session(mandate);
        assert_eq!(session.venues.len(), 2);
        assert!(session.venues[0].tradable && !session.venues[1].tradable);
        assert!(matches!(strategy.start(&session).await, Outcome::Done(())));
        assert!(log.text().contains(&format!("session {session:?}")));

        let mut market = rich_market(&session.venues);
        market.k = 0;
        let Outcome::Done(lines) = strategy.step(&market).await else {
            panic!()
        };
        assert!(
            log.text().contains(&format!("market {market:?}")),
            "the market didn't round-trip:\n{}\n{market:?}",
            log.text()
        );
        let mut expected = Orders::new();
        expected
            .cancel_all()
            .tx(TxOptions {
                cu_limit: Some(400_000),
                cu_price: Some(1_000),
                tip: Some(10_000),
                bundle: Some(1),
            })
            .bid(102.0, 2.0)
            .ioc()
            .sell(5.0, 98.0)
            .reduce_only()
            .bid(94.9, 1.0)
            .post_only()
            .ask(99.9, 1.0)
            .post_only()
            .target_position(3.0)
            .participation(0.1)
            .buy(1.0, 102.0)
            .at("openbook:spot:SOL-USDC")
            .respond("rfq-1", 100.0);
        assert_eq!(lines, expected.clone().into_lines());

        // Planned as the grader sends it, from a position of 4: the sell
        // reduces 6 to 1, and the target may trade 10% of the 5.5 on the
        // tape, less than its minimum trade.
        let (planned, notes) = mtx_kit::plan_sheet(&expected, &market);
        let actions: Vec<_> = planned
            .iter()
            .map(|p| (p.line, p.venue.as_deref(), p.action.clone()))
            .collect();
        assert_eq!(
            actions,
            [
                (0, None, Action::CancelAll),
                (
                    1,
                    None,
                    Action::Market {
                        side: Side::Bid,
                        size: 2.0,
                        limit: 102.0
                    }
                ),
                (
                    2,
                    None,
                    Action::Market {
                        side: Side::Ask,
                        size: 5.0,
                        limit: 98.0
                    }
                ),
                (
                    3,
                    None,
                    Action::PostOnly {
                        side: Side::Bid,
                        price: 94.9,
                        size: 1.0
                    }
                ),
                (
                    4,
                    None,
                    Action::PostOnly {
                        side: Side::Ask,
                        price: 99.9,
                        size: 1.0
                    }
                ),
                (
                    6,
                    Some("openbook:spot:SOL-USDC"),
                    Action::Market {
                        side: Side::Bid,
                        size: 1.0,
                        limit: 102.0
                    }
                ),
                (
                    7,
                    None,
                    Action::Respond {
                        id: "rfq-1".into(),
                        price: 100.0,
                        size: None
                    }
                ),
            ]
        );
        assert_eq!(planned[0].tx.and_then(|tx| tx.tip), Some(10_000));
        assert_eq!(
            notes,
            ["participation 0.1 of the step's volume: trading 0.55 of 2"]
        );

        assert!(matches!(
            strategy.end(Some(&market)).await,
            Outcome::Done(())
        ));
        assert!(log.text().contains("end 0"));
    }
}
