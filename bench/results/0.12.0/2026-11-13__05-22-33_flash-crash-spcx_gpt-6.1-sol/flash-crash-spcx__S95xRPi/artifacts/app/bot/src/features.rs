pub const NF: usize = 96;
#[derive(Clone)]
pub struct Features {
    prices: Vec<f64>,
    start: f64,
    pub dir: f64,
    ema: [f64; 6],
    last: [(f64, usize); 5],
    previous: [(f64, usize); 5],
    jumps: Vec<(f64, usize)>,
}
impl Features {
    pub fn new(start: f64) -> Self {
        Self {
            prices: Vec::new(),
            start,
            dir: 1.0,
            ema: [start; 6],
            last: [(0., 0); 5],
            previous: [(0., 0); 5],
            jumps: Vec::new(),
        }
    }
    pub fn observe(&mut self, p: f64, k: usize, left: usize) -> [f64; NF] {
        let prev = self.prices.last().copied().unwrap_or(self.start);
        let delta = (p - prev) / self.start * 100.;
        for (i, t) in [0.15, 0.3, 0.6, 1.2, 2.0].iter().enumerate() {
            if delta.abs() > *t {
                self.previous[i] = self.last[i];
                self.last[i] = (delta, k);
            }
        }
        if delta.abs() > 0.25 {
            self.dir = delta.signum();
            self.jumps.push((delta, k));
        }
        let d = self.dir;
        self.prices.push(p);
        let mut f = Vec::with_capacity(NF);
        f.extend([
            k as f64,
            left as f64,
            (p - self.start) / self.start * 100. * d,
        ]);
        for lag in [1usize, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128] {
            let old = if k >= lag {
                self.prices[k - lag]
            } else {
                self.start
            };
            f.push((p - old) / self.start * 100. * d);
        }
        for (i, tau) in [2., 4., 8., 16., 32., 64.].iter().enumerate() {
            self.ema[i] += (p - self.ema[i]) / tau;
            f.push((p - self.ema[i]) / self.start * 100. * d);
        }
        for len in [8usize, 16, 32, 64, 128, 256] {
            let mut lo = p * d;
            let mut hi = lo;
            let mut ilo = k;
            let mut ihi = k;
            for i in k.saturating_sub(len - 1)..=k {
                let z = self.prices[i] * d;
                if z <= lo {
                    lo = z;
                    ilo = i;
                }
                if z >= hi {
                    hi = z;
                    ihi = i;
                }
            }
            f.extend([
                (p * d - hi) / self.start * 100.,
                (p * d - lo) / self.start * 100.,
                (k - ihi) as f64,
                (k - ilo) as f64,
            ]);
        }
        for i in 0..5 {
            f.extend([
                self.last[i].0 * d,
                (k - self.last[i].1) as f64,
                self.previous[i].0 * d,
                (k - self.previous[i].1) as f64,
            ]);
        }
        for j in 0..8 {
            let (val, t) = self.jumps.iter().rev().nth(j).copied().unwrap_or((0., 0));
            f.extend([val * d, (k - t) as f64]);
        }
        for len in [4usize, 8, 16, 32, 64, 128] {
            let mut vol = 0.;
            for i in k.saturating_sub(len - 1)..=k {
                let prev = if i == 0 {
                    self.start
                } else {
                    self.prices[i - 1]
                };
                vol += (self.prices[i] - prev).abs() / self.start * 100.;
            }
            f.push(vol);
        }
        let mut out = [0.; NF];
        for (i, v) in f.into_iter().take(NF).enumerate() {
            out[i] = v;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crash_and_squeeze_features_have_the_same_canonical_direction() {
        let start = 143.0;
        let path = [143.0, 143.125, 144.0, 144.5, 144.5, 144.25, 143.5];
        let mut up = Features::new(start);
        let mut down = Features::new(start);
        for (k, price) in path.into_iter().enumerate() {
            let a = up.observe(price, k, 20 - k);
            let b = down.observe(2.0 * start - price, k, 20 - k);
            if k >= 2 {
                assert_eq!(up.dir, -down.dir);
                for i in 0..NF {
                    assert!((a[i] - b[i]).abs() < 1e-9, "feature {i} at {k}");
                }
            }
        }
    }

    #[test]
    fn a_flat_session_is_finite_through_warmup_and_all_lookbacks() {
        let mut state = Features::new(143.0);
        for k in 0..400 {
            let input = state.observe(143.0, k, 399 - k);
            assert!(input.iter().all(|x| x.is_finite()));
            assert!(input[2..23].iter().all(|x| *x == 0.0));
        }
    }

    #[test]
    fn impulse_age_counts_observed_half_second_steps() {
        let mut state = Features::new(143.0);
        state.observe(143.0, 0, 20);
        state.observe(147.0, 1, 19);
        let input = state.observe(147.0, 2, 18);
        assert_eq!(input[48], 1.0);
        assert_eq!(input[68], 1.0);
        assert!((input[47] - 400.0 / 143.0).abs() < 1e-9);
    }
}
