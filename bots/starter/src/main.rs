//! A bot that watches the market and never trades.
//!
//! Start from here: the plumbing works, the strategy is yours. `run` answers
//! the grader, reads the market before every step, and sends what `step`
//! puts on the order sheet, cut to the mandate's position cap. Fill in
//! `step`: `orders.target_position(units)` to be long or short,
//! `orders.quote(bid, ask, size)` to make a market, and `mtx_kit::rpc` to
//! read anything else from the chain.

use mtx_kit::prelude::*;
use tracing::info;

#[derive(Default)]
struct Watcher;

impl Strategy for Watcher {
    fn start(&mut self, session: &Session) -> BotResult {
        info!(
            symbol = session.symbol,
            steps = session.steps,
            cap = session.cap,
            "started"
        );
        Ok(())
    }

    fn step(&mut self, market: &Market, _orders: &mut Orders) -> BotResult {
        info!(
            step = market.k,
            mark = market.mark,
            position = market.position.size,
            pnl = market.pnl,
            "step"
        );
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Watcher)
}
