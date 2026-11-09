//! A bot that watches the market and never trades.
//!
//! Start from here: the plumbing works, the strategy is yours. `run` answers
//! the grader, reads the market before every step, and sends what `step`
//! puts on the order sheet, cut to the mandate's position cap. Fill in
//! `step`: `orders.target_position(units)` to be long or short,
//! `orders.quote(bid, ask, size)` to make a market, and `orders.trader()`
//! for anything else.

use mtx_botkit::prelude::*;
use tracing::info;

#[derive(Default)]
struct Watcher;

impl Strategy for Watcher {
    async fn start(&mut self, session: &Session) -> BotResult {
        info!(
            symbol = session.env.symbol,
            steps = session.hello.steps,
            cap = session.env.max_position,
            "started"
        );
        Ok(())
    }

    async fn step(&mut self, market: &Market, _orders: &mut Orders<'_>) -> BotResult {
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
    mtx_botkit::run(Watcher)
}
