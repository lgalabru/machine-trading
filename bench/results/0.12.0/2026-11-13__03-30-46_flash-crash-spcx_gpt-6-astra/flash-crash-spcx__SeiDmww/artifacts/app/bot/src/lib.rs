//! SPCX shock strategy. Forecasts use oracle history; risk uses account equity
//! at the venue's mark. The component needs no files, network, or runtime model.

use mtx_kit::{log, prelude::*};

mod features;
mod forest;
mod risk;

#[derive(Default)]
struct ShockBot {
    features: features::Features,
    risk: risk::Risk,
    initial: f64,
    steps: u64,
}

impl Strategy for ShockBot {
    fn start(&mut self, session: &Session) -> BotResult {
        self.initial = session.start_mark;
        self.steps = session.steps;
        self.risk.start(session);
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let cap = self.risk.limit(market);
        // There is no future price move to earn back a final-step trading fee.
        if market.last {
            return Ok(());
        }

        let level = (market.oracle / self.initial - 1.0) * 100.0;
        let history = self.features.next(level, self.steps.saturating_sub(market.k + 1));
        let (canonical, direction) = features::canonical(&history);
        let short = forest::predict_one(&canonical, 0) * direction;
        let medium = forest::predict_one(&canonical, 1) * direction;
        let signal = short + 0.1 * medium;

        // Predictions are percentage points. Hysteresis avoids paying the
        // spread repeatedly when a forecast fluctuates near zero.
        let target = if signal > 0.10 {
            cap
        } else if signal < -0.10 {
            -cap
        } else if market.position.size > 0.0 && signal > -0.08 {
            cap
        } else if market.position.size < 0.0 && signal < 0.08 {
            -cap
        } else {
            0.0
        };
        orders.target_position_with(
            target,
            Execution { slippage_bps: 40.0, ..Execution::default() },
        );
        log(&format!(
            "k={} mark={:.4} oracle={:.4} pos={:.2} forecast={:.4},{:.4} target={:.2} equity={:.2} dd={:.2}",
            market.k, market.mark, market.oracle, market.position.size,
            short, medium, target, self.risk.equity, self.risk.drawdown
        ));
        Ok(())
    }
}

mtx_kit::export!(ShockBot);

#[cfg(test)]
mod tests;
