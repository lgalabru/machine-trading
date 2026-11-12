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

use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use rand::SeedableRng;
use wasmtime::{
    Config, Engine, Store, StoreLimits, StoreLimitsBuilder,
    component::{Component, HasSelf, Linker, ResourceTable},
};
use wasmtime_wasi::{
    HostMonotonicClock, HostWallClock, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView,
};

use crate::Error;

pub mod spot;

wasmtime::component::bindgen!({
    world: "strategy",
    path: "../kit/wit",
    imports: { default: async },
    exports: { default: async },
});

pub use mtx::strategy::types::{Execution, Position, Side};

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

/// The most a strategy may print in one call, in bytes: past it, output is
/// dropped (and a note says so), so printing can't exhaust memory or fail.
pub const PRINTED_BYTES_PER_CALL: usize = 1 << 20;

/// What a strategy writes during a call, in order: its standard output and
/// error (`println!`, `eprintln!`) and `mtx_kit::log`'s lines. The host
/// drains it after each call into the session's log.
#[derive(Clone, Default)]
struct Printed(std::sync::Arc<std::sync::Mutex<PrintedBuffer>>);

#[derive(Default)]
struct PrintedBuffer {
    bytes: Vec<u8>,
    dropped: bool,
}

impl Printed {
    fn write(&self, bytes: &[u8]) {
        let mut buffer = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let room = PRINTED_BYTES_PER_CALL.saturating_sub(buffer.bytes.len());
        if bytes.len() > room {
            buffer.dropped = true;
        }
        buffer
            .bytes
            .extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    /// The lines written since the last call, and a note if some were dropped.
    fn take(&self) -> Vec<String> {
        let mut buffer = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let text = String::from_utf8_lossy(&buffer.bytes).into_owned();
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        if std::mem::take(&mut buffer.dropped) {
            lines.push(format!(
                "(output past {PRINTED_BYTES_PER_CALL} bytes in one call was dropped)"
            ));
        }
        buffer.bytes.clear();
        lines
    }
}

impl wasmtime_wasi::cli::IsTerminal for Printed {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl wasmtime_wasi::cli::StdoutStream for Printed {
    fn async_stream(&self) -> Box<dyn tokio::io::AsyncWrite + Send + Sync> {
        Box::new(self.clone())
    }
}

impl tokio::io::AsyncWrite for Printed {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        self.write(buf);
        std::task::Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

/// What the strategy's host side holds.
struct Host {
    wasi: WasiCtx,
    table: ResourceTable,
    log: Box<dyn Write + Send>,
    printed: Printed,
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

impl mtx::strategy::types::Host for Host {}

impl StrategyImports for Host {
    async fn log(&mut self, line: String) {
        self.printed.write(format!("{line}\n").as_bytes());
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

/// A strategy component, instantiated for one session.
pub struct WasmStrategy {
    store: Store<Host>,
    strategy: Strategy,
    /// The session's fuel left, what the call in progress was given, what
    /// the session has spent, and the most one call spent.
    fuel_left: u64,
    fuel_given: u64,
    pub fuel_used: u64,
    pub fuel_max_call: u64,
    last_call_fuel: u64,
    last_printed: Vec<String>,
}

impl WasmStrategy {
    /// Instantiates the component at `path`. `rpc_url` answers its `rpc`
    /// calls, its log lines go to `log`, `seed` seeds its randomness, and
    /// `session_fuel` is all it may spend.
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
        let mut linker: Linker<Host> = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(wasm_error)?;
        Strategy::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host).map_err(wasm_error)?;
        let printed = Printed::default();
        let wasi = WasiCtxBuilder::new()
            .stdout(printed.clone())
            .stderr(printed.clone())
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
            printed,
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
        let strategy = Strategy::instantiate_async(&mut store, &component, &linker)
            .await
            .map_err(wasm_error)?;
        let used = this_fuel - store.get_fuel().unwrap_or(0);
        this_fuel = session_fuel.saturating_sub(used);
        Ok(Self {
            store,
            strategy,
            fuel_left: this_fuel,
            fuel_given: 0,
            fuel_used: used,
            fuel_max_call: used,
            last_call_fuel: used,
            last_printed: Vec::new(),
        })
    }

