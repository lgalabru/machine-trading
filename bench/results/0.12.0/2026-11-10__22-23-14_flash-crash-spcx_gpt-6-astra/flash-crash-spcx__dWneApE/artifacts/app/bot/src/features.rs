#[derive(Default)]
pub struct Features {
    prices: Vec<f64>,
    base: f64,
    extreme: f64,
    direction: f64,
    last_move: i64,
    start: i64,
    count: f64,
    previous_direction: f64,
    previous_size: f64,
    previous_end: i64,
    previous_duration: f64,
    variation: f64,
    biggest: f64,
    changes: f64,
}
impl Features {
    pub fn update(&mut self, p: f64, k: u64, left: u64) -> [f64; 44] {
        // Fill a missed observation with the last known price, keeping lags
        // measured in market turns rather than successful RPC reads.
        while self.prices.len() < k as usize {
            let missing = self.prices.len() as u64;
            let previous = self.prices.last().copied().unwrap_or(p);
            self.update(previous, missing, left + k - missing);
        }
        let i = self.prices.len();
        if i == 0 {
            self.base = p;
            self.extreme = p;
            self.last_move = -200;
            self.start = -200;
            self.previous_end = -200;
        }
        let last = self.prices.last().copied().unwrap_or(p);
        let change = (p / last - 1.0) * 100.0;
        self.variation += change.abs();
        if change.abs() > 0.25 {
            if self.direction == 0.0
                || change * self.direction < 0.0
                || k as i64 - self.last_move > 15
            {
                self.previous_direction = self.direction;
                self.previous_size = (self.extreme / self.base - 1.0) * 100.0;
                self.previous_end = self.last_move;
                self.previous_duration = (self.last_move - self.start) as f64;
                self.base = last;
                self.extreme = p;
                self.direction = change.signum();
                self.start = k as i64;
                self.count += 1.0;
                self.changes = 0.0;
            }
            self.last_move = k as i64;
            self.changes += 1.0;
        }
        if (p - self.extreme) * self.direction > 0.0 {
            self.extreme = p;
        }
        let size = (self.extreme / self.base - 1.0) * 100.0;
        self.biggest = self.biggest.max(size.abs());
        self.prices.push(p);
        let mut f = [0.0; 44];
        f[0] = k as f64 / 2.0;
        f[1] = left as f64 / 2.0;
        f[2] = (p / self.prices[0] - 1.0) * 100.0;
        let mut j = 3;
        for lag in [1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128] {
            f[j] = (p / self.prices[i.saturating_sub(lag)] - 1.0) * 100.0;
            j += 1;
        }
        for window in [8, 24, 64, 128] {
            let a = &self.prices[(i + 1).saturating_sub(window)..];
            let lo = a.iter().copied().fold(f64::INFINITY, f64::min);
            let hi = a.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let mean = a.iter().sum::<f64>() / a.len() as f64;
            f[j] = (p / lo - 1.0) * 100.0;
            f[j + 1] = (p / hi - 1.0) * 100.0;
            f[j + 2] = (p / mean - 1.0) * 100.0;
            j += 3;
        }
        f[j..].copy_from_slice(&[
            self.direction,
            ((k as i64 - self.last_move).min(200)) as f64,
            size,
            ((k as i64 - self.start).min(200)) as f64,
            self.count,
            self.previous_direction,
            self.previous_size,
            ((k as i64 - self.previous_end).min(200)) as f64,
            (p / self.base - 1.0) * 100.0,
            (p / self.extreme - 1.0) * 100.0,
            self.variation,
            self.biggest,
            self.changes,
            self.previous_duration,
            change.abs(),
        ]);
        for x in &mut f {
            *x = (*x as f32) as f64;
        }
        f
    }
}
