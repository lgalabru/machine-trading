//! Oracle-driven SPCX shock trading with native, embedded price forecasts.
//! All observations are causal; Phoenix account equity controls the risk budget.

use mtx_kit::prelude::*;
mod features;
mod model;
use model::Model;

const SHORT_WEIGHT: f64 = 12.0;
const ENTRY_THRESHOLD_BPS: f64 = 120.0;
const EQUITY_RESERVE_USD: f64 = 150.0;
const ADVERSE_STEP_RESERVE: f64 = 0.05;
const HALT_DRAWDOWN_FRACTION: f64 = 0.7;
const FINAL_EXIT_COST: f64 = 0.0008;
const MAX_SLIPPAGE_BPS: f64 = 50.0;

struct ShockBot {
    prices: Vec<f64>,
    initial: f64,
    short: Model,
    long: Model,
    peak: f64,
    halted: bool,
}
impl Default for ShockBot {
    fn default() -> Self {
        Self {
            prices: Vec::new(),
            initial: 0.0,
            short: Model::load(include_bytes!("h1.bin")),
            long: Model::load(include_bytes!("h4.bin")),
            peak: 0.0,
            halted: false,
        }
    }
}
impl Strategy for ShockBot {
    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        // At the final turn, compare marked inventory with an estimated exit value.
        if m.last {
            let value_of_exit = m.position.size.signum() * (m.oracle - m.mark) / m.oracle;
            // Close only when the mark/oracle basis pays a conservative exit cost.
            if value_of_exit > FINAL_EXIT_COST {
                orders.target_position_with(
                    0.0,
                    Execution {
                        slippage_bps: MAX_SLIPPAGE_BPS,
                        ..Execution::default()
                    },
                );
            }
            return Ok(());
        }
        if !(m.oracle.is_finite() && m.oracle > 0.0 && m.mark.is_finite() && m.mark > 0.0) {
            orders.target_position_with(
                0.0,
                Execution {
                    slippage_bps: MAX_SLIPPAGE_BPS,
                    ..Execution::default()
                },
            );
            return Ok(());
        }
        // The run-loop PnL omits costs. Use the venue's actual margin equity.
        // Wallet USDC remains constant, so it does not change drawdown.
        let mut equity = m.pnl;
        if let Some(t) = orders.trader() {
            match t
                .phoenix
                .hawkeye(t.authority())
                .view_margin_for_trader(t.account)
                .await
            {
                Ok(v) => {
                    equity = (v.value.collateral_quote_lots + v.value.unrealized_pnl_quote_lots)
                        as f64
                        / 1e6
                        - 10000.0
                }
                Err(_) => {
                    orders.target_position_with(
                        0.0,
                        Execution {
                            slippage_bps: MAX_SLIPPAGE_BPS,
                            ..Execution::default()
                        },
                    );
                    return Ok(());
                }
            }
        }
        self.peak = self.peak.max(equity);
        let dd = (self.peak - equity).max(0.0);
        if dd >= m.max_drawdown * HALT_DRAWDOWN_FRACTION {
            self.halted = true;
        }
        if self.initial == 0.0 {
            self.initial = m.oracle;
        }
        let p = (m.oracle / self.initial - 1.0) * 10000.0;
        while self.prices.len() < (m.k as usize) {
            self.prices.push(self.prices.last().copied().unwrap_or(p));
        }
        self.prices.push(p);
        let (f, direction) = features::extract(&self.prices, m.steps_left);
        let short = self.short.predict(&f) * direction;
        let long = self.long.predict(&f) * direction;
        let score = SHORT_WEIGHT * short + long;
        let mut target = 0.0;
        if score.abs() >= ENTRY_THRESHOLD_BPS {
            target = score.signum() * m.cap;
        } else if m.position.size * score > 0.0 {
            target = m.position.size;
        }
        // Reserve enough equity for another adverse jump and execution costs.
        let risk_cap = ((m.max_drawdown - dd - EQUITY_RESERVE_USD).max(0.0)
            / (m.mark * ADVERSE_STEP_RESERVE))
            .min(m.cap)
            .floor();
        target = target.clamp(-risk_cap, risk_cap);
        if self.halted {
            target = 0.0;
        }
        if (target - m.position.size).abs() >= 1.0 {
            orders.target_position_with(
                target,
                Execution {
                    slippage_bps: MAX_SLIPPAGE_BPS,
                    ..Execution::default()
                },
            );
        }
        eprintln!(
            "k={} mark={:.5} oracle={:.5} pos={:.2} eq={:.3} dd={:.3} target={:.2} short={:.3} long={:.3}",
            m.k, m.mark, m.oracle, m.position.size, equity, dd, target, short, long
        );
        Ok(())
    }
}
fn main() -> BotResult {
    mtx_kit::run(ShockBot::default())
}