    /// Fuel for the next call, from what the session has left: `false` if
    /// none is left.
    fn refuel(&mut self) -> bool {
        self.store.data_mut().rpc_calls = 0;
        self.fuel_given = self.fuel_left.min(FUEL_PER_CALL);
        self.fuel_given > 0 && self.store.set_fuel(self.fuel_given).is_ok()
    }

    /// What the last call spent, out of the session's fuel; and what it
    /// printed, into the session's log.
    fn charge(&mut self) {
        let used = self.fuel_given - self.store.get_fuel().unwrap_or(0).min(self.fuel_given);
        self.fuel_left -= used;
        self.fuel_used += used;
        self.fuel_max_call = self.fuel_max_call.max(used);
        self.last_call_fuel = used;
        let host = self.store.data_mut();
        let lines = host.printed.take();
        for line in &lines {
            let _ = writeln!(host.log, "{line}");
        }
        self.last_printed = lines;
    }

    /// What the last call printed, its fuel, and its `rpc` calls.
    pub fn last_call(&self) -> (&[String], u64, u32) {
        (
            &self.last_printed,
            self.last_call_fuel,
            self.store.data().rpc_calls,
        )
    }

    fn spent() -> String {
        "the session's fuel is spent".to_string()
    }

    pub async fn start(&mut self, session: &Session) -> Outcome<()> {
        if !self.refuel() {
            return Outcome::Trapped(Self::spent());
        }
        let result = self.strategy.call_start(&mut self.store, session).await;
        self.charge();
        match result {
            Ok(Ok(())) => Outcome::Done(()),
            Ok(Err(e)) => Outcome::Failed(e),
            Err(trap) => Outcome::Trapped(format!("{trap:#}")),
        }
    }

    pub async fn step(&mut self, market: &Market) -> Outcome<Vec<Intent>> {
        if !self.refuel() {
            return Outcome::Trapped(Self::spent());
        }
        let result = self.strategy.call_step(&mut self.store, *market).await;
        self.charge();
        match result {
            Ok(Ok(intents)) => Outcome::Done(intents),
            Ok(Err(e)) => Outcome::Failed(e),
            Err(trap) => Outcome::Trapped(format!("{trap:#}")),
        }
    }

