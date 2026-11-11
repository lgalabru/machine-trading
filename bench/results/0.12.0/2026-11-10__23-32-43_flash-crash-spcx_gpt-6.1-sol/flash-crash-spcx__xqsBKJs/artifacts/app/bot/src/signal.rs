//! Sign-symmetric features of the oracle, including persistent multi-leg shocks.
#[derive(Default)]
pub struct Signal {
    history: Vec<f64>,
    direction: f64,
    anchor: f64,
    extreme: f64,
    event: usize,
    last_move: usize,
    latest: f64,
    last_jump: f64,
    noise_age: usize,
    legs: usize,
    last_primary: usize,
    leg_origin: f64,
}
impl Signal {
    pub fn update(&mut self, p: f64, left: u64) -> Option<([f64; 33], f64)> {
        let k = self.history.len();
        let last = self.history.last().copied().unwrap_or(p);
        self.history.push(p);
        let change = p / last - 1.;
        let window_change = p / self.history[k.saturating_sub(4)] - 1.;
        if change != 0. {
            self.noise_age = k;
        }
        if self.direction == 0. {
            if change.abs() > 0.002 || window_change.abs() > 0.0035 {
                if change.abs() > 0.002 {
                    self.direction = change.signum();
                    self.anchor = last;
                } else {
                    self.direction = window_change.signum();
                    self.anchor = self.history[k.saturating_sub(4)];
                }
                self.latest = self.direction;
                self.extreme = p;
                self.event = k;
                self.last_move = k;
                self.last_jump = change;
                self.legs = 1;
                self.last_primary = k;
                self.leg_origin = self.anchor;
            }
        } else {
            if change.abs() > 0.002 {
                self.last_move = k;
                self.latest = change.signum();
                self.last_jump = change;
                if self.latest == self.direction {
                    if k - self.last_primary > 12 {
                        self.legs += 1;
                        self.leg_origin = last;
                    }
                    self.last_primary = k;
                }
            }
            if (p - self.extreme) * self.direction > 0. {
                self.extreme = p;
            }
        }
        if self.direction == 0. {
            return None;
        }
        let d = self.direction;
        let displacement = (self.extreme / self.anchor - 1.).abs();
        let recovery = (self.extreme - p) * d / self.anchor;
        let age = k - self.last_move;
        let mut f = [0.; 33];
        f[..10].copy_from_slice(&[
            displacement * 100.,
            (p / self.anchor - 1.) * d * 100.,
            recovery / displacement.max(0.001),
            age as f64,
            (k - self.event) as f64,
            self.last_jump * d * 100.,
            self.latest * d,
            k as f64,
            left as f64,
            (k - self.noise_age) as f64,
        ]);
        for (i, lag) in [1, 2, 3, 4, 6, 10, 16, 32, 64, 100].iter().enumerate() {
            f[10 + i] = (p / self.history[k.saturating_sub(*lag)] - 1.) * d * 100.;
        }
        for (i, window) in [6, 16, 40, 100].iter().enumerate() {
            let a = &self.history[(k + 1).saturating_sub(*window)..];
            let low = a.iter().copied().fold(f64::INFINITY, f64::min);
            let high = a.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            f[20 + 2 * i] = (high - low) / p * 100.;
            f[21 + 2 * i] = if high > low {
                (p - low) / (high - low)
            } else {
                0.5
            };
            if d < 0. {
                f[21 + 2 * i] = 1. - f[21 + 2 * i];
            }
        }
        f[28] = (p / self.history[0] - 1.) * d * 100.;
        f[29] = self.legs as f64;
        f[30] = (k - self.last_primary) as f64;
        f[31] = (p / self.leg_origin - 1.) * d * 100.;
        f[32] = window_change * d * 100.;
        if age >= 3 && (recovery > 0.0014_f64.max(displacement * 0.35) || age > 100) {
            self.direction = 0.;
        }
        Some((f.map(|v| v as f32 as f64), d))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    include!("signal_fixtures.rs");
    #[test]
    fn features_match_training_paths() {
        for (path, expected) in SIGNAL_FIXTURES {
            let mut s = Signal::default();
            let mut at = 0;
            for (k, &p) in path.iter().enumerate() {
                let out = s.update(p, (path.len() - k - 1) as u64);
                if at < expected.len() && expected[at].0 == k {
                    let (x, d) = out.expect("missing event");
                    assert_eq!(d, expected[at].2);
                    for i in 0..33 {
                        assert!(
                            (x[i] - expected[at].1[i]).abs() < 1e-7,
                            "step {k}, feature {i}: {} vs {}",
                            x[i],
                            expected[at].1[i]
                        );
                    }
                    at += 1;
                } else {
                    assert!(out.is_none(), "unexpected event at {k}");
                }
            }
            assert_eq!(at, expected.len());
        }
    }
    #[test]
    fn flat_market_has_no_signal() {
        let mut s = Signal::default();
        for _ in 0..500 {
            assert!(s.update(143., 50).is_none());
        }
    }
}
