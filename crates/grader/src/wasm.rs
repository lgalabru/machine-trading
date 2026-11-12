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

use mtx_kit::{Intent, LineOptions, Planned, SendSource, SendState, SendStatus};
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

/// The most `rpc` calls one step may make, and the most `send` calls.
const RPC_CALLS_PER_STEP: u32 = 256;

/// How many of a step's raw transactions the host keeps to show.
const RAW_SENDS_KEPT: usize = 4;

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
    /// The step's `send` calls, and the first few transactions they asked
    /// for, as the kit has them: what a test or the debug view shows.
    raw_calls: u32,
    raw_sends: Vec<mtx_kit::RawTransaction>,
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

impl v2::mtx::strategy::types::Host for Host {}

impl v2::StrategyImports for Host {
    async fn log(&mut self, line: String) {
        Host::log(self, line)
    }

    async fn rpc(&mut self, method: String, params: String) -> std::result::Result<String, String> {
        Host::rpc(self, method, params).await
    }

    /// Reserved for raw sends (the grader-signed `send`): no session enables
    /// them yet. A malformed transaction is refused for what is wrong with
    /// it; any other, because raw sends are off.
    async fn send(
        &mut self,
        tx: v2::mtx::strategy::types::RawTransaction,
    ) -> std::result::Result<v2::mtx::strategy::types::RawReceipt, String> {
        self.raw_calls += 1;
        if self.raw_calls > RPC_CALLS_PER_STEP {
            return Err(format!("more than {RPC_CALLS_PER_STEP} sends this step"));
        }
        let tx = wit2::raw_transaction(tx);
        let checked = tx.check();
        if self.raw_sends.len() < RAW_SENDS_KEPT {
            self.raw_sends.push(tx);
        }
        checked?;
        Err("raw sends aren't enabled in this session".into())
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
    last_call_fuel: u64,
    last_printed: Vec<String>,
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
            raw_calls: 0,
            raw_sends: Vec::new(),
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
            last_call_fuel: used,
            last_printed: Vec::new(),
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
        let host = self.store.data_mut();
        host.rpc_calls = 0;
        host.raw_calls = 0;
        host.raw_sends.clear();
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

    /// The raw transactions the last call asked to send, the first few:
    /// refused or not.
    pub fn raw_sends(&self) -> &[mtx_kit::RawTransaction] {
        &self.store.data().raw_sends
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
    /// Each line's options, beside `intents`.
    pub options: Vec<mtx_kit::LineOptions>,
    /// Where `plan_sheet()` cut the sheet to the cap.
    pub notes: Vec<String>,
    /// Each order planned, and what became of it; the grader stops at the
    /// first that fails.
    pub sent: Vec<(mtx_kit::Action, SendState)>,
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
    /// What the session's steps report beyond prices.
    pub reports: mtx_kit::Reports,
}

/// The mandate a component's session runs under, as its `start` sees it.
pub struct Terms<'a> {
    pub end_slot: u64,
    pub mandate: &'a mtx_core::Mandate,
    /// The position cap on its market, and its drawdown limit.
    pub cap: f64,
    pub max_drawdown: f64,
}

/// What the grader trades and reads for a component: a seat on each market
/// the session trades, with the bot's key, through the bot's guarded
/// endpoint, and how its market is read.
pub struct Desk {
    /// The session's markets, the bot's own first, as `start` lists them.
    pub venues: Vec<mtx_kit::VenueInfo>,
    /// A seat on each market the session trades. A line for a market
    /// without one comes back not sent.
    pub seats: Vec<Box<dyn crate::venue::Seat>>,
    /// The market traded, as the venue names it, and the venue's metadata,
    /// as `start` gets them.
    pub symbol: String,
    pub venue: String,
    /// The bot's key, base58.
    pub authority: String,
    pub reader: Reader,
}

/// How a component's market is read at each turn.
pub enum Reader {
    /// A Phoenix perp account, as a process bot's run loop reads it: the
    /// venue's mark, oracle and index, the position there, and a PnL
    /// estimated from the mark's moves.
    Perp(std::sync::Arc<mtx_kit::Trader>),
    /// A wallet the session measures (a spot session): its reading at each
    /// turn ([`Wallet`]), valued at the oracle, and each market's top as
    /// its seat reads it.
    Wallet {
        /// The account when the session starts.
        start: Wallet,
    },
}

/// A wallet as the session measures it, at the session's oracle price.
pub use crate::spot::Wallet;

/// A component playing one session: the strategy, and the markets the
/// grader trades for it ([`Desk`]). Reads and sends as `mtx_kit::run` does
/// for a process bot.
pub struct WasmBot {
    /// `None` if the component didn't load: the session plays without it.
    strategy: Option<WasmStrategy>,
    desk: Desk,
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
    observer: Option<std::sync::Arc<dyn StepObserver>>,
    /// The account the session measured for the coming turn.
    account: Option<Account>,
}

impl WasmBot {
    /// Loads the component and calls its `start`. Its own `rpc` reads go to
    /// `rpc_url`, the bot's guarded endpoint.
    pub async fn start(
        path: &Path,
        rpc_url: &str,
        clock: Clock<'_>,
        terms: Terms<'_>,
        desk: Desk,
        log: Box<dyn Write + Send>,
        observer: Option<std::sync::Arc<dyn StepObserver>>,
    ) -> crate::Result<Self> {
        let Clock {
            steps,
            slots_per_step,
            start_slot,
            scenario,
            vars,
            reports,
        } = clock;
        let (start_mark, start_oracle) = match &desk.reader {
            Reader::Perp(trader) => (trader.mark_usd().await?, trader.oracle_usd().await?),
            Reader::Wallet { start } => (start.price_usd, start.price_usd),
        };
        let seed = mtx_core::turns::scenario_seed(scenario);
        let session_fuel = (steps + 2).saturating_mul(FUEL_PER_STEP);
        let mut stopped = None;
        let mut strategy = match WasmStrategy::load(path, rpc_url, log, seed, session_fuel).await {
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
            end_slot: terms.end_slot,
            symbol: desk.symbol.clone(),
            start_mark,
            start_oracle,
            cap: terms.cap,
            max_drawdown: terms.max_drawdown,
            mandate: terms.mandate.clone(),
            authority: desk.authority.clone(),
            vars,
            seed,
            venues: desk.venues.clone(),
            venue: desk.venue.clone(),
            reports,
            applies_tx_options: false,
        };
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
            if let Some(observer) = &observer {
                let printed = strategy.last_call().0.to_vec();
                tokio::task::block_in_place(|| {
                    observer.started(&session, &printed, failed.as_deref())
                });
            }
        } else if let (Some(observer), Some(reason)) = (&observer, &stopped) {
            tokio::task::block_in_place(|| observer.started(&session, &[], Some(reason)));
        }
        Ok(Self {
            strategy,
            desk,
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
            observer,
            account: None,
        })
    }