    pub async fn end(&mut self, last: Option<&Market>) -> Outcome<()> {
        if !self.refuel() {
            return Outcome::Trapped(Self::spent());
        }
        let result = self.strategy.call_end(&mut self.store, last.copied()).await;
        self.charge();
        match result {
            Ok(()) => Outcome::Done(()),
            Err(trap) => Outcome::Trapped(format!("{trap:#}")),
        }
    }
}

/// The account at a step, as the session measures it (not the strategy's
/// estimate): taken before the strategy's turn, so the position change is
/// what filled during the previous step.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Account {
    pub value_usd: f64,
    pub pnl_usd: f64,
    /// Below the session's highest value so far.
    pub drawdown_usd: f64,
    pub position: f64,
    /// Since the step before: the net of what filled meanwhile.
    pub position_change: f64,
    /// The scenario's planned level at the step.
    pub level_usd: f64,
}

/// One step of a component's session, as `mtx bot run --debug` shows it:
/// what the strategy was handed, what it printed and asked for, and what the
/// grader sent.
#[derive(Debug, Clone)]
pub struct StepTrace {
    pub steps: u64,
    pub market: mtx_kit::Market,
    pub printed: Vec<String>,
    /// The strategy's error or trap, if `step` didn't return a sheet.
    pub failed: Option<String>,
    pub intents: Vec<mtx_kit::Intent>,
    /// Where `plan()` cut the sheet to the cap.
    pub notes: Vec<String>,
    /// Each order sent, and how it went; the grader stops at the first that
    /// fails.
    pub sent: Vec<(mtx_kit::Action, std::result::Result<(), String>)>,
    pub fuel: u64,
    pub rpc_calls: u32,
    /// The account as the session measured it at this step.
    pub account: Option<Account>,
}

/// Watches a component's session step by step: `mtx bot run --debug`. It
/// may block (to wait for a key): the world waits for the strategy.
pub trait StepObserver: Send + Sync {
    /// After `start`: the session, and what `start` printed or failed with.
    fn started(&self, session: &mtx_kit::Session, printed: &[String], failed: Option<&str>);
    /// After each step's orders are sent.
    fn step(&self, trace: &StepTrace);
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
    session: Session,
    pnl: f64,
    peak_pnl: f64,
    last_mark: Option<f64>,
    resting: (f64, f64),
    last: Option<Market>,
    /// Why the strategy plays no more of the session, if it doesn't: it
    /// didn't load, or it trapped (Wasmtime won't enter its instance again).
    pub stopped: Option<String>,
    observer: Option<std::sync::Arc<dyn StepObserver>>,
    /// The account the session measured for the coming turn.
    account: Option<Account>,
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
        observer: Option<std::sync::Arc<dyn StepObserver>>,
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
        let seed = mtx_core::turns::scenario_seed(scenario);
        let session_fuel = (steps + 2).saturating_mul(FUEL_PER_STEP);
        let mut stopped = None;
        let mut strategy = match WasmStrategy::load(path, &env.rpc_url, log, seed, session_fuel)
            .await
        {
            Ok(strategy) => Some(strategy),
            Err(error) => {
                tracing::warn!(%error, "the component didn't load; the session plays without it");
                stopped = Some(format!("didn't load: {error}"));
                None
            }
        };
        let session = Session {
            steps,
            slots_per_step,
            start_slot,
            end_slot: env.end_slot,
            symbol: env.symbol.clone(),
            start_mark,
            start_oracle,
            cap: env.max_position,
            max_drawdown: env.max_drawdown_usd,
            mandate: serde_json::to_string(mandate)?,
            venue: std::fs::read_to_string(&env.exchange_path)
                .map_err(crate::error::io_error(&env.exchange_path))?,
            authority: trader.authority().to_string(),
            vars,
            seed,
        };
        let observed = observer.as_ref().map(|_| mtx_kit::Session {
            steps: session.steps,
            slots_per_step: session.slots_per_step,
            start_slot: session.start_slot,
            end_slot: session.end_slot,
            symbol: session.symbol.clone(),
            start_mark: session.start_mark,
            start_oracle: session.start_oracle,
            cap: session.cap,
            max_drawdown: session.max_drawdown,
            mandate: mandate.clone(),
            venue: session.venue.clone(),
            authority: session.authority.clone(),
            vars: session.vars.clone(),
            seed: session.seed,
        });
        if let Some(strategy) = strategy.as_mut() {
            let failed = match strategy.start(&session).await {
                Outcome::Done(()) => None,
                Outcome::Failed(e) => {
                    tracing::warn!(error = %e, "the strategy's start failed; it plays on");
                    Some(e)
                }
                Outcome::Trapped(e) => {
                    tracing::warn!(error = %e, "the strategy's start trapped; it plays no more of the session");
                    stopped = Some(format!("trapped in start: {e}"));
                    Some(format!("trapped: {e}"))
                }
            };
            if let (Some(observer), Some(observed)) = (&observer, &observed) {
                let printed = strategy.last_call().0.to_vec();
                tokio::task::block_in_place(|| {
                    observer.started(observed, &printed, failed.as_deref())
                });
            }
        } else if let (Some(observer), Some(observed), Some(reason)) =
            (&observer, &observed, &stopped)
        {
            tokio::task::block_in_place(|| observer.started(observed, &[], Some(reason)));
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
            stopped,
            observer,
            account: None,
        })
    }

