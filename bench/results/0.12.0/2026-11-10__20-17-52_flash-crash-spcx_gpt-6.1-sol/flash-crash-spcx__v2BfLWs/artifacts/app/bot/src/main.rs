use bot::{forecast::Forecaster, risk::Risk};
use mtx_kit::prelude::*;
use tracing::info;

#[derive(Default)]
struct ShockBot {
    signal: Forecaster,
    risk: Risk,
}
impl Strategy for ShockBot {
    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        let fraction = self
            .signal
            .step(m.oracle, m.steps_left as usize, m.position.size);
        let Some(trader) = orders.trader() else {
            return Ok(());
        };
        let margin = trader
            .phoenix
            .hawkeye(trader.authority())
            .view_margin_for_trader(trader.account)
            .await?
            .value;
        let equity = (margin.collateral_quote_lots + margin.unrealized_pnl_quote_lots) as f64 / 1e6;
        let decision = self.risk.assess(
            equity,
            m.mark,
            m.cap,
            m.max_drawdown,
            fraction,
            margin.is_liquidatable != 0,
        );
        let target = decision.target;
        let dd = decision.drawdown;
        if m.last {
            return Ok(());
        }
        info!(
            k = m.k,
            mark = m.mark,
            oracle = m.oracle,
            pos = m.position.size,
            equity,
            dd,
            target,
            score = self.signal.score,
            "state"
        );
        orders.target_position(target);
        Ok(())
    }
}
fn main() -> BotResult {
    mtx_kit::run(ShockBot::default())
}
