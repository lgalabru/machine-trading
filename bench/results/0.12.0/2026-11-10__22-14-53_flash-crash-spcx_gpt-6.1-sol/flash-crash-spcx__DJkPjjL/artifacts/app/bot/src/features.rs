#[derive(Default, Clone)]
struct Cluster {
    start: f64,
    end: f64,
    begin: usize,
    last: usize,
    parts: usize,
    direction: f64,
    last_part: f64,
}

#[derive(Default)]
pub struct Features {
    p: Vec<f64>,
    dir: f64,
    ema: [f64; 6],
    last: [usize; 5],
    jump: [f64; 5],
    count: [f64; 5],
    first: Option<usize>,
    gross: f64,
    clusters: Vec<Cluster>,
    checkpoint: f64,
    high: f64,
    low: f64,
    high_at: usize,
    low_at: usize,
}
impl Features {
    pub fn observe(&mut self, price: f64, left: usize) -> (f64, Vec<f64>) {
        let k = self.p.len();
        let prior = self.p.last().copied().unwrap_or(price);
        let change = (price - prior) / prior;
        let thresholds = [0.0025, 0.004, 0.008, 0.016, 0.03];
        if change.abs() > thresholds[0] {
            self.dir = change.signum();
            self.first.get_or_insert(k);
        }
        if self.dir == 0.0 {
            self.dir = 1.0;
        }
        let d = self.dir;
        if k == 0 {
            self.checkpoint = price;
            self.high = price;
            self.low = price;
        }
        if price > self.high {
            self.high = price;
            self.high_at = k;
        }
        if price < self.low {
            self.low = price;
            self.low_at = k;
        }
        let impulse = price / self.checkpoint - 1.;
        if impulse.abs() > 0.0025 {
            let new_cluster = self
                .clusters
                .last()
                .is_none_or(|c| c.direction != impulse.signum() || k - c.last > 14);
            if new_cluster {
                self.clusters.push(Cluster {
                    start: self.checkpoint,
                    end: price,
                    begin: k,
                    last: k,
                    parts: 1,
                    direction: impulse.signum(),
                    last_part: impulse,
                });
            } else {
                let c = self.clusters.last_mut().unwrap();
                c.end = price;
                c.last = k;
                c.parts += 1;
                c.last_part = impulse;
            }
            self.checkpoint = price;
        }

        for i in 0..5 {
            if change.abs() > thresholds[i] {
                self.last[i] = k;
                self.jump[i] = change;
                self.count[i] += 1.0;
            }
        }
        self.gross += change.abs();
        self.p.push(price);
        let initial = self.p[0];
        let mut f = vec![
            k as f64 * 0.5,
            left as f64 * 0.5,
            (price / initial - 1.0) * d,
            (price / initial - 1.0).abs(),
            self.gross,
            self.first.map(|x| (k - x) as f64).unwrap_or(999.0),
        ];
        for lag in [1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192] {
            let past = self.p[k.saturating_sub(lag)];
            f.push((price / past - 1.0) * d);
        }
        for window in [4, 8, 16, 32, 64, 128, 512] {
            let slice = &self.p[self.p.len().saturating_sub(window)..];
            let hi = slice.iter().copied().fold(price, f64::max);
            let lo = slice.iter().copied().fold(price, f64::min);
            if d > 0.0 {
                f.push((hi - price) / price);
                f.push((price - lo) / price);
            } else {
                f.push((price - lo) / price);
                f.push((hi - price) / price);
            }
            let mean = slice.iter().sum::<f64>() / slice.len() as f64;
            f.push((price / mean - 1.0) * d);
        }
        for i in 0..5 {
            f.push((k - self.last[i]) as f64);
            f.push(self.jump[i] * d);
            f.push(self.count[i]);
        }
        for (i, n) in [4.0, 8.0, 16.0, 32.0, 64.0, 128.0].iter().enumerate() {
            if k == 0 {
                self.ema[i] = price;
            } else {
                self.ema[i] += 2.0 / (n + 1.0) * (price - self.ema[i]);
            }
            f.push((price / self.ema[i] - 1.0) * d);
        }
        // Explicit move histories help distinguish the first shock from its aftermath.
        f.push(self.clusters.len() as f64);
        for lag in 0..4 {
            if let Some(c) = self.clusters.iter().rev().nth(lag) {
                f.push((c.end / c.start - 1.) * d);
                f.push((k - c.last) as f64);
                f.push((c.last - c.begin) as f64);
                f.push(c.parts as f64);
                f.push(c.last_part * d);
            } else {
                f.extend_from_slice(&[0., 999., 0., 0., 0.]);
            }
        }
        if d > 0. {
            f.push((self.high - price) / price);
            f.push((price - self.low) / price);
            f.push((k - self.high_at) as f64);
            f.push((k - self.low_at) as f64);
        } else {
            f.push((price - self.low) / price);
            f.push((self.high - price) / price);
            f.push((k - self.low_at) as f64);
            f.push((k - self.high_at) as f64);
        }
        let peak = if self.high / initial - 1. > 1. - self.low / initial {
            self.high
        } else {
            self.low
        };
        f.push((peak / initial - 1.) * d);
        f.push((price - initial) * d / (peak - initial).abs().max(initial * 0.001));
        (d, f)
    }
}