    /// The strategy's turn at step `k`: read the market, call `step`, cut
    /// the sheet to the mandate's cap and send it, in order, stopping at the
    /// first order that fails. Returns whether the strategy answered.
    pub async fn turn(&mut self, k: u64, slot: u64, last: bool) -> bool {
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
        let kit = mtx_kit::Market {
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
            ..mtx_kit::Market::default()
        };
        let market = Market {
            k,
            slot,
            last,
            steps_left: kit.steps_left,
            slots_left: kit.slots_left,
            mark,
            oracle,
            index: top.index,
            best_bid: top.best_bid,
            best_ask: top.best_ask,
            position: Position {
                size: position.size,
                entry_usd: position.entry_usd,
            },
            cap: kit.cap,
            max_drawdown: kit.max_drawdown,
            resting_bids: kit.resting_bids,
            resting_asks: kit.resting_asks,
            pnl: kit.pnl,
            peak_pnl: kit.peak_pnl,
            drawdown: kit.drawdown,
        };
        self.last = Some(market);
        let Some(strategy) = self.strategy.as_mut() else {
            return false;
        };
        let outcome = strategy.step(&market).await;
        let mut trace = self.observer.as_ref().map(|_| {
            let (printed, fuel, rpc_calls) = strategy.last_call();
            StepTrace {
                steps: self.session.steps,
                market: kit.clone(),
                printed: printed.to_vec(),
                failed: None,
                intents: Vec::new(),
                notes: Vec::new(),
                sent: Vec::new(),
                fuel,
                rpc_calls,
                account: self.account,
            }
        });
        let intents = match outcome {
            Outcome::Done(intents) => intents,
            Outcome::Failed(e) => {
                tracing::warn!(step = k, error = %e, "the strategy failed; no orders this step");
                self.observe(trace.map(|t| StepTrace {
                    failed: Some(e),
                    ..t
                }));
                return true;
            }
            Outcome::Trapped(e) => {
                tracing::warn!(step = k, error = %e, "the strategy trapped; it plays no more of the session");
                self.stopped = Some(format!("trapped at step {k}: {e}"));
                self.observe(trace.map(|t| StepTrace {
                    failed: Some(format!("trapped: {e}; it plays no more of the session")),
                    ..t
                }));
                return false;
            }
        };
        let intents: Vec<mtx_kit::Intent> = intents.into_iter().map(kit_intent).collect();
        let (actions, notes) = mtx_kit::plan(&intents, &kit);
        for note in &notes {
            tracing::warn!(step = k, "{note}");
        }
        if let Some(trace) = trace.as_mut() {
            trace.intents = intents.clone();
            trace.notes = notes;
        }
        let mut sent = 0;
        for action in &actions {
            let result = match *action {
                mtx_kit::Action::Market { side, size, limit } => self
                    .trader
                    .place_market(side, size, limit)
                    .await
                    .map(|_| ()),
                mtx_kit::Action::Limit { side, price, size } => {
                    self.trader.place_limit(side, price, size).await.map(|_| ())
                }
                mtx_kit::Action::CancelAll => self.trader.cancel_all().map(|_| ()),
            };
            if let Some(trace) = trace.as_mut() {
                trace.sent.push((
                    action.clone(),
                    result.as_ref().map(|_| ()).map_err(|e| e.to_string()),
                ));
            }
            if let Err(error) = result {
                tracing::warn!(step = k, ?action, %error, "order failed; the step's other orders aren't sent");
                break;
            }
            sent += 1;
        }
        // A failed resting order counts anyway: the ledger only over-counts.
        self.resting = mtx_kit::resting_after(
            self.resting.0,
            self.resting.1,
            &actions[..(sent + 1).min(actions.len())],
        );
        self.observe(trace);
        true
    }

    /// The account as the session measured it, for the next turn's trace.
    pub fn measured(&mut self, account: Account) {
        self.account = Some(account);
    }

