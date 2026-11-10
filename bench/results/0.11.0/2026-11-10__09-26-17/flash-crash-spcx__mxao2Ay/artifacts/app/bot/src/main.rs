//! Shock trader for SPCX: rides moves while they run, fades them once they
//! stop, sized so a further leg can't break the drawdown limit. The
//! decisions live in `brain.rs`, shared with the offline simulator.

mod brain;

use brain::{Brain, Params};
use mtx_kit::prelude::*;
use tracing::info;

struct Bot {
    brain: Brain,
}

impl Strategy for Bot {
    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        if !(market.mark > 0.0) {
            return Ok(());
        }
        let room = market.max_drawdown - market.drawdown;
        let target = self.brain.decide(market.mark, market.cap, room).round();
        info!(
            step = market.k,
            mark = market.mark,
            position = market.position.size,
            target,
            pnl = market.pnl,
            "step"
        );
        if (target - market.position.size).abs() >= 1.0 && !market.last {
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
        brain: Brain::new(Params::default()),
    })
}
