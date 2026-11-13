//! Causal shock forecasts with position limits and a marked account equity guard.
mod features;
mod models;

use mtx_kit::{
    log, prelude::*, rpc_account,
    venue::{PhoenixVenue, phoenix_rise::accounts::owned::Trader},
};

#[derive(Default)]
struct ShockBot {
    trader: String,
    peak: f64,
    initial_mark: f64,
    steps: u64,
    history: features::Features,
    smoothed: f64,
    halted: bool,
}

impl Strategy for ShockBot {
    fn start(&mut self, session: &Session) -> BotResult {
        let venue = PhoenixVenue::from_json(&session.venue)?;
        self.trader = venue.trader_account(&session.authority.parse()?).to_string();
        self.peak = 10_000.0;
        self.initial_mark = session.start_mark;
        self.steps = session.steps;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let price = if market.oracle > 0.0 { market.oracle } else { market.mark };
        if !price.is_finite() || price <= 0.0 || !market.mark.is_finite() || market.mark <= 0.0 {
            orders.target_position(0.0);
            return Ok(());
        }

        let (features, direction) = self.history.update(price / self.initial_mark - 1.0, self.steps);
        let forecast = (0.75 * models::predict0(&features) as f64
            + 0.25 * models::predict1(&features) as f64) * 0.01 * direction;
        self.smoothed += (forecast - self.smoothed) * 0.5;

        // Wallet USDC is constant. Collateral plus virtual quote and the marked
        // position therefore has the same drawdown as total account value.
        let mut equity = f64::NAN;
        let mut drawdown = market.drawdown;
        if let Ok(Some(bytes)) = rpc_account(&self.trader) {
            if let Ok(account) = Trader::try_from_account_bytes(&bytes) {
                let virtual_quote: f64 = account.positions.entries.iter()
                    .filter(|(_, position)| position.base_lot_position.as_inner() != 0)
                    .map(|(_, position)| position.virtual_quote_lot_position.as_inner() as f64 / 1e6)
                    .sum();
                equity = account.state.quote_lot_collateral.as_inner() as f64 / 1e6
                    + virtual_quote + market.position.size * market.mark;
                self.peak = self.peak.max(equity);
                drawdown = self.peak - equity;
            }
        }

        // Unknown account value closes risk. The halt is permanent for this session.
        let drawdown_limit = market.max_drawdown.min(1000.0).max(0.0);
        if !equity.is_finite() || drawdown > 0.65 * drawdown_limit {
            self.halted = true;
        }
        // Reserve room for another abrupt move and execution costs.
        let size = market.cap.min(
            ((0.9 * drawdown_limit - drawdown) / (market.mark * 0.042)).max(0.0),
        );
        let mut target = market.position.size.clamp(-size, size);
        if self.smoothed > 0.001 {
            target = size;
        } else if self.smoothed < -0.001 {
            target = -size;
        } else if market.position.size * self.smoothed < 0.0 || self.smoothed.abs() < 0.0001 {
            target = 0.0;
        }
        if self.halted {
            target = 0.0;
        }
        if (target - market.position.size).abs() > 0.5 {
            orders.target_position(target);
        }
        log(&format!(
            "{} p {:.4} pos {:.2} target {:.2} forecast {:.6} equity {:.4} dd {:.2}",
            market.k, price, market.position.size, target, self.smoothed, equity, drawdown,
        ));
        Ok(())
    }
}

mtx_kit::export!(ShockBot);
