//! Causal, direction-normalized features. Their order is part of the model.
pub const FEATURES: usize = 53;

#[derive(Default)]
pub struct Features {
    prices: Vec<f64>,
    jumps: Vec<(usize, f64)>,
    dir: f64,
    max: f64,
    min: f64,
    max_k: usize,
    min_k: usize,
    last_small: usize,
    last_medium: usize,
    last_large: usize,
}

impl Features {
    pub fn update(&mut self, k: usize, p: f64, left: usize) -> ([f32; FEATURES], f64) {
        let init = self.prices.first().copied().unwrap_or(p);
        let prev = self.prices.last().copied().unwrap_or(p);
        let delta = p / prev - 1.0;
        if delta.abs() > 0.001 {
            self.last_small = k;
        }
        if delta.abs() > 0.0025 {
            self.last_medium = k;
            self.dir = delta.signum();
            self.jumps.push((k, delta));
        }
        if delta.abs() > 0.005 {
            self.last_large = k;
        }
        if self.prices.is_empty() || p > self.max {
            self.max = p;
            self.max_k = k;
        }
        if self.prices.is_empty() || p < self.min {
            self.min = p;
            self.min_k = k;
        }
        let dir = if self.dir == 0.0 { 1.0 } else { self.dir };
        self.prices.push(p);
        let mut f = Vec::with_capacity(FEATURES);

        // Clock, displacement, and returns over 0.5 to 64 seconds.
        f.push(k as f64 * 0.5);
        f.push(left as f64 * 0.5);
        f.push(dir * (p / init - 1.0));
        for lag in [1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128] {
            let past = self.prices[self.prices.len().saturating_sub(lag + 1)];
            f.push(dir * (p / past - 1.0));
        }

        // Session extremes, oriented to the most recent meaningful move.
        let (high, low, high_k, low_k) = if dir > 0.0 {
            (self.max, self.min, self.max_k, self.min_k)
        } else {
            (self.min, self.max, self.min_k, self.max_k)
        };
        f.push(dir * (p / high - 1.0));
        f.push(dir * (p / low - 1.0));
        f.push((k - high_k) as f64 * 0.5);
        f.push((k - low_k) as f64 * 0.5);

        // Local ranges and ages of their extremes.
        for window in [8, 20, 60] {
            let start = self.prices.len().saturating_sub(window);
            let mut hi = self.prices[start];
            let mut lo = hi;
            let mut hik = start;
            let mut lok = start;
            for i in start..self.prices.len() {
                if self.prices[i] >= hi {
                    hi = self.prices[i];
                    hik = i;
                }
                if self.prices[i] <= lo {
                    lo = self.prices[i];
                    lok = i;
                }
            }
            if dir > 0.0 {
                f.push(dir * (p / hi - 1.0));
                f.push(dir * (p / lo - 1.0));
                f.push((k - hik) as f64 * 0.5);
                f.push((k - lok) as f64 * 0.5);
            } else {
                f.push(dir * (p / lo - 1.0));
                f.push(dir * (p / hi - 1.0));
                f.push((k - lok) as f64 * 0.5);
                f.push((k - hik) as f64 * 0.5);
            }
        }

        f.push((k - self.last_small) as f64 * 0.5);
        f.push((k - self.last_medium) as f64 * 0.5);
        f.push((k - self.last_large) as f64 * 0.5);
        for j in 0..6 {
            let jump = self.jumps.iter().rev().nth(j);
            f.push(jump.map(|(_, d)| dir * d).unwrap_or(0.0));
            f.push(jump.map(|(t, _)| (k - t) as f64 * 0.5).unwrap_or(200.0));
        }
        f.push(self.jumps.len() as f64);
        let mut run = 0.0;
        let mut n = 0;
        for &(_, d) in self.jumps.iter().rev() {
            if dir * d < 0.0 {
                break;
            }
            run += d * dir;
            n += 1;
        }
        f.push(run);
        f.push(n as f64);
        let mut trend = 0.0;
        for lag in 1..=8 {
            let old = self.prices[self.prices.len().saturating_sub(lag + 1)];
            trend += dir * (p / old - 1.0);
        }
        f.push(trend / 8.0);
        f.push((k as f64 + left as f64) * 0.5);

        assert_eq!(f.len(), FEATURES);
        let mut out = [0f32; FEATURES];
        for (i, value) in f.iter().enumerate() {
            out[i] = *value as f32;
        }
        (out, dir)
    }
}
