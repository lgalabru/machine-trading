//! A causal shock strategy with an embedded forecast and independent loss limits.
use mtx_kit::prelude::*;
mod features;
mod model;
mod risk;

#[derive(Default)]
struct ShockBot {
    features: features::Features,
    model: model::Model,
    risk: risk::Risk,
}

impl Strategy for ShockBot {
    async fn start(&mut self, session: &Session) -> BotResult {
        if session.env.symbol != "SPCX" {
            return Err("this strategy only trades SPCX".into());
        }
        Ok(())
    }

    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        if !market.oracle.is_finite() || market.oracle <= 0.0 || !market.mark.is_finite() || market.mark <= 0.0 {
            self.risk.halted = true;
            orders.target_position_with(0.0, execution());
            return Ok(());
        }
        let (features, orientation) = self.features.observe(market.k as usize, market.steps_left, market.oracle);
        let forecast = self.model.predict(&features) * orientation;

        // The SDK's mark-to-market estimate excludes fees. Use the actual account.
        let equity = if let Some(trader) = orders.trader() {
            match trader.phoenix.hawkeye(trader.authority()).view_margin_for_trader(trader.account).await {
                Ok(result) => (result.value.collateral_quote_lots + result.value.unrealized_pnl_quote_lots) as f64 / 1e6,
                Err(error) => {
                    self.risk.halted = true;
                    orders.target_position_with(0.0, execution());
                    eprintln!("risk account read failed: {error}");
                    return Ok(());
                }
            }
        } else { 10000.0 + market.pnl };

        // Hysteresis pays the spread only when the forecast supports a reversal.
        let entry = 0.1125;
        let exit = -0.1125;
        let desired = if forecast > entry || (market.position.size > 0.0 && forecast > exit) {
            market.cap
        } else if forecast < -entry || (market.position.size < 0.0 && forecast < -exit) {
            -market.cap
        } else { 0.0 };
        let target = self.risk.target(market, equity, desired);
        eprintln!("TRACE,{},{:.6},{:.6},{:.3},{:.3},{:.3},{:.3},{:.6}", market.k, market.mark, market.oracle, market.position.size, target, equity, self.risk.peak-equity, forecast);
        // IOC market orders only. The kit clips sizes and slices orders at 10 shares.
        orders.target_position_with(target, execution());
        Ok(())
    }
}
fn execution() -> Execution {
    Execution { slippage_bps: 50.0, ..Execution::default() }
}

fn main() -> BotResult { mtx_kit::run(ShockBot::default()) }

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn inference_matches_training_runtime() {
        let mut features = features::Features::default();
        let model = model::Model::default();
        for line in include_str!("parity.tsv").lines() {
            let a: Vec<f64> = line.split('\t').map(|v| v.parse().unwrap()).collect();
            let (x, s) = features.observe(a[0] as usize, a[1] as u64, a[2]);
            let value = model.predict(&x) * s;
            assert!((value-a[3]).abs() < 0.00001, "step {}: {} != {}", a[0], value, a[3]);
        }
    }
}
