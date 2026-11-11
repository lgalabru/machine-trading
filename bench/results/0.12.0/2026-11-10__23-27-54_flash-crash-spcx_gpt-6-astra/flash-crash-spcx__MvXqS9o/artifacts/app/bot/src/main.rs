use mtx_kit::prelude::*;
mod features;
mod model;
mod risk;

struct ShockBot {
    history: features::History,
    predictor: model::Model,
    slower: model::Model,
    base: f64,
    risk: risk::Risk,
    pending_target: f64,
    confirm_at: u64,
}
impl Default for ShockBot {
    fn default() -> Self {
        Self {
            history: features::History::default(),
            predictor: model::Model::from_bytes(include_bytes!("../models/model0.bin")),
            slower: model::Model::from_bytes(include_bytes!("../models/medium.bin")),
            base: 0.,
            risk: risk::Risk::default(),
            pending_target: 0.0,
            confirm_at: 0,
        }
    }
}
impl Strategy for ShockBot {
    async fn start(&mut self, session: &Session) -> BotResult {
        if session.env.symbol != "SPCX" {
            return Err("this strategy trades only SPCX".into());
        }
        Ok(())
    }
    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        if !(m.oracle.is_finite() && m.oracle > 0. && m.mark.is_finite() && m.mark > 0.) {
            self.risk.halted = true;
            orders.target_position(0.);
            return Ok(());
        }
        if self.base == 0. {
            self.base = m.mark;
        }
        let (x, orientation) = self
            .history
            .update(100. * (m.oracle / self.base - 1.), m.steps_left);
        let score = 0.5 * (self.predictor.predict(&x) + self.slower.predict(&x)) * orientation;
        let equity = if let Some(t) = orders.trader() {
            let read = t
                .inner
                .exchange()
                .hawkeye(&t.env.rpc_url, t.authority())
                .view_margin(t.authority(), 0, 0)
                .await;
            match read {
                Ok(view) => {
                    (view.value.collateral_quote_lots + view.value.unrealized_pnl_quote_lots) as f64
                        / 1e6
                }
                Err(error) => {
                    self.risk.halted = true;
                    orders.target_position_with(
                        0.,
                        Execution {
                            min_trade: 0.001,
                            ..Execution::default()
                        },
                    );
                    eprintln!("margin read failed; closing exposure: {error}");
                    return Ok(());
                }
            }
        } else {
            10000. + m.pnl
        };
        self.risk.observe(equity, m.max_drawdown);
        let dd = self.risk.drawdown;
        let mut target = m.position.size;
        if score > 0.07 {
            target = m.cap;
        } else if score < -0.07 {
            target = -m.cap;
        } else if m.position.size * score < m.position.size.abs() * (-0.05) {
            target = 0.;
        }
        let risk_cap = self.risk.cap(m.mark, m.cap, m.max_drawdown);
        target = target.clamp(-risk_cap, risk_cap).trunc();
        if self.risk.halted {
            target = 0.;
        }
        let execution = Execution {
            slippage_bps: if self.risk.halted { 200.0 } else { 80.0 },
            min_trade: 0.001,
            ..Execution::default()
        };
        if !m.last {
            // During a quiet pause, confirm a weaker forecast for two turns.
            // Close opposing exposure immediately. Fresh moves, strong signals,
            // and every risk reduction execute without this entry delay.
            let confirm = !self.risk.halted
                && target.abs() > 0.0
                && (target - m.position.size).abs() >= 5.0
                && (target * m.position.size <= 0.0 || target.abs() > m.position.size.abs() + 1.0)
                && x[6].abs() < 0.03
                && x[53] > 1.0
                && x[53] < 20.0
                && score.abs() < 0.20
                && m.steps_left > 5;
            if confirm {
                if self.pending_target == 0.0 || self.pending_target.signum() != target.signum() {
                    self.pending_target = target;
                    self.confirm_at = m.k + 2;
                }
                if m.k < self.confirm_at {
                    if target * m.position.size < 0.0 {
                        orders.target_position_with(0.0, execution);
                    }
                } else {
                    orders.target_position_with(target, execution);
                }
            } else {
                self.pending_target = 0.0;
                orders.target_position_with(target, execution);
            }
        }
        eprintln!(
            "k={} mark={:.5} oracle={:.5} pos={:.2} target={:.2} score={:.5} equity={:.3} dd={:.3}",
            m.k, m.mark, m.oracle, m.position.size, target, score, equity, dd
        );
        Ok(())
    }
}
fn main() -> BotResult {
    mtx_kit::run(ShockBot::default())
}
