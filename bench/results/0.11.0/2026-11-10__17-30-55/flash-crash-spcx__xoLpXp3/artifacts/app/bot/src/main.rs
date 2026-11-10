//! Shock-fading bot for Phoenix SPCX: see `core.rs` for the decision core.

mod core;

use mtx_kit::prelude::*;
use tracing::info;

struct Bot {
    core: Option<core::Core>,
}

impl Strategy for Bot {
    async fn start(&mut self, session: &Session) -> BotResult {
        self.core = Some(core::Core::new(
            core::Params::default(),
            session.env.max_position,
            session.env.max_drawdown_usd,
        ));
        info!(steps = session.hello.steps, cap = session.env.max_position, "started");
        Ok(())
    }

    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let core = self.core.get_or_insert_with(|| {
            core::Core::new(core::Params::default(), market.cap, market.max_drawdown)
        });
        let target = core.step(market.k as usize, market.mark, market.position.size);
        info!(
            step = market.k,
            mark = market.mark,
            anchor = core.anchor(),
            position = market.position.size,
            target,
            pnl = market.pnl,
            eq = core.equity,
            dd = core.drawdown(),
            "step"
        );
        if (target - market.position.size).abs() >= 1.0 {
            orders.target_position_with(
                target,
                Execution {
                    slice: 10.0,
                    slippage_bps: 150.0,
                    ..Execution::default()
                },
            );
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Bot { core: None })
}
