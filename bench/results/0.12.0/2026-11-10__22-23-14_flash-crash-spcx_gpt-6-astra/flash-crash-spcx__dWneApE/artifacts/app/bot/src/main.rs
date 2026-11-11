use mtx_kit::prelude::*;
mod features;
mod model;
#[cfg(test)]
mod parity;
mod policy;
mod risk;
#[derive(Default)]
struct ShockBot {
    features: features::Features,
    risk: risk::RiskGuard,
}
impl Strategy for ShockBot {
    async fn start(&mut self, s: &Session) -> BotResult {
        if s.env.symbol != "SPCX" {
            return Err("this strategy only trades SPCX".into());
        }
        Ok(())
    }
    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        if !(m.oracle.is_finite()
            && m.oracle > 0.0
            && m.mark.is_finite()
            && m.mark > 0.0
            && m.cap > 0.0)
        {
            orders.target_position(0.0);
            return Ok(());
        }
        let f = self.features.update(m.oracle, m.k, m.steps_left);
        let forecast = model::predict(&f);
        let mut target = policy::target(forecast, m.position.size, m.cap);
        let t = orders.trader().ok_or("no trader")?;
        let margin = match t
            .inner
            .exchange()
            .hawkeye(&t.env.rpc_url, t.authority())
            .view_margin_for_trader(t.account)
            .await
        {
            Ok(r) => r.value,
            Err(e) => {
                eprintln!("margin unavailable: {e}");
                orders.target_position(0.0);
                return Ok(());
            }
        };
        let equity = (margin.collateral_quote_lots + margin.unrealized_pnl_quote_lots) as f64
            / 10f64.powi(t.inner.market().quote_lot_decimals as i32);
        let (safe, dd) = self.risk.target(
            equity,
            m.mark,
            m.cap,
            m.max_drawdown,
            target,
            margin.is_liquidatable != 0,
        );
        target = safe;
        // Mark settlement needs no closing trade, except for unsafe margin.
        if m.last && margin.is_liquidatable == 0 {
            target = m.position.size;
        }
        eprintln!(
            "TRACE,{},{:.5},{:.5},{:.3},{:.3},{:.3},{:.3},{:.3},{:.4},{:.4},{:.4}",
            m.k,
            m.mark,
            m.oracle,
            m.position.size,
            m.position.entry_usd,
            equity - 10000.0,
            target,
            dd,
            forecast[0],
            forecast[1],
            forecast[2]
        );
        orders.target_position(target);
        Ok(())
    }
}
fn main() -> BotResult {
    mtx_kit::run(ShockBot::default())
}
