//! Strategies as WebAssembly components (`docs/rfd/0002-wasm-strategies.md`):
//! a bot directory with a `bot.wasm` is a component of the `mtx:strategy`
//! world (`crates/kit/wit`), which the grader calls instead of starting
//! a process.
//!
//! The component gets no files, no environment and no network. Its WASI
//! clocks are fixed and its randomness is seeded, so a step's result depends
//! only on what the grader hands it and what `rpc` reads, which is the
//! paused chain. Each call runs on a fuel budget: a strategy that runs out
//! traps, and the step sends nothing.

use std::{
    io::Write,
    path::{Path, PathBuf},
    time::Duration,
};

use rand::SeedableRng;
use wasmtime::{
    Config, Engine, Store,
    component::{Component, HasSelf, Linker, ResourceTable},
};
use wasmtime_wasi::{
    HostMonotonicClock, HostWallClock, WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView,
};

use crate::Error;

wasmtime::component::bindgen!({
    world: "strategy",
    path: "../kit/wit",
    imports: { default: async },
    exports: { default: async },
});

pub use mtx::strategy::types::{Execution, Position, Side};

/// The file a bot directory holds when its bot is a component.
pub const BOT_WASM: &str = "bot.wasm";

/// Fuel for one call: far more than a strategy should need. To calibrate on
/// the reference bots (RFD 0002, step 3).
const FUEL_PER_CALL: u64 = 10_000_000_000;

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
}

impl WasmStrategy {
    /// Instantiates the component at `path`. `rpc_url` answers its `rpc`
    /// calls, its log lines go to `log`, and `seed` seeds its randomness.
    pub async fn load(
        path: &Path,
        rpc_url: &str,
        log: Box<dyn Write + Send>,
        seed: u64,
    ) -> crate::Result<Self> {
        let wasm_error = |e: wasmtime::Error| Error::Wasm(format!("{}: {e:#}", path.display()));
        let mut config = Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(wasm_error)?;
        let component = Component::from_file(&engine, path).map_err(wasm_error)?;
        let mut linker: Linker<Host> = Linker::new(&engine);
        wasmtime_wasi::p2::add_to_linker_async(&mut linker).map_err(wasm_error)?;
        Strategy::add_to_linker::<_, HasSelf<_>>(&mut linker, |host| host).map_err(wasm_error)?;
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
        };
        let mut store = Store::new(&engine, host);
        store.set_fuel(FUEL_PER_CALL).map_err(wasm_error)?;
        let strategy = Strategy::instantiate_async(&mut store, &component, &linker)
            .await
            .map_err(wasm_error)?;
        Ok(Self { store, strategy })
    }

    fn refuel(&mut self) {
        let _ = self.store.set_fuel(FUEL_PER_CALL);
        self.store.data_mut().rpc_calls = 0;
    }

    pub async fn start(&mut self, session: &Session) -> Outcome<()> {
        self.refuel();
        match self.strategy.call_start(&mut self.store, session).await {
            Ok(Ok(())) => Outcome::Done(()),
            Ok(Err(e)) => Outcome::Failed(e),
            Err(trap) => Outcome::Trapped(format!("{trap:#}")),
        }
    }

    pub async fn step(&mut self, market: &Market) -> Outcome<Vec<Intent>> {
        self.refuel();
        match self.strategy.call_step(&mut self.store, *market).await {
            Ok(Ok(intents)) => Outcome::Done(intents),
            Ok(Err(e)) => Outcome::Failed(e),
            Err(trap) => Outcome::Trapped(format!("{trap:#}")),
        }
    }

    pub async fn end(&mut self, last: Option<&Market>) -> Outcome<()> {
        self.refuel();
        match self.strategy.call_end(&mut self.store, last.copied()).await {
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
}

/// A component playing one session: the strategy, and the venue the grader
/// trades for it, with the bot's key, through the bot's guarded endpoint.
/// Reads and sends as `mtx_kit::run` does for a process bot.
pub struct WasmBot {
    strategy: WasmStrategy,
    trader: mtx_kit::Trader,
    session: Session,
    pnl: f64,
    peak_pnl: f64,
    last_mark: Option<f64>,
    resting: (f64, f64),
    last: Option<Market>,
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
        } = clock;
        let trader = mtx_kit::Trader::connect(env)?;
        let start_mark = trader.mark_usd().await?;
        let start_oracle = trader.oracle_usd().await?;
        let seed = seed_of(scenario);
        let mut strategy = WasmStrategy::load(path, &env.rpc_url, log, seed).await?;
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
            seed,
        };
        match strategy.start(&session).await {
            Outcome::Done(()) => {}
            Outcome::Failed(e) | Outcome::Trapped(e) => {
                tracing::warn!(error = %e, "the strategy's start failed; it plays on");
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
        })
    }

    /// The strategy's turn at step `k`: read the market, call `step`, cut
    /// the sheet to the mandate's cap and send it, in order, stopping at the
    /// first order that fails. Returns whether the strategy answered.
    pub async fn turn(&mut self, k: u64, slot: u64, last: bool) -> bool {
        let (mark, oracle, position) = match (
            self.trader.mark_usd().await,
            self.trader.oracle_usd().await,
            self.trader.position().await,
        ) {
            (Ok(mark), Ok(oracle), Ok(position)) => (mark, oracle, position),
            (mark, oracle, position) => {
                tracing::warn!(step = k, mark = ?mark.err(), oracle = ?oracle.err(), position = ?position.err(), "couldn't read the market; skipping the step");
                return false;
            }
        };
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
            position,
            cap: self.session.cap,
            max_drawdown: self.session.max_drawdown,
            resting_bids: self.resting.0,
            resting_asks: self.resting.1,
            pnl: self.pnl,
            peak_pnl: self.peak_pnl,
            drawdown: self.peak_pnl - self.pnl,
        };
        let market = Market {
            k,
            slot,
            last,
            steps_left: kit.steps_left,
            slots_left: kit.slots_left,
            mark,
            oracle,
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
        let intents = match self.strategy.step(&market).await {
            Outcome::Done(intents) => intents,
            Outcome::Failed(e) => {
                tracing::warn!(step = k, error = %e, "the strategy failed; no orders this step");
                return true;
            }
            Outcome::Trapped(e) => {
                tracing::warn!(step = k, error = %e, "the strategy trapped; no orders this step");
                return false;
            }
        };
        let intents: Vec<mtx_kit::Intent> = intents.into_iter().map(kit_intent).collect();
        let (actions, notes) = mtx_kit::plan(&intents, &kit);
        for note in notes {
            tracing::warn!(step = k, "{note}");
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
        true
    }

    /// The session is over: the strategy's `end`.
    pub async fn end(&mut self) {
        let last = self.last;
        if let Outcome::Trapped(e) = self.strategy.end(last.as_ref()).await {
            tracing::warn!(error = %e, "the strategy's end trapped");
        }
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

/// A scenario's seed for the strategy: the same in every run of it (FNV-1a
/// of its name).
fn seed_of(scenario: &str) -> u64 {
    scenario.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
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
    /// `bots/reference-wasm`'s build.
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
