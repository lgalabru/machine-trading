//! Hard risk controls run after the price signal, independent of its confidence.
use mtx_kit::prelude::Market;

#[derive(Default)]
pub struct Risk {
    pub peak: f64,
    pub halted: bool,
}
impl Risk {
    pub fn target(&mut self, market: &Market, equity: f64, desired: f64) -> f64 {
        if !equity.is_finite() || !desired.is_finite() || market.mark <= 0.0 || !market.mark.is_finite() {
            self.halted = true;
            return 0.0;
        }
        // The wallet's 100 USDC never changes, so it cancels out of drawdown.
        self.peak = self.peak.max(10_000.0).max(equity);
        let drawdown = self.peak - equity;
        if drawdown > market.max_drawdown * 0.65 {
            self.halted = true;
        }
        // Reserve 15% of the loss limit for execution and valuation differences.
        // The remaining position must withstand a further 5.5% adverse move.
        let safe = ((market.max_drawdown * 0.85 - drawdown) / (market.mark * 0.055))
            .max(0.0).min(market.cap).floor();
        let mut target = desired.clamp(-safe, safe);
        // Avoid tiny replenishments, but always reduce when the risk cap shrinks.
        if (target - market.position.size).abs() < 5.0 && market.position.size.abs() <= safe {
            target = market.position.size;
        }
        if self.halted { target = 0.0; }
        // Final valuation is at the mark; closing here only pays execution costs.
        if market.last { target = market.position.size; }
        target
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn market() -> Market {
        Market { mark: 143.0, cap: 70.0, max_drawdown: 1000.0, ..Market::default() }
    }
    #[test]
    fn enforces_cap_and_permanent_halt() {
        let mut r = Risk::default();
        let m = market();
        assert_eq!(r.target(&m, 10000.0, 1000.0), 70.0);
        assert_eq!(r.target(&m, 10000.0, -1000.0), -70.0);
        assert!(r.target(&m, 9400.0, 70.0) < 35.0);
        assert_eq!(r.target(&m, 9349.0, 70.0), 0.0);
        assert_eq!(r.target(&m, 11000.0, 70.0), 0.0);
    }
    #[test]
    fn consecutive_adverse_jumps_stay_inside_loss_limit() {
        for direction in [-1.0, 1.0] {
            let mut r = Risk::default();
            let mut m = market();
            let mut equity = 10000.0;
            let mut previous_position = 0.0;
            for _ in 0..20 {
                m.position.size = previous_position;
                let target = r.target(&m, equity, direction * 70.0);
                // Stress execution at 50 bps per share, then a 4% adverse jump.
                equity -= (target - previous_position).abs() * m.mark * 0.005;
                let next_price = m.mark * (1.0 - direction * 0.04);
                equity += target * (next_price - m.mark);
                assert!(10000.0 - equity < m.max_drawdown);
                assert!(target.abs() <= m.cap);
                previous_position = target;
                m.mark = next_price;
            }
            assert!(r.halted);
            assert_eq!(previous_position, 0.0);
        }
    }
}
