//! Causal, direction-normalized features for recent impulses and the session's
//! earlier extremes. Feature order is fixed by the embedded model tables.
#[path = "feature_policy.rs"]
mod policy;
use policy::{Params, Policy};
pub const NF: usize = 30;
#[derive(Default)]
pub struct Features {
    pub state: Policy,
    pub prices: Vec<f64>,
    pub events: f64,
    pub ema: f64,
    pub prior_dir: f64,
    pub high: f64,
    pub low: f64,
    pub high_at: u64,
    pub low_at: u64,
    pub biggest: f64,
    pub biggest_at: u64,
    pub jump: f64,
    pub jump_at: u64,
    pub previous_jump: f64,
    pub gap: f64,
    pub variation: f64,
}
impl Features {
    pub fn observe(&mut self, k: u64, p: f64, left: u64) -> Option<([f32; NF], f64)> {
        if self.prices.is_empty() {
            self.high = p;
            self.low = p;
        } else {
            if p > self.high {
                self.high = p;
                self.high_at = k;
            }
            if p < self.low {
                self.low = p;
                self.low_at = k;
            }
            let change = (p / self.prices[self.prices.len() - 1] - 1.0) * 100.0;
            if change.abs() > self.biggest.abs() {
                self.biggest = change;
                self.biggest_at = k;
            }
            if change.abs() > 0.3 {
                self.previous_jump = self.jump;
                self.gap = (k - self.jump_at) as f64 * 0.5;
                self.jump = change;
                self.jump_at = k;
                self.variation += change.abs();
            }
        }
        let par = Params {
            trigger: 0.003,
            drift: 0.006,
            refresh: 0.002,
            momentum_steps: 4,
            momentum_size: 1.0,
            fade_size: 1.0,
            recovery: 1.1,
            expiry: 140,
            cooldown: 0,
        };
        let was_active = self.state.active;
        let old_dir = self.state.direction;
        self.state.target(k, p, &par);
        if !was_active && self.state.active {
            self.events += 1.0;
            self.prior_dir = old_dir;
        }
        self.prices.push(p);
        if self.ema == 0.0 {
            self.ema = p;
        } else {
            self.ema += 0.03 * (p - self.ema);
        }
        if !self.state.active {
            return None;
        }
        let s = &self.state;
        let d = s.direction;
        let n = self.prices.len();
        let ret = |t: usize| d * (p / self.prices[n.saturating_sub(t + 1)] - 1.0) * 100.0;
        let amp = (s.extreme - s.origin).abs() / s.origin;
        let retr = (s.extreme - p) * d / s.origin;
        let mean_abs = (n.saturating_sub(16).max(1)..n)
            .map(|i| (self.prices[i] / self.prices[i - 1] - 1.0).abs() * 100.0)
            .sum::<f64>()
            / 16.0;
        let max_recent = self.prices[n.saturating_sub(16)..]
            .iter()
            .map(|x| (*x - p) * d / p * 100.0)
            .fold(0.0, f64::max);
        let min_recent = self.prices[n.saturating_sub(16)..]
            .iter()
            .map(|x| (*x - p) * d / p * 100.0)
            .fold(0.0, f64::min);
        let features = [
            k as f64 * 0.5,
            left as f64 * 0.5,
            amp * 100.0,
            retr * 100.0,
            retr / amp.max(0.001),
            (k - s.moved) as f64 * 0.5,
            d * (p / self.prices[0] - 1.0) * 100.0,
            ret(1),
            ret(2),
            ret(4),
            ret(8),
            ret(16),
            ret(32),
            max_recent,
            min_recent,
            self.events,
            d * self.prior_dir,
            s.moved as f64 * 0.5,
            mean_abs,
            d * (p / self.ema - 1.0) * 100.0,
            if d > 0.0 {
                (self.high / p - 1.0) * 100.0
            } else {
                (p / self.low - 1.0) * 100.0
            },
            if d > 0.0 {
                (p / self.low - 1.0) * 100.0
            } else {
                (self.high / p - 1.0) * 100.0
            },
            (k - if d > 0.0 { self.high_at } else { self.low_at }) as f64 * 0.5,
            self.biggest * d,
            (k - self.biggest_at) as f64 * 0.5,
            self.jump * d,
            (k - self.jump_at) as f64 * 0.5,
            self.previous_jump * d,
            self.gap,
            self.variation,
        ];
        Some((features.map(|x| x as f32), d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_flat_market_does_not_start_an_event() {
        let mut f = Features::default();
        for k in 0..360 {
            assert!(f.observe(k, 143.11, 360 - k).is_none());
        }
    }
    #[test]
    fn shocks_on_either_side_produce_finite_oriented_features() {
        for side in [-1.0, 1.0] {
            let mut f = Features::default();
            assert!(f.observe(0, 143.11, 300).is_none());
            let (x, d) = f.observe(1, 143.11 * (1.0 + 0.03 * side), 299).unwrap();
            assert_eq!(d, side);
            assert!(x.iter().all(|v| v.is_finite()));
            assert!(x[2] > 2.9 && x[7] > 2.9);
        }
    }
}