    /// The market at `step`, as the desk reads it: `None` if it couldn't.
    /// `wallet` is the session's reading of a measured wallet.
    async fn market(
        &mut self,
        step: &mtx_core::Step,
        wallet: Option<&Wallet>,
    ) -> Option<mtx_kit::Market> {
        let (k, slot, last) = (step.k, step.slot, step.last);
        let (mark, oracle, index, best_bid, best_ask, position) = match &self.desk.reader {
            Reader::Perp(trader) => {
                let (top, oracle, position) = match (
                    trader.top().await,
                    trader.oracle_usd().await,
                    trader.position().await,
                ) {
                    (Ok(top), Ok(oracle), Ok(position)) => (top, oracle, position),
                    (top, oracle, position) => {
                        tracing::warn!(step = k, mark = ?top.err(), oracle = ?oracle.err(), position = ?position.err(), "couldn't read the market; skipping the step");
                        return None;
                    }
                };
                let mark = top.mark;
                if let Some(previous) = self.last_mark {
                    self.pnl += position.size * (mark - previous);
                }
                self.last_mark = Some(mark);
                (
                    mark,
                    oracle,
                    top.index,
                    top.best_bid,
                    top.best_ask,
                    position,
                )
            }
            Reader::Wallet { start } => {
                let Some(wallet) = wallet else {
                    tracing::warn!(step = k, "no reading of the wallet; skipping the step");
                    return None;
                };
                let Some(Ok((best_bid, best_ask))) = (match self.desk.seats.first() {
                    Some(seat) => Some(seat.top().await),
                    None => None,
                }) else {
                    tracing::warn!(step = k, "couldn't read the book; skipping the step");
                    return None;
                };
                self.pnl = wallet.value_usd - start.value_usd;
                let position = mtx_kit::Position {
                    size: wallet.base,
                    entry_usd: start.price_usd,
                };
                let price = wallet.price_usd;
                (price, price, price, best_bid, best_ask, position)
            }
        };
        self.peak_pnl = self.peak_pnl.max(self.pnl);
        let (mut venues, fills) = mtx_kit::venue_views(
            &self.session.venues,
            self.session.reports,
            best_bid,
            best_ask,
            position,
            &step.tape,
            &step.depth,
        );
        // A measured wallet's other markets: each seat's own top, where the
        // step's depth didn't give one.
        if matches!(self.desk.reader, Reader::Wallet { .. }) {
            for view in venues.iter_mut().skip(1) {
                if view.best_bid.is_some() || view.best_ask.is_some() {
                    continue;
                }
                if let Some(seat) = self.desk.seats.iter().find(|s| s.market() == view.id)
                    && let Ok((bid, ask)) = seat.top().await
                {
                    (view.best_bid, view.best_ask) = (bid, ask);
                }
            }
        }
        Some(mtx_kit::Market {
            k,
            slot,
            last,
            steps_left: self.session.steps.saturating_sub(k + 1),
            slots_left: self.session.end_slot.saturating_sub(slot),
            mark,
            oracle,
            index,
            best_bid,
            best_ask,
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
            fees: None,
        })
    }

