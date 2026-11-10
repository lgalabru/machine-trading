//! A shock trader for SPCX: rides a move while it lands, fades it once it
//! stops, and fades fizzles, inside a drawdown guard. The decisions are in
//! `strategy`; this is the plumbing around them.

mod strategy;

use mtx_kit::prelude::*;
use strategy::{Params, Strat};
use tracing::info;

struct ShockBot {
    strat: Strat,
}

impl Strategy for ShockBot {
    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let target = self
            .strat
            .on_step(market.mark, market.position.size, market.cap);
        info!(
            step = market.k,
            mark = market.mark,
            position = market.position.size,
            entry = market.position.entry_usd,
            target,
            eq = self.strat.eq,
            phase = ?self.strat.phase,
            "step"
        );
        if market.last {
            return Ok(());
        }
        orders.target_position_with(
            target,
            Execution {
                slippage_bps: 300.0,
                ..Execution::default()
            },
        );
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(ShockBot {
        strat: Strat::new(Params::default()),
    })
}
