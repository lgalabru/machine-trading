//! Drawdown control uses the account's actual margin equity, including execution costs.
#[derive(Default)]
pub struct Risk {
    peak: f64,
    halted: bool,
}
#[derive(Debug)]
pub struct Decision {
    pub target: f64,
    pub drawdown: f64,
}
impl Risk {
    pub fn assess(
        &mut self,
        equity: f64,
        mark: f64,
        cap: f64,
        limit: f64,
        fraction: f64,
        liquidatable: bool,
    ) -> Decision {
        self.peak = self.peak.max(equity);
        let drawdown = (self.peak - equity).max(0.0);
        if drawdown > 0.49 * limit || liquidatable {
            self.halted = true;
        }
        // Keep cash headroom for a jump, fees, and the change in the mark caused by fills.
        let cash_room = (limit - drawdown - 0.10 * limit).max(0.0);
        let units = (cash_room / (mark * 0.045)).clamp(0.0, cap);
        let target = if self.halted {
            0.0
        } else {
            (fraction.clamp(-1.0, 1.0) * units).trunc()
        };
        Decision { target, drawdown }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn an_adverse_jump_and_unwind_leave_drawdown_headroom() {
        for dd in (0..=490).step_by(5) {
            for mark in (75..=250).step_by(5) {
                for side in [-1.0, 1.0] {
                    let mut risk = Risk::default();
                    risk.assess(10000.0, mark as f64, 70.0, 1000.0, 0.0, false);
                    let decision =
                        risk.assess(10000.0 - dd as f64, mark as f64, 70.0, 1000.0, side, false);
                    assert!(decision.target.abs() <= 70.0);
                    // A 4% gap is larger than a scenario's usual maximum step.
                    // Add 20 bps of liquidation friction and $25 of valuation uncertainty.
                    let projected = dd as f64 + decision.target.abs() * mark as f64 * 0.042 + 25.0;
                    assert!(projected < 1000.0, "projected drawdown {projected}");
                }
            }
        }
    }
    #[test]
    fn a_halt_survives_recovery_and_the_peak_is_remembered() {
        let mut risk = Risk::default();
        risk.assess(10000.0, 150.0, 70.0, 1000.0, 1.0, false);
        risk.assess(10500.0, 150.0, 70.0, 1000.0, 1.0, false);
        let d = risk.assess(9990.0, 150.0, 70.0, 1000.0, 1.0, false);
        assert_eq!(d.drawdown, 510.0);
        assert_eq!(d.target, 0.0);
        assert_eq!(
            risk.assess(10600.0, 150.0, 70.0, 1000.0, -1.0, false)
                .target,
            0.0
        );
    }
    #[test]
    fn a_liquidatable_account_only_reduces_exposure() {
        let mut risk = Risk::default();
        assert_eq!(
            risk.assess(10000.0, 150.0, 70.0, 1000.0, 1.0, true).target,
            0.0
        );
    }
}