    /// The seat for a planned line's venue: the session's own market's for
    /// none.
    fn seat(&self, venue: Option<&str>) -> Option<&dyn crate::venue::Seat> {
        match venue {
            None => self.desk.seats.first(),
            Some(id) => self.desk.seats.iter().find(|s| s.market() == id),
        }
        .map(|seat| seat.as_ref())
    }

    /// The strategy's turn at `step`: read the market, call `step`, cut the
    /// sheet to the mandate's cap and send each line to its market, in
    /// order, stopping at the first order that fails. The step's turn data,
    /// if the scenario reports any, goes into the market, keyed by market;
    /// `wallet` is the session's reading of a measured wallet. Returns
    /// whether the strategy answered.
    pub async fn turn(&mut self, step: &mtx_core::Step, wallet: Option<&Wallet>) -> bool {
        let k = step.k;
        if self.stopped.is_some() {
            return false;
        }
        let Some(market) = self.market(step, wallet).await else {
            return false;
        };
        let Some(strategy) = self.strategy.as_mut() else {
            self.last = Some(market);
            return false;
        };
        let outcome = strategy.step(&market).await;
        let mut trace = self.observer.as_ref().map(|_| {
            let (printed, fuel, rpc_calls) = strategy.last_call();
            StepTrace {
                steps: self.session.steps,
                market: market.clone(),
                printed: printed.to_vec(),
                failed: None,
                intents: Vec::new(),
                options: Vec::new(),
                notes: Vec::new(),
                sent: Vec::new(),
                fuel,
                rpc_calls,
                account: self.account,
            }
        });
        let lines = match outcome {
            Outcome::Done(lines) => lines,
            Outcome::Failed(e) => {
                tracing::warn!(step = k, error = %e, "the strategy failed; no orders this step");
                self.last = Some(market);
                self.observe(trace.map(|t| StepTrace {
                    failed: Some(e),
                    ..t
                }));
                return true;
            }
            Outcome::Trapped(e) => {
                tracing::warn!(step = k, error = %e, "the strategy trapped; it plays no more of the session");
                self.stopped = Some(format!("trapped at step {k}: {e}"));
                self.last = Some(market);
                self.observe(trace.map(|t| StepTrace {
                    failed: Some(format!("trapped: {e}; it plays no more of the session")),
                    ..t
                }));
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
        // Only what the session executes counts toward the cap.
        let (planned, notes) = mtx_kit::plan_sheet_with(&orders, &market, |venue, action| {
            self.seat(venue).is_some_and(|seat| seat.executes(action))
        });
        for note in &notes {
            tracing::warn!(step = k, "{note}");
        }
        if let Some(trace) = trace.as_mut() {
            (trace.intents, trace.options) = orders.into_lines().into_iter().unzip();
            trace.notes = notes.clone();
        }
        self.notes = notes;
        self.last = Some(market);
        let own = self.session.venues[0].id.clone();
        // The orders that reached the chain, the one that failed included:
        // the ledger of what rests only over-counts.
        let mut tried = Vec::new();
        let mut failed = false;
        for planned in planned {
            let state = if failed {
                SendState::NotSent("an earlier order of the step failed".into())
            } else if let Some(seat) = self.seat(planned.venue.as_deref()) {
                let sent = seat.send(&planned).await;
                if sent.sent {
                    tried.push(planned.action.clone());
                    if let SendState::Failed { reason, .. } = &sent.state {
                        tracing::warn!(step = k, action = ?planned.action, error = %reason, "order failed; the step's other orders aren't sent");
                        failed = true;
                    }
                }
                sent.state
            } else {
                SendState::NotSent(format!(
                    "{} isn't traded in this session",
                    planned.venue.as_deref().unwrap_or(&own)
                ))
            };
            let Planned {
                line,
                venue,
                client_id,
                action,
                expires_slot,
                ..
            } = planned;
            if let Some(trace) = trace.as_mut() {
                trace.sent.push((action.clone(), state.clone()));
            }
            self.sends.push(SendStatus {
                step: k,
                source: SendSource::Line(line as u32),
                venue: venue.unwrap_or_else(|| own.clone()),
                client_id,
                order: Some(action),
                expires_slot,
                state,
                slot: None,
                fee: None,
                tip: None,
            });
        }
        self.resting = mtx_kit::resting_after(self.resting.0, self.resting.1, &tried);
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
        Action, AmountUnit, Intent, LineOptions, LiquidityShape, Market, MarketKind, PriceRange,
        Route, SendSource, SendState, Session, Side, TimeInForce, TxOptions, raw,
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
                        MarketKind::Perp => w::MarketKind::Perp,
                        MarketKind::Spot => w::MarketKind::Spot,
                        MarketKind::Amm => w::MarketKind::Amm,
                        MarketKind::Lending => w::MarketKind::Lending,
                        MarketKind::StakePool => w::MarketKind::StakePool,
                    },
                    address: v.address.clone(),
                    tradable: v.tradable,
                })
                .collect(),
            reports: w::Reports {
                tape: s.reports.tape,
                depth: s.reports.depth,
                timestamps: s.reports.timestamps,
                fees: s.reports.fees,
            },
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