    /// Hand a step's trace to the observer, if there is one.
    fn observe(&self, trace: Option<StepTrace>) {
        if let (Some(observer), Some(trace)) = (&self.observer, trace) {
            tokio::task::block_in_place(|| observer.step(&trace));
        }
    }

    /// The session is over: the strategy's `end`.
    pub async fn end(&mut self) {
        let last = self.last;
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

fn kit_side(side: Side) -> mtx_kit::Side {
    match side {
        Side::Bid => mtx_kit::Side::Bid,
        Side::Ask => mtx_kit::Side::Ask,
    }
}

fn kit_intent(intent: Intent) -> mtx_kit::Intent {
    match intent {
        Intent::Target((position, e)) => mtx_kit::Intent::Target {
            position,
            execution: mtx_kit::Execution {
                max_per_step: e.max_per_step,
                slice: e.slice,
                slippage_bps: e.slippage_bps,
                min_trade: e.min_trade,
            },
        },
        Intent::Quote((bid, ask, size)) => mtx_kit::Intent::Quote { bid, ask, size },
        Intent::Market((side, size, limit)) => mtx_kit::Intent::Market {
            side: kit_side(side),
            size,
            limit,
        },
        Intent::Limit((side, price, size)) => mtx_kit::Intent::Limit {
            side: kit_side(side),
            price,
            size,
        },
        Intent::CancelAll => mtx_kit::Intent::CancelAll,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn market(k: u64, mark: f64, size: f64) -> Market {
        Market {
            k,
            slot: 100 + 2 * k,
            last: false,
            steps_left: 100 - k,
            slots_left: 200 - 2 * k,
            mark,
            oracle: mark,
            index: mark,
            best_bid: None,
            best_ask: None,
            position: Position {
                size,
                entry_usd: 100.0,
            },
            cap: 70.0,
            max_drawdown: 1_000.0,
            resting_bids: 0.0,
            resting_asks: 0.0,
            pnl: 0.0,
            peak_pnl: 0.0,
            drawdown: 0.0,
        }
    }

    /// The reference as a component: set `MTX_TEST_WASM` to
    /// `bots/reference`'s component build (`just build`).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a built component"]
    async fn the_reference_component_rides_a_move() {
        let path = PathBuf::from(std::env::var("MTX_TEST_WASM").unwrap());
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
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
        let mut strategy = WasmStrategy::load(
            &path,
            "http://127.0.0.1:1",
            Box::new(Shared(log.clone())),
            7,
            100 * FUEL_PER_CALL,
        )
        .await
        .unwrap();
        let session = Session {
            steps: 100,
            slots_per_step: 2,
            start_slot: 100,
            end_slot: 300,
            symbol: "SPCX".into(),
            start_mark: 100.0,
            start_oracle: 100.0,
            cap: 70.0,
            max_drawdown: 1_000.0,
            mandate: serde_json::to_string(&mtx_core::Mandate::perp(
                "phoenix", "SPCX", 70.0, 1_000.0,
            ))
            .unwrap(),
            venue: "{}".into(),
            authority: String::new(),
            vars: Vec::new(),
            seed: 7,
        };
        assert!(matches!(strategy.start(&session).await, Outcome::Done(())));
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
            .map(|i| match i {
                Intent::Market((Side::Bid, size, limit)) => {
                    assert_eq!(*limit, 105.06);
                    *size
                }
                other => panic!("{other:?}"),
            })
            .sum();
        assert_eq!(bought, 35.0);
        // A drawdown past its halt: it logs and closes.
        let Outcome::Done(_) = strategy.step(&market(2, 80.0, 35.0)).await else {
            panic!()
        };
        assert!(
            String::from_utf8(log.lock().unwrap().clone())
                .unwrap()
                .contains("halt")
        );
        assert!(matches!(strategy.end(None).await, Outcome::Done(())));
    }
}
