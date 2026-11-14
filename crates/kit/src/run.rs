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
    Action, BotEnv, BotResult, Market, Orders, Planned, SendSource, SendStatus, Session, Strategy,
    Trader, Turns, VenueInfo, phoenix_market_address, plan_sheet_with, resting_after, rpc_client,
    venue_views,
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
        .chain(crate::ROUTER_METHODS)
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
    let start_oracle = trader.oracle_usd().await?;
    let venue = std::fs::read_to_string(&env.exchange_path)?;
    let session = Session {
        steps: hello.steps,
        slots_per_step: hello.slots_per_step,
        start_slot: hello.start_slot,
        end_slot: hello.end_slot,
        symbol: env.symbol.clone(),
        start_mark,
        start_oracle,
        cap: env.max_position,
        max_drawdown: env.max_drawdown_usd,
        mandate: mandate.clone(),
        venue: venue.clone(),
        authority: trader.authority().to_string(),
        vars: crate::SESSION_VARS
            .iter()
            .filter_map(|name| Some((name.to_string(), std::env::var(name).ok()?)))
            .collect(),
        // The scenario's seed, as a component gets it; a grader from
        // before seeds sent none.
        seed: hello.seed.unwrap_or(hello.start_slot),
        venues: VenueInfo::of_perp_session(
            &mandate,
            &env.symbol,
            phoenix_market_address(&venue, &env.symbol),
        ),
        reports: hello.reports.into(),
        applies_tx_options: false,
    };
    strategy.start(&session)?;
    info!(authority = %trader.authority(), end_slot = session.end_slot, "started");

    let (mut pnl, mut peak_pnl, mut last_mark): (f64, f64, Option<f64>) = (0.0, 0.0, None);
    let mut last_market: Option<Market> = None;
    // What the loop has resting: see [`Market::resting_bids`].
    let (mut resting_bids, mut resting_asks) = (0.0, 0.0);
    // What became of the last sheet, for the next step's market.
    let (mut sends, mut notes) = (Vec::new(), Vec::new());
    while let Some(step) = turns.next()? {
        let (top, oracle, position) = match (
            trader.top().await,
            trader.oracle_usd().await,
            trader.position().await,
        ) {
            (Ok(top), Ok(oracle), Ok(position)) => (top, oracle, position),
            (top, oracle, position) => {
                warn!(step = step.k, mark = ?top.err(), oracle = ?oracle.err(), position = ?position.err(), "couldn't read the market; skipping the step");
                continue;
            }
        };
        let mark = top.mark;
        if let Some(last) = last_mark {
            pnl += position.size * (mark - last);
        }
        last_mark = Some(mark);
        peak_pnl = peak_pnl.max(pnl);
        let (venues, fills) = venue_views(
            &session.venues,
            session.reports,
            top.best_bid,
            top.best_ask,
            position,
            &step.tape,
            &step.depth,
        );
        let market = Market {
            k: step.k,
            slot: step.slot,
            last: step.last,
            steps_left: session.steps.saturating_sub(step.k + 1),
            slots_left: session.end_slot.saturating_sub(step.slot),
            mark,
            oracle,
            index: top.index,
            best_bid: top.best_bid,
            best_ask: top.best_ask,
            position,
            cap: session.cap,
            max_drawdown: session.max_drawdown,
            resting_bids,
            resting_asks,
            pnl,
            peak_pnl,
            drawdown: peak_pnl - pnl,
            tape: step.tape,
            depth: step.depth,
            unix_timestamp: step.unix_timestamp,
            venues,
            sends: std::mem::take(&mut sends),
            fills,
            rfqs: Vec::new(),
            notes: std::mem::take(&mut notes),
            fees: None,
        };
        let mut orders = Orders::new();
        match strategy.step(&market, &mut orders) {
            Ok(()) => {
                let planned;
                (planned, notes) = plan_sheet_with(&orders, &market, |venue, action| {
                    venue.is_none() && Trader::executes(action)
                });
                for note in &notes {
                    warn!(step = step.k, "{note}");
                }
                let tried;
                (sends, tried) = send(&trader, step.k, &session.venues[0].id, &planned).await;
                (resting_bids, resting_asks) = resting_after(resting_bids, resting_asks, &tried);
            }
            Err(error) => warn!(step = step.k, %error, "the strategy failed; no orders this step"),
        }
        last_market = Some(market);
    }
    strategy.end(last_market.as_ref());
    Ok(())
}

/// Send planned orders, in order, and say what became of each, with the
/// orders that reached the chain: a resting order that failed counts toward
/// what rests anyway, as the ledger only over-counts. A failed order is
/// logged, and the rest of the step's orders still go: each is its own
/// transaction. Only the session's own market is traded.
async fn send(
    trader: &Trader,
    k: u64,
    own: &str,
    planned: &[Planned],
) -> (Vec<SendStatus>, Vec<Action>) {
    let mut sends = Vec::with_capacity(planned.len());
    let mut tried = Vec::new();
    for planned in planned {
        let state = match &planned.venue {
            Some(venue) => {
                crate::SendState::NotSent(format!("{venue} isn't traded in this session"))
            }
            None => {
                let sent = trader.send_planned(planned).await;
                if sent.sent {
                    tried.push(planned.action.clone());
                }
                if let crate::SendState::Failed { reason, .. } = &sent.state {
                    warn!(action = ?planned.action, error = %reason, "order failed");
                }
                sent.state
            }
        };
        sends.push(SendStatus {
            step: k,
            source: SendSource::Line(planned.line as u32),
            venue: planned.venue.clone().unwrap_or_else(|| own.to_string()),
            client_id: planned.client_id,
            order: Some(planned.action.clone()),
            expires_slot: planned.expires_slot,
            state,
            slot: None,
            fee: None,
            tip: None,
        });
    }
    (sends, tried)
}
