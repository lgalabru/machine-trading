//! SPCX shock trader: causal price features, two embedded regression ensembles,
//! and an independent account-equity risk governor.
use mtx_kit::prelude::*;
mod features;
mod model;
use features::Features;
use model::Model;
struct ShockBot {
    features: Features,
    fast: Model,
    direction: Model,
    peak: f64,
    initial: Option<f64>,
    halted: bool,
}
impl Strategy for ShockBot {
    async fn step(&mut self, m: &Market, orders: &mut Orders<'_>) -> BotResult {
        if !(m.oracle.is_finite() && m.oracle > 0.0 && m.mark.is_finite() && m.mark > 0.0) {
            orders.target_position(0.0);
            return Ok(());
        }
        let equity = if let Some(t) = orders.trader() {
            let view = t
                .phoenix
                .hawkeye(t.authority())
                .view_margin(t.authority(), 0, 0)
                .await?
                .value;
            if view.is_liquidatable != 0 {
                self.halted = true;
            }
            (view.collateral_quote_lots + view.unrealized_pnl_quote_lots) as f64 / 1e6
        } else {
            10000.0 + m.pnl
        };
        let initial = *self.initial.get_or_insert(equity);
        self.peak = self.peak.max(equity);
        let dd = (self.peak - equity).max(0.0);
        if dd > m.max_drawdown * 0.60 {
            self.halted = true;
        }
        let prediction = if let Some((x, d)) = self.features.observe(m.k, m.oracle, m.steps_left) {
            (0.25 * self.fast.predict(&x) + 0.5 * self.direction.predict(&x)) * d
        } else {
            f64::NAN
        };
        let desired = desired_fraction(prediction, m.position.size);
        let risk_cap = risk_capacity(m.mark, m.cap, m.max_drawdown, dd);
        let mut target = (desired * m.cap).clamp(-risk_cap, risk_cap).round();
        if self.halted {
            target = 0.0;
        }
        if m.last {
            target = m.position.size;
        }
        if (target - m.position.size).abs() >= 1.0 {
            orders.target_position(target);
        }
        eprintln!(
            "OBS,{},{:.6},{:.6},{:.3},{:.6},{:.3},{:.3},{:.3},{:.0}",
            m.k,
            m.mark,
            m.oracle,
            m.position.size,
            m.position.entry_usd,
            equity - initial,
            dd,
            target,
            m.k - self.features.state.moved
        );
        Ok(())
    }
}
// Entry/exit hysteresis avoids paying the spread for weak changes of opinion.
fn desired_fraction(prediction: f64, position: f64) -> f64 {
    if prediction.is_nan() {
        0.0
    } else if prediction > 0.075 {
        1.0
    } else if prediction < -0.075 {
        -1.0
    } else if position.abs() > 0.5 && prediction * position.signum() > -0.05 {
        position.signum()
    } else {
        0.0
    }
}

// Reserve $150 for execution and valuation differences. Size for a 5% next-step
// adverse move, above the approximately 3.8% Phoenix step limit in this task.
fn risk_capacity(mark: f64, cap: f64, max_drawdown: f64, drawdown: f64) -> f64 {
    ((max_drawdown - drawdown - 150.0) / (mark * 0.05))
        .clamp(0.0, cap)
        .floor()
}

fn main() -> BotResult {
    mtx_kit::run(ShockBot {
        features: Features::default(),
        fast: Model::parse(include_str!("model0.txt")),
        direction: Model::parse(include_str!("model4.txt")),
        peak: 0.0,
        initial: None,
        halted: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_signals_do_not_choose_a_side_for_zero_inventory() {
        for position in [0.0, -0.0] {
            for prediction in [-0.074, -0.01, 0.0, 0.01, 0.074, f64::NAN] {
                assert_eq!(desired_fraction(prediction, position), 0.0);
            }
        }
    }

    #[test]
    fn exit_hysteresis_is_symmetric() {
        for side in [-1.0, 1.0] {
            assert_eq!(desired_fraction(0.04 * side, 70.0 * side), side);
            assert_eq!(desired_fraction(-0.04 * side, 70.0 * side), side);
            assert_eq!(desired_fraction(-0.06 * side, 70.0 * side), 0.0);
            assert_eq!(desired_fraction(-0.08 * side, 70.0 * side), -side);
        }
    }

    #[test]
    fn position_budget_survives_an_adverse_jump_and_expensive_reversal() {
        for price in (100..=200).step_by(5) {
            for drawdown in (0..=800).step_by(10) {
                let p = price as f64;
                let dd = drawdown as f64;
                let q = risk_capacity(p, 70.0, 1000.0, dd);
                assert!((0.0..=70.0).contains(&q));
                // Four percent against the whole new position, plus 20 bps
                // execution cost on a full reversal from the opposite cap.
                let stressed = dd + 0.04 * p * q + 0.002 * p * (70.0 + q);
                assert!(stressed < 1000.0, "price={p} drawdown={dd} size={q}");
            }
        }
    }
}
