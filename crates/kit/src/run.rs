//! A [`Strategy`] as a process bot: [`run`] answers the grader's turn
//! protocol, reads the market, calls the strategy and sends its sheet.
//!
//! A component (feature `component`) doesn't need this: the grader calls it
//! directly. A process is for practice on a laptop, `mtx bot practice`, and
//! tasks that grade processes.
//!
//! ```no_run
//! use mtx_kit::prelude::*;
//!
//! #[derive(Default)]
//! struct Flat;
//!
//! impl Strategy for Flat {
//!     fn step(&mut self, _market: &Market, _orders: &mut Orders) -> BotResult {
//!         Ok(())
//!     }
//! }
//!
//! fn main() -> BotResult {
//!     mtx_kit::run(Flat)
//! }
//! ```

use std::sync::OnceLock;

use solana_rpc_client_api::request::RpcRequest;
use tracing::{info, warn};

use crate::{
    Action, BotEnv, BotResult, Market, Orders, Session, Strategy, Trader, Turns, plan,
    resting_after, rpc_client,
};

/// The endpoint [`crate::rpc`] calls in a process: the session's.
static RPC_URL: OnceLock<String> = OnceLock::new();

/// [`crate::rpc`] in a process: a blocking call to the session's endpoint,
/// for a method of [`crate::RPC_METHODS`].
pub(crate) fn process_rpc(method: &str, params: &str) -> Result<String, String> {
    let url = RPC_URL
        .get()
        .cloned()
        .or_else(|| std::env::var(BotEnv::RPC_URL).ok())
        .ok_or("no session endpoint: rpc works inside a session")?;
    let method = crate::RPC_METHODS
        .iter()
        .find(|m| **m == method)
        .ok_or_else(|| format!("{method} is not a read a strategy may make"))?;
    let params: serde_json::Value =
        serde_json::from_str(params).map_err(|e| format!("params are not JSON: {e}"))?;
    let client = rpc_client(url);
    // The blocking client runs its own runtime: off the async one.
    let answer: Result<serde_json::Value, String> = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                client
                    .send(RpcRequest::Custom { method }, params)
                    .map_err(|e| e.to_string())
            })
            .join()
            .map_err(|_| "the rpc call panicked".to_string())?
    });
    Ok(answer?.to_string())
}

/// Run a strategy for one session as a process: the whole bot. Call it from
/// `main`.
///
/// It answers the grader's hello, reads the session, connects to the venue,
/// and calls [`Strategy::start`]. Then, every step, it reads the market,
/// calls [`Strategy::step`], sends the sheet, and hands the turn back, until
/// the grader ends the session. Logs go to standard error, at the level
/// `RUST_LOG` names.
pub fn run<S: Strategy>(strategy: S) -> BotResult {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();
    // Multi-threaded: the blocking RPC client underneath the trader only
    // runs on that kind of runtime.
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?
        .block_on(session(strategy))
}

async fn session<S: Strategy>(mut strategy: S) -> BotResult {
    // First: the world waits for the bot's answer to the hello.
    let mut turns = Turns::connect()?;
    let hello = turns.hello().clone();
    let env = BotEnv::from_env()?;
    let mandate = env.mandate()?;
    let trader = Trader::connect(&env)?;
    let _ = RPC_URL.set(env.rpc_url.clone());
    let start_mark = trader.mark_usd().await?;
    let session = Session {
        steps: hello.steps,
        slots_per_step: hello.slots_per_step,
        start_slot: hello.start_slot,
        end_slot: hello.end_slot,
        symbol: env.symbol.clone(),
        start_mark,
        cap: env.max_position,
        max_drawdown: env.max_drawdown_usd,
        mandate,
        seed: hello.start_slot,
    };
    strategy.start(&session)?;
    info!(authority = %trader.authority(), end_slot = session.end_slot, "started");

    let (mut pnl, mut peak_pnl, mut last_mark): (f64, f64, Option<f64>) = (0.0, 0.0, None);
    let mut last_market: Option<Market> = None;
    // What the loop has resting: see [`Market::resting_bids`].
    let (mut resting_bids, mut resting_asks) = (0.0, 0.0);
    while let Some(step) = turns.next()? {
        let (mark, oracle, position) = match (
            trader.mark_usd().await,
            trader.oracle_usd().await,
            trader.position().await,
        ) {
            (Ok(mark), Ok(oracle), Ok(position)) => (mark, oracle, position),
            (mark, oracle, position) => {
                warn!(step = step.k, mark = ?mark.err(), oracle = ?oracle.err(), position = ?position.err(), "couldn't read the market; skipping the step");
                continue;
            }
        };
        if let Some(last) = last_mark {
            pnl += position.size * (mark - last);
        }
        last_mark = Some(mark);
        peak_pnl = peak_pnl.max(pnl);
        let market = Market {
            k: step.k,
            slot: step.slot,
            last: step.last,
            steps_left: session.steps.saturating_sub(step.k + 1),
            slots_left: session.end_slot.saturating_sub(step.slot),
            mark,
            oracle,
            position,
            cap: session.cap,
            max_drawdown: session.max_drawdown,
            resting_bids,
            resting_asks,
            pnl,
            peak_pnl,
            drawdown: peak_pnl - pnl,
        };
        let mut orders = Orders::new();
        match strategy.step(&market, &mut orders) {
            Ok(()) => {
                let (actions, notes) = plan(orders.intents(), &market);
                for note in notes {
                    warn!(step = step.k, "{note}");
                }
                send(&trader, &actions).await;
                // A resting order that failed to send counts anyway: the
                // ledger only ever over-counts.
                (resting_bids, resting_asks) = resting_after(resting_bids, resting_asks, &actions);
            }
            Err(error) => warn!(step = step.k, %error, "the strategy failed; no orders this step"),
        }
        last_market = Some(market);
    }
    strategy.end(last_market.as_ref());
    Ok(())
}

/// Send planned orders, in order. A failed order is logged, and the rest of
/// the step's orders still go: each is its own transaction.
async fn send(trader: &Trader, actions: &[Action]) {
    for action in actions {
        let result = match *action {
            Action::Market { side, size, limit } => {
                trader.place_market(side, size, limit).await.map(|_| ())
            }
            Action::Limit { side, price, size } => {
                trader.place_limit(side, price, size).await.map(|_| ())
            }
            Action::CancelAll => trader.cancel_all().map(|_| ()),
        };
        if let Err(error) = result {
            warn!(?action, %error, "order failed");
        }
    }
}
