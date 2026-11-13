//! Position sizing reserves cash for a further adverse jump and exit costs.
#[derive(Default)]
pub struct Risk {
    peak: f64,
    pub drawdown: f64,
    pub halted: bool,
}
impl Risk {
    pub fn size(&mut self, pnl: f64, mark: f64, cap: f64, limit: f64) -> f64 {
        if !pnl.is_finite()
            || !mark.is_finite()
            || mark <= 0.0
            || !cap.is_finite()
            || cap < 0.0
            || !limit.is_finite()
            || limit <= 0.0
        {
            self.halted = true;
            return 0.0;
        }
        self.peak = self.peak.max(pnl);
        self.drawdown = self.peak - pnl;
        if self.drawdown >= limit * 0.70 {
            self.halted = true;
        }
        if self.halted {
            return 0.0;
        }
        let available = (limit * 0.90 - self.drawdown).max(0.0);
        cap.min(70.0).min(available / (mark * 0.055))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn jump_and_execution_reserve() {
        for price in [80.0, 143.11, 200.0, 400.0] {
            for loss in 0..1000 {
                let mut r = Risk::default();
                let size = r.size(-(loss as f64), price, 70.0, 1000.0);
                assert!((0.0..=70.0).contains(&size));
                if !r.halted {
                    // 3.8% maximum scenario jump, with $40 execution allowance.
                    assert!((loss as f64) + size * price * 0.038 + 40.0 < 1000.0);
                }
            }
        }
    }
    #[test]
    fn peak_drawdown_and_halt_are_persistent() {
        let mut r = Risk::default();
        assert_eq!(r.size(500.0, 143.11, 70.0, 1000.0), 70.0);
        assert!(r.size(100.0, 143.11, 70.0, 1000.0) < 70.0);
        assert_eq!(r.size(-201.0, 143.11, 70.0, 1000.0), 0.0);
        assert_eq!(r.size(600.0, 143.11, 70.0, 1000.0), 0.0);
        assert!(r.halted);
    }
    #[test]
    fn respects_smaller_mandates_and_invalid_inputs() {
        assert_eq!(Risk::default().size(0., 143.11, 20., 1000.), 20.);
        assert_eq!(Risk::default().size(f64::NAN, 143.11, 70., 1000.), 0.);
    }
}
