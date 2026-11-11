//! SPCX shock trading with embedded forecasts and an independent equity guard.
//! All market access and execution use the turn-based mtx-kit run loop.

mod features;
mod model;
use features::Features;
use model::Model;
use mtx_kit::prelude::*;
use tracing::{info, warn};

// Forecasts are in percent, so 0.18 means eighteen basis points.
const ENTRY_EDGE_PCT: f64 = 0.18;
const EXIT_EDGE_PCT: f64 = 0.08;
const DRAWDOWN_HALT: f64 = 0.50;
const ADVERSE_JUMP: f64 = 0.045;
const CASH_RESERVE: f64 = 80.0;

struct ShockBot {
    desired: f64,
    features: Features,
    models: [Model; 2],
    urgent: Model,
    halted: bool,
    peak: f64,
    equity: f64,
}
impl Default for ShockBot {
    fn default() -> Self {
        Self {
            desired: 0.,
            urgent: Model::load(include_bytes!("urgent.bin")),
            features: Features::default(),
            models: [
                Model::load(include_bytes!("model6.bin")),
                Model::load(include_bytes!("model12.bin")),
            ],
            halted: false,
            peak: 0.,
            equity: 0.,
        }
    }
}
impl Strategy for ShockBot {
    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        if !(m.oracle.is_finite() && m.oracle > 0. && m.mark.is_finite() && m.mark > 0.) {
            orders.target_position(0.);
            return Ok(());
        }
        let mut dd = m.drawdown;
        if let Some(t) = orders.trader() {
            match t
                .phoenix
                .hawkeye(t.authority())
                .view_margin(t.authority(), 0, 0)
                .await
            {
                Ok(v) => {
                    let v = v.value;
                    self.equity = (v.collateral_quote_lots + v.unrealized_pnl_quote_lots
                        - v.unsettled_funding_quote_lots) as f64
                        / 1e6;
                    self.peak = self.peak.max(self.equity);
                    dd = (self.peak - self.equity).max(0.);
                    if v.is_liquidatable != 0 {
                        self.halted = true;
                    }
                }
                Err(e) => {
                    warn!(%e,"risk read failed; flattening");
                    self.halted = true;
                }
            }
        }
        let (d, f) = self.features.observe(m.oracle, m.steps_left as usize);
        let short = self.models[0].predict(&f);
        let medium = self.models[1].predict(&f);
        let score = d * (short + medium) * 0.5;
        let urgency_forecast = d * self.urgent.predict(&f);
        let q = m.position.size;
        let mode = self.desired;
        let mut target = mode;
        if mode >= 0. {
            if score < -ENTRY_EDGE_PCT {
                target = -m.cap;
            } else if mode > 0. && score < -EXIT_EDGE_PCT {
                target = 0.;
            } else if mode == 0. && score > ENTRY_EDGE_PCT {
                target = m.cap;
            }
        } else if score > ENTRY_EDGE_PCT {
            target = m.cap;
        } else if score > EXIT_EDGE_PCT {
            target = 0.;
        }
        self.desired = target;
        if dd > m.max_drawdown * DRAWDOWN_HALT {
            self.halted = true;
        }
        let safe = ((m.max_drawdown - dd - CASH_RESERVE).max(0.) / (m.mark * ADVERSE_JUMP))
            .floor()
            .min(m.cap);
        target = target.clamp(-safe, safe);
        if self.halted {
            target = 0.;
        }
        if m.last {
            target = if self.halted { 0. } else { q };
        }
        if (target - q).abs() >= 1. {
            orders.target_position_with(
                target,
                Execution {
                    max_per_step: if score.abs() >= 0.6
                        || urgency_forecast * (target - q).signum() > 0.08
                        || dd > 300.
                        || self.halted
                    {
                        f64::INFINITY
                    } else {
                        35.
                    },
                    slippage_bps: 300.,
                    ..Execution::default()
                },
            );
        }
        info!(
            step = m.k,
            mark = m.mark,
            oracle = m.oracle,
            position = q,
            target,
            score,
            short,
            medium,
            dd,
            equity = self.equity,
            halted = self.halted,
            "decision"
        );
        Ok(())
    }
}
fn main() -> BotResult {
    if std::env::args().any(|a| a == "--self-test") {
        let b = ShockBot::default();
        for row in include_bytes!("fixture.bin").chunks_exact(93 * 8) {
            let values: Vec<f64> = row
                .chunks_exact(8)
                .map(|x| f64::from_le_bytes(x.try_into().unwrap()))
                .collect();
            for i in 0..3 {
                let model = if i < 2 { &b.models[i] } else { &b.urgent };
                assert!((model.predict(&values[..90]) - values[90 + i]).abs() < 1e-10);
            }
        }
        println!("shock-forecast-v2-urgency: 90 features, 1300 trees; predictor check passed");
        return Ok(());
    }
    mtx_kit::run(ShockBot::default())
}
