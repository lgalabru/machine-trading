/// Tracks actual account equity, including fees and fills, independently of forecasts.
#[derive(Default)]
pub struct RiskGuard {
    peak: f64,
    halted: bool,
}
impl RiskGuard {
    pub fn target(
        &mut self,
        equity: f64,
        mark: f64,
        cap: f64,
        limit: f64,
        proposed: f64,
        liquidatable: bool,
    ) -> (f64, f64) {
        self.peak = self.peak.max(equity);
        let drawdown = (self.peak - equity).max(0.0);
        // Leave 15% of the limit for valuation differences and execution costs.
        // Size the remaining exposure against a 5% adverse move in one turn.
        let room = (0.85 * limit - drawdown).max(0.0);
        let allowed = (room / (mark * 0.05)).min(cap).floor();
        if drawdown > 0.70 * limit || liquidatable {
            self.halted = true;
        }
        (
            if self.halted {
                0.0
            } else {
                proposed.clamp(-allowed, allowed)
            },
            drawdown,
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reserves_room_for_an_adverse_jump() {
        let mut g = RiskGuard::default();
        assert_eq!(g.target(10000.0, 150.0, 70.0, 1000.0, 70.0, false).0, 70.0);
        let (p, dd) = g.target(9500.0, 150.0, 70.0, 1000.0, 70.0, false);
        assert!(dd + p.abs() * 150.0 * 0.05 <= 850.0);
        assert_eq!(p, 46.0);
    }
    #[test]
    fn halt_is_permanent_and_symmetric() {
        let mut g = RiskGuard::default();
        g.target(10000.0, 150.0, 70.0, 1000.0, -70.0, false);
        assert_eq!(g.target(9290.0, 150.0, 70.0, 1000.0, -70.0, false).0, 0.0);
        assert_eq!(g.target(11000.0, 150.0, 70.0, 1000.0, 70.0, false).0, 0.0);
    }
    #[test]
    fn cap_and_liquidation_state_override_forecasts() {
        let mut g = RiskGuard::default();
        assert_eq!(
            g.target(10000.0, 150.0, 20.0, 1000.0, -500.0, false).0,
            -20.0
        );
        assert_eq!(g.target(10000.0, 150.0, 20.0, 1000.0, 500.0, true).0, 0.0);
    }
}

#[cfg(test)]
mod shock_tests {
    use super::*;
    #[test]
    fn repeated_maximum_shocks_stay_inside_the_mandate() {
        for direction in [-1.0, 1.0] {
            let mut g = RiskGuard::default();
            let mut equity = 10000.0;
            let mut price = 150.0;
            let mut position = g
                .target(equity, price, 70.0, 1000.0, -direction * 70.0, false)
                .0;
            for _ in 0..8 {
                let next = price * (1.0 + direction * 0.038);
                equity += position * (next - price);
                let target = g
                    .target(equity, next, 70.0, 1000.0, -direction * 70.0, false)
                    .0;
                equity -= (position - target).abs() * next * 0.0015;
                assert!(10000.0 - equity < 1000.0);
                assert!(target.abs() <= 70.0);
                position = target;
                price = next;
            }
            assert_eq!(position, 0.0);
        }
    }
}
