//! SPCX shock trading with causal forecasts and account-equity risk limits.
use mtx_kit::prelude::*;

mod features;
mod model;

#[derive(Default)]
struct ShockBot {
    features: features::Features,
    peak: f64,
    halted: bool,
}

impl Strategy for ShockBot {
    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        if !(m.oracle.is_finite() && m.oracle > 0.0 && m.mark.is_finite() && m.mark > 0.0) {
            self.halted = true;
            orders.target_position(0.0);
            return Ok(());
        }

        // Oracle history prevents our own fills from manufacturing a signal.
        let (x, direction) = self.features.next(m.oracle, m.steps_left);
        let y = model::predict(&x);
        // Fast forecasts favor continuation; slower ones capture recovery.
        let score = direction * (10.05 * y[0] + 7.27 * y[1] + 2.40 * y[2]
            + 0.62 * y[3] + 0.46 * y[4] + y[5]);
        let mut target = m.position.size;
        let enter = 0.0128;
        let exit = -0.0171;
        if score > enter {
            target = m.cap;
        } else if score < -enter {
            target = -m.cap;
        } else if score * target.signum() < exit {
            target = 0.0;
        }

        let Some(trader) = orders.trader() else {
            orders.target_position(target);
            return Ok(());
        };
        // Actual collateral includes realized PnL, fees, and execution costs.
        let view = trader.phoenix.hawkeye(trader.authority())
            .view_margin(trader.authority(), 0, 0).await;
        let Ok(view) = view else {
            orders.target_position(0.0);
            return Ok(());
        };
        let account = view.value;
        let equity = (account.collateral_quote_lots + account.unrealized_pnl_quote_lots)
            as f64 / 1e6;
        self.peak = self.peak.max(equity).max(10000.0);
        let drawdown = self.peak - equity;
        if drawdown > m.max_drawdown * 0.6 || account.is_liquidatable != 0 {
            self.halted = true;
        }
        // Reserve cash for execution and a move larger than the next-step limit.
        let cap = ((m.max_drawdown - drawdown - 150.0) / (m.mark * 0.045))
            .clamp(0.0, m.cap);
        target = target.clamp(-cap, cap).trunc();
        if self.halted {
            target = 0.0;
        }
        // The final account is marked to market; a final trade only adds costs.
        if !m.last {
            orders.target_position(target);
        }
        eprintln!("T {} {:.6} {:.6} {:.2} {:.2} {:.2} {:.2} {:.6}",
            m.k, m.mark, m.oracle, m.position.size,
            equity - 10000.0, target, drawdown, score);
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(ShockBot::default())
}

#[cfg(test)]
mod parity;
