//! Causal shock forecasts, transaction-cost hysteresis, and an account-value stop.
use mtx_kit::{log, prelude::*};

mod fast_model;
mod features;
mod slow_model;

#[derive(Default)]
struct ShockTrader {
    history: Option<features::Features>,
    trader_account: String,
    initial_value: Option<f64>,
    peak_pnl: f64,
    drawdown_limit: f64,
    estimated_costs: f64,
    previous_position: f64,
    halted: bool,
}

impl ShockTrader {
    /// Collateral includes realized PnL and exchange fees. Entry is the venue's
    /// entry price, so adding unrealized PnL gives the marked account value.
    fn account_pnl(&mut self, market: &Market) -> Option<f64> {
        use mtx_kit::venue::phoenix_rise::accounts::trader::TraderHeader;
        let data = mtx_kit::rpc_account(&self.trader_account).ok()??;
        let header = TraderHeader::try_read_from_account_bytes(&data).ok()?;
        let collateral = header.state().quote_lot_collateral.as_inner() as f64 / 1e6;
        let value = collateral + market.position.size * (market.mark - market.position.entry_usd);
        let initial = *self.initial_value.get_or_insert(value);
        Some(value - initial)
    }
}

impl Strategy for ShockTrader {
    fn start(&mut self, session: &Session) -> BotResult {
        self.drawdown_limit = session.max_drawdown;
        let venue = mtx_kit::venue::PhoenixVenue::from_json(&session.venue)?;
        self.trader_account = venue
            .trader_account(&mtx_kit::venue::key(&session.authority)?)
            .to_string();
        self.history = Some(features::Features::new(session.start_mark));
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let price = if market.oracle > 0.0 {
            market.oracle
        } else {
            market.mark
        };
        if !price.is_finite() || price <= 0.0 || !market.mark.is_finite() || market.mark <= 0.0 {
            return Ok(());
        }
        let cap = market.cap.min(70.0);
        if cap <= 0.0 {
            orders.target_position(0.0);
            return Ok(());
        }

        // A conservative fallback if a permitted account read is unavailable.
        self.estimated_costs +=
            (market.position.size - self.previous_position).abs() * price * 0.0012;
        let pnl = self
            .account_pnl(market)
            .unwrap_or(market.pnl - self.estimated_costs);
        self.peak_pnl = self.peak_pnl.max(pnl);
        let drawdown = (self.peak_pnl - pnl).max(market.drawdown);
        if drawdown > self.drawdown_limit * 0.6 {
            self.halted = true;
        }

        let history = self
            .history
            .get_or_insert_with(|| features::Features::new(price));
        let mut input = history.observe(price, market.k as usize, market.steps_left as usize);
        // Training inputs are IEEE float32; preserve their split decisions.
        for value in &mut input {
            *value = (*value as f32) as f64;
        }
        let fast = fast_model::predict(&input);
        let slow = slow_model::predict(&input);
        let forecast = (0.5 * fast + 0.5 * slow) * history.dir;

        // Forecasts are in percent. Penalize turnover to avoid repeatedly
        // paying the spread during wobbles, and weak exposure for its risk.
        let mut target = 0.0;
        let mut best_utility = f64::NEG_INFINITY;
        for candidate in [-cap, 0.0, cap] {
            let utility = candidate / cap * forecast
                - 0.015 * (candidate * candidate / (cap * cap))
                - 0.07 * (candidate - market.position.size).abs() / cap;
            if utility > best_utility {
                best_utility = utility;
                target = candidate;
            }
        }
        if self.halted {
            target = 0.0;
        }

        // Reserve room for another abrupt adverse move and the exit's costs.
        let risk_unit = market.mark * 0.05;
        let safe_size = ((self.drawdown_limit * 0.85 - drawdown) / risk_unit).clamp(0.0, cap);
        target = target.clamp(-safe_size, safe_size);
        if (target - market.position.size).abs() > 0.9 {
            orders.target_position_with(
                target,
                Execution {
                    slippage_bps: 200.0,
                    ..Execution::default()
                },
            );
        }
        log(&format!(
            "{} price {:.4} position {:.3} target {:.3} forecast {:.5} pnl {:.3} drawdown {:.3}",
            market.k, price, market.position.size, target, forecast, pnl, drawdown
        ));
        self.previous_position = market.position.size;
        Ok(())
    }
}

mtx_kit::export!(ShockTrader);
