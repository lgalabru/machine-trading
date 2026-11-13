//! SPCX shock strategy: two learned return horizons with inventory hysteresis.
//!
//! Every feature uses only oracle history observed in this session. The fitted
//! trees are compiled into the component; runtime has no data-file dependency.
//! Risk uses the trader's on-chain quote collateral and mark-valued position.

use mtx_kit::{
    log,
    prelude::*,
    rpc_account,
    venue::{self, PhoenixVenue, phoenix_rise::accounts::trader::Trader},
};
mod features;
mod model_fast;
mod model_slow;

#[derive(Default)]
struct ForecastBot {
    features: features::Features,
    start_mark: f64,
    trader: String,
    equity_peak: f64,
    halted: bool,
}
impl Strategy for ForecastBot {
    fn start(&mut self, s: &Session) -> BotResult {
        self.features = features::Features::new();
        self.start_mark = s.start_mark;
        self.trader = PhoenixVenue::from_json(&s.venue)?
            .trader_account(&venue::key(&s.authority)?)
            .to_string();
        self.equity_peak = 10000.0;
        Ok(())
    }
    fn step(&mut self, m: &Market, orders: &mut Orders) -> BotResult {
        if !m.oracle.is_finite() || m.oracle <= 0.0 || !m.mark.is_finite() || m.mark <= 0.0 {
            return Ok(());
        }
        let (x, d) = self
            .features
            .update((m.oracle / self.start_mark - 1.0) * 100.0);
        // Forecasts are percentage points of the starting mark. Blend the
        // two-second and five-second horizons on a common five-second scale.
        let forecast = (1.25 * model_fast::predict(&x) + 0.5 * model_slow::predict(&x)) * d;
        // Wallet USDC is constant, so it cancels out of the drawdown. The
        // collateral read includes realized trading losses and exchange fees.
        let mut dd = m.drawdown;
        let mut equity = 10000.0 + m.pnl;
        if let Ok(Some(data)) = rpc_account(&self.trader) {
            if let Ok(trader) = Trader::try_from_account_bytes(&data) {
                let collateral =
                    trader.header().state().quote_lot_collateral.as_inner() as f64 / 1e6;
                equity = collateral + m.position.size * (m.mark - m.position.entry_usd);
                self.equity_peak = self.equity_peak.max(equity);
                dd = (self.equity_peak - equity).max(0.0);
            } else {
                self.halted = true;
            }
        } else {
            self.halted = true;
        }
        // A permanent halt and an adverse-move budget protect the mandate.
        // Reserve $150 for execution/valuation differences; stress the next
        // move at 6%, including large shocks split across consecutive turns.
        if dd > 700.0 {
            self.halted = true;
        }
        let safe = ((m.max_drawdown - dd - 150.0).max(0.0) / (m.mark * 0.06)).min(m.cap);
        // Keep an existing position through small contrary forecasts; demand
        // a stronger signal to enter or reverse, avoiding repeated spread costs.
        let mut target = if forecast > 0.18 {
            safe
        } else if forecast < -0.18 {
            -safe
        } else {
            0.0
        };
        if m.position.size != 0.0 && forecast * m.position.size.signum() > -0.12 {
            target = m.position.size.signum() * safe;
        }
        if self.halted {
            target = 0.0;
        }
        // IOC market slices leave no resting orders. At the final mark there
        // is no future move to earn back a new execution cost.
        if !m.last && (target - m.position.size).abs() > 0.5 {
            orders.target_position(target);
        }
        log(&format!(
            "F {} {:.4} {:.4} {:.4} {:.4} {:.5} {:.4} {:.4}",
            m.k, m.oracle, m.mark, m.position.size, target, forecast, equity, dd
        ));
        Ok(())
    }
}
mtx_kit::export!(ForecastBot);
