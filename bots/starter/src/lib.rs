//! A strategy that watches the market and never trades.
//!
//! Start from here: the plumbing works, the strategy is yours. The grader
//! reads the market before every step, calls `step`, and sends what it puts
//! on the order sheet, cut to the mandate's position cap. Fill in `step`:
//! `orders.target_position(units)` to be long or short,
//! `orders.quote(bid, ask, size)` to make a market, and `mtx_kit::rpc` to
//! read anything else from the chain. `market.sends` says what became of
//! the last step's orders. `build.sh` builds it into `bot.wasm`.

use mtx_kit::{log, prelude::*};

#[derive(Default)]
struct Watcher;

impl Strategy for Watcher {
    fn start(&mut self, session: &Session) -> BotResult {
        log(&format!(
            "started: {} for {} steps, cap {}",
            session.symbol, session.steps, session.cap
        ));
        Ok(())
    }

    fn step(&mut self, market: &Market, _orders: &mut Orders) -> BotResult {
        log(&format!(
            "step {}: mark {:.2}, position {}, pnl {:.2}",
            market.k, market.mark, market.position.size, market.pnl
        ));
        Ok(())
    }
}

mtx_kit::export!(Watcher);
