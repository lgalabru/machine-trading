//! Causal, direction-normalized price history features. Each update consumes
//! exactly one current oracle observation. Windows, impulses and move groups
//! describe both rapid continuation and the time spent at a shocked level.

#[derive(Default)]
pub struct Features {
    prices: Vec<f64>,
    jump_t: [i64; 4],
    jump_p: [f64; 4],
    group_t: [i64; 4],
    group_end: [i64; 4],
    group_size: [f64; 4],
    direction: f64,
    hi: f64,
    lo: f64,
    hit: usize,
    lot: usize,
    count: usize,
}
impl Features {
    pub fn new() -> Self {
        Self {
            jump_t: [-1000; 4],
            group_t: [-1000; 4],
            group_end: [-1000; 4],
            direction: 1.0,
            ..Self::default()
        }
    }
    pub fn update(&mut self, p: f64) -> ([f32; 65], f64) {
        let k = self.prices.len();
        let delta = p - self.prices.last().copied().unwrap_or(p);
        if k == 0 {
            self.hi = p;
            self.lo = p;
        }
        self.prices.push(p);
        if p > self.hi {
            self.hi = p;
            self.hit = k;
        }
        if p < self.lo {
            self.lo = p;
            self.lot = k;
        }
        if delta.abs() > 0.25 {
            self.direction = delta.signum();
            for j in (1..4).rev() {
                self.jump_t[j] = self.jump_t[j - 1];
                self.jump_p[j] = self.jump_p[j - 1];
            }
            self.jump_t[0] = k as i64;
            self.jump_p[0] = delta;
            if self.group_size[0] * delta > 0.0 && k as i64 - self.group_end[0] <= 14 {
                self.group_size[0] += delta;
                self.group_end[0] = k as i64;
            } else {
                for j in (1..4).rev() {
                    self.group_t[j] = self.group_t[j - 1];
                    self.group_end[j] = self.group_end[j - 1];
                    self.group_size[j] = self.group_size[j - 1];
                }
                self.group_t[0] = k as i64;
                self.group_end[0] = k as i64;
                self.group_size[0] = delta;
                self.count += 1;
            }
        }
        let d = self.direction;
        let mut x = [0.0f32; 65];
        let mut f = 0;
        let mut put = |v: f64| {
            x[f] = v as f32;
            f += 1;
        };
        put(k as f64 * 0.5);
        put((p - self.prices[0]) * d);
        put(if d > 0.0 { self.hi - p } else { p - self.lo });
        put(if d > 0.0 { self.lo - p } else { p - self.hi });
        put((k - if d > 0.0 { self.hit } else { self.lot }) as f64 * 0.5);
        put((k - if d > 0.0 { self.lot } else { self.hit }) as f64 * 0.5);
        for w in [1, 2, 3, 4, 6, 10, 16, 24, 40, 60, 100] {
            put((p - self.prices[k.saturating_sub(w)]) * d);
        }
        for w in [6, 20, 60, 160] {
            let a = (k + 1).saturating_sub(w);
            let mut mx = self.prices[a];
            let mut mn = mx;
            let mut total = 0.0;
            let mut mt = a;
            let mut nt = a;
            for i in a..=k {
                let v = self.prices[i];
                total += v;
                if v >= mx {
                    mx = v;
                    mt = i;
                }
                if v <= mn {
                    mn = v;
                    nt = i;
                }
            }
            put((p - total / (k - a + 1) as f64) * d);
            put(if d > 0.0 { mx - p } else { p - mn });
            put(if d > 0.0 { mn - p } else { p - mx });
            put((k - if d > 0.0 { mt } else { nt }) as f64 * 0.5);
            put((k - if d > 0.0 { nt } else { mt }) as f64 * 0.5);
        }
        for j in 0..4 {
            put(self.jump_p[j] * d);
            put(((k as i64 - self.jump_t[j]) as f64 * 0.5).min(500.0));
        }
        for j in 0..4 {
            put(self.group_size[j] * d);
            put(((k as i64 - self.group_t[j]) as f64 * 0.5).min(500.0));
            put(((k as i64 - self.group_end[j]) as f64 * 0.5).min(500.0));
        }
        put(self.count as f64);
        for w in [10, 40, 100] {
            let a = k.saturating_sub(w);
            let mut v = 0.0;
            for i in a + 1..=k {
                v += (self.prices[i] - self.prices[i - 1]).abs();
            }
            put(v);
            put((p - self.prices[a]) * d / (v + 0.001));
        }
        put(k as f64 * 0.5 - ((k as i64 - self.group_t[0]) as f64 * 0.5).min(500.0));
        (x, d)
    }
}