    fn range(r: PriceRange) -> w::PriceRange {
        w::PriceRange {
            lower: r.lower,
            upper: r.upper,
        }
    }

    fn kit_range(r: w::PriceRange) -> PriceRange {
        PriceRange {
            lower: r.lower,
            upper: r.upper,
        }
    }

    fn unit(unit: AmountUnit) -> w::AmountUnit {
        match unit {
            AmountUnit::Base => w::AmountUnit::Base,
            AmountUnit::Quote => w::AmountUnit::Quote,
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

    fn shape(shape: LiquidityShape) -> w::LiquidityShape {
        match shape {
            LiquidityShape::Spot => w::LiquidityShape::Spot,
            LiquidityShape::Curve => w::LiquidityShape::Curve,
            LiquidityShape::BidAsk => w::LiquidityShape::BidAsk,
        }
    }

    /// An order as planned, with its line's expiry.
    fn order(action: &Action, expires_slot: Option<u64>) -> w::Order {
        let amount = |amount: &Option<f64>| w::Amount { amount: *amount };
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
                expires_slot,
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
                expires_slot,
            }),
            Action::CancelAll => w::Order::CancelAll,
            &Action::Cancel { client_id } => w::Order::Cancel(client_id),
            Action::Respond { id, bid, ask, size } => w::Order::Respond(w::RfqResponse {
                id: id.clone(),
                bid: *bid,
                ask: *ask,
                size: *size,
                expiry_slot: expires_slot,
            }),
            &Action::Swap {
                side: s,
                amount,
                unit: u,
                limit,
            } => w::Order::Swap(w::Swap {
                side: side(s),
                amount,
                unit: unit(u),
                limit,
            }),
            Action::AddLiquidity {
                base,
                quote,
                range: r,
                position,
                shape: s,
            } => w::Order::AddLiquidity(w::AddLiquidity {
                base: *base,
                quote: *quote,
                range: r.map(range),
                position: position.clone(),
                shape: s.map(shape),
            }),
            Action::RemoveLiquidity {
                position,
                share,
                min_base,
                min_quote,
            } => w::Order::RemoveLiquidity(w::RemoveLiquidity {
                position: position.clone(),
                share: *share,
                min_base: *min_base,
                min_quote: *min_quote,
            }),
            Action::Deposit { amount: a } => w::Order::Deposit(amount(a)),
            Action::Withdraw { amount: a } => w::Order::Withdraw(amount(a)),
            Action::Borrow { amount: a } => w::Order::Borrow(amount(a)),
            Action::Repay { amount: a } => w::Order::Repay(amount(a)),
            Action::Liquidate {
                obligation,
                collateral,
                repay,
                min_received,
            } => w::Order::Liquidate(w::Liquidate {
                obligation: obligation.clone(),
                collateral: collateral.clone(),
                repay: *repay,
                min_received: *min_received,
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
                    orders: v
                        .orders
                        .iter()
                        .map(|o| w::OpenOrder {
                            client_id: o.client_id,
                            side: side(o.side),
                            price: o.price,
                            size: o.size,
                        })
                        .collect(),
                    liquidity: v
                        .liquidity
                        .iter()
                        .map(|p| w::LiquidityPosition {
                            id: p.id.clone(),
                            range: p.range.map(range),
                            base: p.base,
                            quote: p.quote,
                            fees_base: p.fees_base,
                            fees_quote: p.fees_quote,
                        })
                        .collect(),
                    lending: v.lending.map(|l| w::LendingPosition {
                        supplied: l.supplied,
                        borrowed: l.borrowed,
                        health: l.health,
                    }),
                    stats: v.stats.clone(),
                })
                .collect(),
            sends: m
                .sends
                .iter()
                .map(|s| w::SendStatus {
                    step: s.step,
                    source: match s.source {
                        SendSource::Line(n) => w::SendSource::Line(n),
                        SendSource::Raw(n) => w::SendSource::Raw(n),
                    },
                    venue: s.venue.clone(),
                    client_id: s.client_id,
                    order: s.order.as_ref().map(|o| order(o, s.expires_slot)),
                    state: match &s.state {
                        SendState::Landed(signature) => w::SendState::Landed(signature.clone()),
                        SendState::Failed { reason, signature } => {
                            w::SendState::Failed(w::Failure {
                                reason: reason.clone(),
                                signature: signature.clone(),
                            })
                        }
                        SendState::NotSent(why) => w::SendState::NotSent(why.clone()),
                        SendState::Delayed => w::SendState::Delayed,
                        SendState::Expired => w::SendState::Expired,
                    },
                    slot: s.slot,
                    fee: s.fee,
                    tip: s.tip,
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
                    side: r.side.map(side),
                    size: r.size,
                    deadline_slot: r.deadline_slot,
                })
                .collect(),
            notes: m.notes.clone(),
            fees: m.fees.as_ref().map(|f| w::FeeView {
                cu_prices: f.cu_prices.clone(),
                tip_floor: f.tip_floor,
                leaders: f.leaders.clone(),
            }),
        }
    }

    fn tx_options(tx: w::TxOptions) -> TxOptions {
        TxOptions {
            cu_limit: tx.cu_limit,
            cu_price: tx.cu_price,
            tip: tx.tip,
            bundle: tx.bundle,
            route: tx.route.map(|route| match route {
                w::Route::Rpc => Route::Rpc,
                w::Route::Staked => Route::Staked,
                w::Route::Jito(j) => Route::Jito {
                    bundle_only: j.bundle_only,
                    dont_front: j.dont_front,
                },
            }),
            retry_until_slot: tx.retry_until_slot,
        }
    }

    fn account_key(key: w::AccountKey) -> raw::AccountKey {
        match key {
            w::AccountKey::Address(address) => raw::AccountKey::Address(address),
            w::AccountKey::Authority => raw::AccountKey::Authority,
            w::AccountKey::Fresh(n) => raw::AccountKey::Fresh(n),
            w::AccountKey::Derived(n) => raw::AccountKey::Derived(n),
        }
    }

    /// A raw transaction, as the kit has it.
    pub fn raw_transaction(tx: w::RawTransaction) -> raw::RawTransaction {
        raw::RawTransaction {
            instructions: tx
                .instructions
                .into_iter()
                .map(|ix| raw::RawInstruction {
                    program: ix.program,
                    accounts: ix
                        .accounts
                        .into_iter()
                        .map(|a| raw::RawAccount {
                            key: account_key(a.key),
                            signer: a.signer,
                            writable: a.writable,
                        })
                        .collect(),
                    data: ix.data,
                })
                .collect(),
            lookup_tables: tx.lookup_tables,
            fresh_signers: tx.fresh_signers,
            derived: tx
                .derived
                .into_iter()
                .map(|d| match d {
                    w::Derivation::Pda(p) => raw::Derivation::Pda {
                        program: account_key(p.program),
                        seeds: p
                            .seeds
                            .into_iter()
                            .map(|seed| match seed {
                                w::Seed::Bytes(bytes) => raw::Seed::Bytes(bytes),
                                w::Seed::Key(key) => raw::Seed::Key(account_key(key)),
                            })
                            .collect(),
                    },
                    w::Derivation::Ata(a) => raw::Derivation::Ata {
                        owner: account_key(a.owner),
                        mint: account_key(a.mint),
                        token_program: account_key(a.token_program),
                    },
                })
                .collect(),
            tx: tx.tx.map(tx_options),
        }
    }

    /// A line of the sheet, as the kit plans it.
    pub fn line(intent: w::Intent) -> (Intent, LineOptions) {
        let mut options = LineOptions {
            venue: intent.venue,
            client_id: intent.client_id,
            reduce_only: intent.reduce_only,
            tx: intent.tx.map(tx_options),
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
                options.expires_slot = q.expires_slot;
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
                options.expires_slot = o.expires_slot;
                Intent::Limit {
                    side: kit_side(o.side),
                    price: o.price,
                    size: o.size,
                }
            }
            w::Action::CancelAll => Intent::CancelAll,
            w::Action::Cancel(client_id) => Intent::Cancel { client_id },
            w::Action::Respond(r) => {
                options.expires_slot = r.expiry_slot;
                Intent::Respond {
                    id: r.id,
                    bid: r.bid,
                    ask: r.ask,
                    size: r.size,
                }
            }
            w::Action::Swap(o) => Intent::Swap {
                side: kit_side(o.side),
                amount: o.amount,
                unit: match o.unit {
                    w::AmountUnit::Base => AmountUnit::Base,
                    w::AmountUnit::Quote => AmountUnit::Quote,
                },
                limit: o.limit,
            },
            w::Action::AddLiquidity(o) => Intent::AddLiquidity {
                base: o.base,
                quote: o.quote,
                range: o.range.map(kit_range),
                position: o.position,
                shape: o.shape.map(|shape| match shape {
                    w::LiquidityShape::Spot => LiquidityShape::Spot,
                    w::LiquidityShape::Curve => LiquidityShape::Curve,
                    w::LiquidityShape::BidAsk => LiquidityShape::BidAsk,
                }),
            },
            w::Action::RemoveLiquidity(o) => Intent::RemoveLiquidity {
                position: o.position,
                share: o.share,
                min_base: o.min_base,
                min_quote: o.min_quote,
            },
            w::Action::Deposit(a) => Intent::Deposit { amount: a.amount },
            w::Action::Withdraw(a) => Intent::Withdraw { amount: a.amount },
            w::Action::Borrow(a) => Intent::Borrow { amount: a.amount },
            w::Action::Repay(a) => Intent::Repay { amount: a.amount },
            w::Action::Liquidate(o) => Intent::Liquidate {
                obligation: o.obligation,
                collateral: o.collateral,
                repay: o.repay,
                min_received: o.min_received,
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

#[cfg(test)]
mod tests {
    use mtx_core::turns::{BookLevels, Level, Role, Trade};
    use mtx_kit::{
        Action, AmountUnit, Market, Orders, Position, PriceRange, Route, Session, Side, TxOptions,
        VenueInfo,
    };

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
            venues: VenueInfo::of_perp_session(&mandate, "SPCX", Some("market".into())),
            mandate,
            venue: "{}".into(),
            authority: String::new(),
            vars: Vec::new(),
            seed: 7,
            reports: mtx_kit::Reports {
                tape: true,
                depth: true,
                timestamps: true,
                fees: false,
            },
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
    /// venues, a tape with one of the strategy's fills, depth, a clock, what
    /// became of the last sheet, a request for a quote and a fee market.
    fn rich_market(session: &Session) -> Market {
        let venues = &session.venues;
        let trade = |side, price, base, slot, own| Trade {
            venue: "phoenix".into(),
            market: Some(venues[0].id.clone()),
            side,
            price,
            base,
            slot,
            taker: Some("taker".into()),
            maker: Some("maker".into()),
            own,
        };
        let tape = vec![
            trade(mtx_core::Side::Sell, 99.95, 4.0, 101, Some(Role::Maker)),
            trade(mtx_core::Side::Buy, 100.05, 1.5, 102, None),
        ];
        let depth = vec![
            BookLevels {
                venue: "phoenix".into(),
                market: Some(venues[0].id.clone()),
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
                market: Some(venues[1].id.clone()),
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
        let (views, fills) = mtx_kit::venue_views(
            venues,
            session.reports,
            Some(99.9),
            Some(100.1),
            position,
            &tape,
            &depth,
        );
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
                    source: SendSource::Line(2),
                    venue: venues[0].id.clone(),
                    client_id: Some(7),
                    order: Some(Action::PostOnly {
                        side: Side::Bid,
                        price: 99.5,
                        size: 10.0,
                    }),
                    expires_slot: Some(150),
                    state: SendState::Landed("sig".into()),
                    slot: Some(101),
                    fee: Some(5_000),
                    tip: None,
                },
                SendStatus {
                    step: 0,
                    source: SendSource::Line(3),
                    venue: venues[1].id.clone(),
                    client_id: None,
                    order: Some(Action::Respond {
                        id: "rfq-0".into(),
                        bid: Some(150.0),
                        ask: None,
                        size: Some(2.0),
                    }),
                    expires_slot: Some(104),
                    state: SendState::NotSent("no requests for quotes".into()),
                    slot: None,
                    fee: None,
                    tip: None,
                },
                SendStatus {
                    step: 0,
                    source: SendSource::Raw(0),
                    venue: String::new(),
                    client_id: None,
                    order: None,
                    expires_slot: None,
                    state: SendState::Failed {
                        reason: "failed on chain".into(),
                        signature: Some("sig".into()),
                    },
                    slot: None,
                    fee: None,
                    tip: None,
                },
            ],
            rfqs: vec![
                mtx_kit::Rfq {
                    id: "rfq-1".into(),
                    venue: venues[0].id.clone(),
                    side: Some(Side::Ask),
                    size: 5.0,
                    deadline_slot: 110,
                },
                mtx_kit::Rfq {
                    id: "rfq-2".into(),
                    venue: venues[0].id.clone(),
                    side: None,
                    size: 1.0,
                    deadline_slot: 112,
                },
            ],
            notes: vec!["bid of 20 cut to 10, to stay within the cap".into()],
            fees: Some(mtx_kit::FeeView {
                cu_prices: vec![(50, 1_000), (90, 25_000)],
                tip_floor: Some(10_000),
                leaders: vec!["leader".into()],
            }),
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

        let mut market = rich_market(&session);
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
                route: Some(Route::Jito {
                    bundle_only: true,
                    dont_front: true,
                }),
                retry_until_slot: Some(112),
            })
            .bid(102.0, 2.0)
            .ioc()
            .client_id(1)
            .sell(5.0, 98.0)
            .reduce_only()
            .bid(94.9, 1.0)
            .post_only()
            .client_id(2)
            .ask(99.9, 1.0)
            .post_only()
            .ask(110.0, 1.0)
            .reduce_only()
            .expires(202)
            .target_position(3.0)
            .participation(0.1)
            .buy(1.0, 102.0)
            .at("openbook:spot:SOL-USDC")
            .respond("rfq-1", Some(100.0), None)
            .expires(107)
            .cancel(2)
            .swap(Side::Bid, 100.0, AmountUnit::Quote, 101.0)
            .add_liquidity(
                1.0,
                100.0,
                Some(PriceRange {
                    lower: 90.0,
                    upper: 110.0,
                }),
            )
            .push(
                Intent::AddLiquidity {
                    base: 1.0,
                    quote: 100.0,
                    range: None,
                    position: Some("position".into()),
                    shape: Some(mtx_kit::LiquidityShape::BidAsk),
                },
                LineOptions::default(),
            )
            .push(
                Intent::RemoveLiquidity {
                    position: None,
                    share: 0.0,
                    min_base: Some(0.5),
                    min_quote: None,
                },
                LineOptions::default(),
            )
            .deposit(10.0)
            .withdraw(None)
            .borrow(5.0)
            .repay(None)
            .push(
                Intent::Liquidate {
                    obligation: "11111111111111111111111111111111".into(),
                    collateral: "phoenix:perp:SPCX".into(),
                    repay: 1.0,
                    min_received: Some(0.9),
                },
                LineOptions::default(),
            );
        assert_eq!(lines, expected.clone().into_lines());

        // The raw transactions it asked for came across whole, fresh
        // signers and derived addresses included, and were refused: the
        // well-formed one because raw sends are off, the cyclic one for its
        // cycle.
        use mtx_kit::raw::{AccountKey, Derivation, RawAccount, RawInstruction, Seed};
        let clmm = "CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK";
        let account = |key, signer, writable| RawAccount {
            key,
            signer,
            writable,
        };
        let open_position = mtx_kit::RawTransaction {
            instructions: vec![RawInstruction {
                program: clmm.into(),
                accounts: vec![
                    account(AccountKey::Authority, true, true),
                    account(AccountKey::Fresh(0), true, true),
                    account(AccountKey::Derived(0), false, true),
                    account(AccountKey::Derived(1), false, true),
                ],
                data: vec![77, 255, 0, 1],
            }],
            lookup_tables: vec!["AddressLookupTab1e1111111111111111111111111".into()],
            fresh_signers: 1,
            derived: vec![
                Derivation::Pda {
                    program: AccountKey::Address(clmm.into()),
                    seeds: vec![
                        Seed::Bytes(b"position".to_vec()),
                        Seed::Key(AccountKey::Fresh(0)),
                    ],
                },
                Derivation::Ata {
                    owner: AccountKey::Authority,
                    mint: AccountKey::Fresh(0),
                    token_program: AccountKey::Address(
                        "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb".into(),
                    ),
                },
            ],
            tx: Some(TxOptions {
                cu_limit: Some(600_000),
                ..TxOptions::default()
            }),
        };
        let mut cyclic = open_position.clone();
        cyclic.derived[0] = Derivation::Pda {
            program: AccountKey::Derived(1),
            seeds: vec![],
        };
        assert_eq!(strategy.raw_sends(), [open_position, cyclic]);
        assert!(
            log.text()
                .contains(r#"send: Err("raw sends aren't enabled in this session")"#),
            "{}",
            log.text()
        );
        assert!(
            log.text()
                .contains(r#"send cyclic: Err("derived address 1 is named before it is made")"#)
        );

        // Planned as the grader sends it, from a position of 4: the IOC buy
        // may not fill, so the reduce-only sell gets the 4 held, and the
        // reduce-only ask nothing (the sell and the post-only ask could take
        // the rest); the target may trade 10% of the 5.5 on the tape, less
        // than its minimum trade.
        let (planned, notes) = mtx_kit::plan_sheet(&expected, &market);
        let lines: Vec<_> = planned.iter().map(|p| p.line).collect();
        assert_eq!(
            lines,
            [0, 1, 2, 3, 4, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18]
        );
        assert!(planned[2].reduce_only);
        assert_eq!(planned[6].expires_slot, Some(107));
        assert_eq!(
            planned[2].action,
            Action::Market {
                side: Side::Ask,
                size: 4.0,
                limit: 98.0
            }
        );
        assert_eq!(planned[1].client_id, Some(1));
        assert_eq!(planned[0].tx.and_then(|tx| tx.tip), Some(10_000));
        assert_eq!(planned[5].venue.as_deref(), Some("openbook:spot:SOL-USDC"));
        assert_eq!(
            notes,
            [
                "reduce-only market order of 5 cut to 4, so as not to grow the position",
                "reduce-only resting order of 1 cut to 0, so as not to grow the position",
                "participation 0.1 of the step's volume: trading 0.55 of 1"
            ]
        );

        assert!(matches!(
            strategy.end(Some(&market)).await,
            Outcome::Done(())
        ));
        assert!(log.text().contains("end 0"));
    }

    /// The spot smoke as a component: set `MTX_TEST_SPOT_WASM` to
    /// `bots/spot-smoke`'s buyer build (`just build` there).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a built component"]
    async fn the_spot_smoke_component_sells_and_buys_once() {
        let path = PathBuf::from(std::env::var("MTX_TEST_SPOT_WASM").unwrap());
        let mut strategy = WasmStrategy::load(
            &path,
            "http://127.0.0.1:1",
            Box::new(std::io::sink()),
            7,
            100 * FUEL_PER_CALL,
        )
        .await
        .unwrap();
        let venues: Vec<String> = vec!["phoenix".into(), "openbook".into()];
        let mandate =
            mtx_core::Mandate::spot(&crate::venue::books::VENUES, crate::spot::SYMBOL, 10.0);
        let own = mandate.universe[0].instrument.clone();
        let session = Session {
            steps: 20,
            slots_per_step: 2,
            start_slot: 100,
            end_slot: 140,
            symbol: crate::spot::SYMBOL.into(),
            start_mark: 150.0,
            start_oracle: 150.0,
            cap: 10.0,
            max_drawdown: 1_500.0,
            venues: VenueInfo::of_mandate(&mandate, &own, |_| true),
            mandate,
            venue: serde_json::to_string(&crate::venue::books::SessionVenues::of(&venues).unwrap())
                .unwrap(),
            authority: String::new(),
            vars: Vec::new(),
            seed: 7,
            reports: mtx_kit::Reports::default(),
            applies_tx_options: false,
        };
        assert!(matches!(strategy.start(&session).await, Outcome::Done(())));
        let market = |k| Market {
            best_bid: Some(149.9),
            best_ask: Some(150.1),
            last: false,
            ..market(k, 150.0, 10.0)
        };
        // It reads the books through rpc, which this test doesn't answer,
        // and trades anyway: a sell, and the buyer's buy.
        let Outcome::Done(first) = strategy.step(&market(0)).await else {
            panic!()
        };
        let sides: Vec<Side> = first
            .iter()
            .map(|(intent, _)| match intent {
                Intent::Market { side, .. } => *side,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(sides, [Side::Ask, Side::Bid]);
        let Outcome::Done(second) = strategy.step(&market(1)).await else {
            panic!()
        };
        assert!(second.is_empty());
    }
}
