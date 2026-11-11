//! Exposure limits use actual collateral plus unrealized PnL, including fees.
//! The reserve covers execution costs, changes in the mark during a turn,
//! and rounding. The jump allowance exceeds a normal single replay step.
#[derive(Debug)]
pub struct Risk {
    peak: f64,
    pub halted: bool,
    pub drawdown: f64,
}
impl Default for Risk {
    fn default() -> Self {
        Self {
            peak: 10_000.0,
            halted: false,
            drawdown: 0.0,
        }
    }
}
impl Risk {
    pub fn observe(&mut self, equity: f64, limit: f64) {
        if !equity.is_finite() || limit <= 0.0 {
            self.halted = true;
            return;
        }
        self.peak = self.peak.max(equity);
        self.drawdown = (self.peak - equity).max(0.0);
        if self.drawdown > 0.65 * limit {
            self.halted = true;
        }
    }
    pub fn cap(&self, mark: f64, position_cap: f64, loss_limit: f64) -> f64 {
        if self.halted || !mark.is_finite() || mark <= 0.0 {
            return 0.0;
        }
        let reserve = 0.18 * loss_limit;
        ((loss_limit - self.drawdown - reserve).max(0.0) / (mark * 0.055))
            .min(position_cap)
            .max(0.0)
            .floor()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gap_and_execution_reserve_fit_inside_the_loss_limit() {
        for peak_gain in [0.0, 500.0, 2500.0] {
            for loss in 0..1000 {
                for mark in [90.0, 143.11, 210.0] {
                    let mut risk = Risk::default();
                    risk.observe(10_000.0 + peak_gain, 1000.0);
                    risk.observe(10_000.0 + peak_gain - loss as f64, 1000.0);
                    let position = risk.cap(mark, 70.0, 1000.0);
                    assert!(position <= 70.0 && position >= 0.0);
                    if position > 0.0 {
                        let next_loss = loss as f64 + position * mark * 0.055 + 100.0;
                        assert!(next_loss < 1000.0);
                    }
                }
            }
        }
    }
    #[test]
    fn a_halt_is_permanent_even_if_the_account_recovers() {
        let mut risk = Risk::default();
        risk.observe(10_500.0, 1000.0);
        risk.observe(9_800.0, 1000.0);
        assert_eq!(risk.cap(140.0, 70.0, 1000.0), 0.0);
        risk.observe(11_000.0, 1000.0);
        assert_eq!(risk.cap(140.0, 70.0, 1000.0), 0.0);
    }
    #[test]
    fn repeated_adverse_gaps_force_an_exit_before_the_mandate() {
        let mut risk = Risk::default();
        let mut equity = 10_000.0;
        let mut mark = 143.11;
        for _ in 0..30 {
            risk.observe(equity, 1000.0);
            let size = risk.cap(mark, 70.0, 1000.0);
            equity -= size * mark * 0.04 + size * mark * 0.001;
            mark *= 1.04;
            assert!(10_000.0 - equity < 1000.0);
        }
        assert!(risk.halted);
    }
}
