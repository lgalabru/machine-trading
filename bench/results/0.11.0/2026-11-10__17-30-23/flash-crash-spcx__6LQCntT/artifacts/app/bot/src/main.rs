//! Trades SPCX through shocks: rides a move while it runs, fades fizzles
//! briefly, and fades a standing shock until its aftermath, under a
//! drawdown governor. The strategy itself is in `strategy.rs`.

mod strategy;

use mtx_kit::prelude::*;
use strategy::{Params, Strat};

/// What a fill is assumed to cost, as a share of its notional, in the bot's
/// own estimate of its drawdown (the run loop's leaves costs out).
const COST: f64 = 0.0010;

struct Bot {
    strat: Strat,
    base: Option<f64>,
    last_pos: f64,
    costs: f64,
    peak: f64,
}

impl Strategy for Bot {
    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let base = *self.base.get_or_insert(market.mark);
        // The oracle is the scenario's path; the mark blends in the book,
        // which the bot's own orders move.
        let price = if market.k == 0 { market.mark } else { market.oracle };
        let x = (price / base - 1.0) * 100.0;
        let pos = market.position.size;
        self.costs += (pos - self.last_pos).abs() * market.mark * COST;
        self.last_pos = pos;
        let equity = market.pnl - self.costs;
        self.peak = self.peak.max(equity);
        let notional = market.cap * base / 100.0;
        let dd = (self.peak - equity) / notional;
        let limit = market.max_drawdown / notional;
        let share = self.strat.step(x) * self.strat.allowed(dd, limit);
        let target = if market.last { pos } else { (share * market.cap).round() };
        if (target - pos).abs() >= 1.0 {
            eprintln!(
                "k={} x={:.3} mark={:.2} pos={} -> {} eq={:.1} dd%={:.2}",
                market.k, x, market.mark, pos, target, equity, dd
            );
            orders.target_position_with(
                target,
                Execution {
                    slippage_bps: 300.0,
                    ..Execution::default()
                },
            );
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Bot {
        strat: Strat::new(Params::default()),
        base: None,
        last_pos: 0.0,
        costs: 0.0,
        peak: 0.0,
    })
}
