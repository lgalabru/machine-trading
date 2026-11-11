mod config;
mod features;
mod model;

use features::Features;
use model::Model;
use mtx_kit::prelude::*;

struct ShockBot {
    features: Features,
    models: Vec<(Model, f64)>,
    peak_equity: f64,
    halted: bool,
}

impl Default for ShockBot {
    fn default() -> Self {
        Self {
            features: Features::default(),
            models: config::MODELS.iter().map(|(bytes, weight)| (Model::load(bytes), *weight)).collect(),
            peak_equity: 0.0,
            halted: false,
        }
    }
}

impl Strategy for ShockBot {
    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        if !(market.oracle > 0.0 && market.mark > 0.0) {
            return Ok(());
        }
        let (features, direction) = self.features.update(
            market.k as usize,
            market.oracle,
            market.steps_left as usize,
        );
        let weight: f64 = self.models.iter().map(|(_, w)| w).sum();
        let signal = direction * self.models.iter()
            .map(|(model, w)| model.predict(&features) * w).sum::<f64>() / weight;

        // Unlike Market::pnl, this includes execution costs, realized PnL,
        // and funding. Wallet USDC stays constant throughout the strategy.
        let equity = if let Some(trader) = orders.trader() {
            match trader.phoenix.hawkeye(trader.authority())
                .view_margin_for_trader(trader.account).await {
                Ok(margin) => 100.0 + (margin.value.collateral_quote_lots
                    + margin.value.unrealized_pnl_quote_lots) as f64 / 1e6,
                Err(error) => {
                    self.halted = true;
                    orders.target_position(0.0);
                    eprintln!("account read failed; flattening: {error}");
                    return Ok(());
                }
            }
        } else {
            10100.0 + market.pnl
        };
        self.peak_equity = self.peak_equity.max(equity);
        let drawdown = self.peak_equity - equity;
        if drawdown > 0.55 * market.max_drawdown {
            self.halted = true;
        }

        // Separate entry and hold thresholds avoid paying for repeated
        // flips when the forecast is close to zero.
        let mut target = if signal > config::ENTRY_THRESHOLD {
            market.cap
        } else if signal < -config::ENTRY_THRESHOLD {
            -market.cap
        } else if market.position.size > 0.0 && signal > config::HOLD_THRESHOLD {
            market.cap
        } else if market.position.size < 0.0 && signal < -config::HOLD_THRESHOLD {
            -market.cap
        } else {
            0.0
        };

        // Reserve headroom for another abrupt move before the next turn.
        // Exposure shrinks as drawdown grows, before the permanent halt.
        let risk_cap = ((0.90 * market.max_drawdown - drawdown).max(0.0)
            / (market.mark * 0.055)).min(market.cap).floor();
        target = target.clamp(-risk_cap, risk_cap);
        if self.halted {
            target = 0.0;
        }
        // The final mark has already arrived. Avoid an unnecessary exit fee.
        if !market.last {
            orders.target_position_with(target, Execution {
                slippage_bps: if self.halted { 200.0 } else { 60.0 },
                ..Execution::default()
            });
        }
        eprintln!("TRACE {} {:.6} {:.6} {:.3} {:.3} {:.4} {:.3} {} {:.7}",
            market.k, market.mark, market.oracle, market.position.size,
            target, equity, drawdown, self.halted, signal);
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(ShockBot::default())
}
