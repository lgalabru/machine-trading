#[derive(Default)]
pub struct Features {
    pub index: Vec<f64>,
    pub book: Vec<f64>,
    ema: [f64; 7],
    ep: i32,
    epstart: usize,
    epidx: f64,
    epgap: f64,
    maxgap: f64,
    epnum: f64,
    infnum: f64,
    last_informed: bool,
    zeroage: f64,
    vol: f64,
    prev_tick: usize,
    sign: f64,
}
impl Features {
    pub fn update(&mut self, idx: f64, bk: f64, period: f64, cap: f64, left: f64) -> [f64; 65] {
        let i = self.index.len();
        let gap = bk - idx;
        let tick = i == 0 || idx != self.index[i - 1] || bk != self.book[i - 1];
        self.index.push(idx);
        self.book.push(bk);
        if i == 0 {
            self.ema.fill(idx);
            self.epidx = idx;
            self.vol = 6.;
            self.sign = 1.;
        }
        if gap.abs() > 12. && (self.ep == 0 || gap * (self.ep as f64) < 0.) {
            if self.ep != 0 {
                self.epnum += 1.;
                self.infnum += if self.last_informed { 1. } else { 0. };
            }
            self.ep = if gap > 0. { 1 } else { -1 };
            self.epstart = i;
            self.epidx = idx;
            self.epgap = gap;
            self.maxgap = gap.abs();
            self.last_informed = false;
        }
        if self.ep != 0 {
            self.maxgap = self.maxgap.max(gap.abs());
            if (idx - self.epidx) * self.ep as f64 > 18f64.max(self.maxgap * 0.7) {
                self.last_informed = true;
            }
            if gap.abs() < 3. {
                self.zeroage += 0.5;
                if self.zeroage >= 2. {
                    self.epnum += 1.;
                    self.infnum += if self.last_informed { 1. } else { 0. };
                    self.ep = 0;
                }
            } else {
                self.zeroage = 0.;
            }
        }
        if gap.abs() > 5. {
            self.sign = gap.signum();
        } else if (idx - self.ema[4]).abs() > 1. {
            self.sign = (idx - self.ema[4]).signum();
        }
        if tick {
            if i > 0 {
                let d = (idx - self.index[self.prev_tick]).abs();
                self.vol = 0.94 * self.vol + 0.06 * d.min(30.);
            }
            self.prev_tick = i;
        }
        let s = self.sign;
        let mut x = [0.; 65];
        x[0] = gap * s;
        x[1] = gap.abs();
        x[2] = period;
        x[3] = cap;
        x[4] = gap.clamp(-cap, cap) / period * s;
        x[5] = self.vol;
        let mut k = 6;
        for lag in [1, 2, 4, 8, 16, 32, 64, 120] {
            let j = i.saturating_sub(lag);
            x[k] = (idx - self.index[j]) * s;
            k += 1;
            x[k] = (gap - (self.book[j] - self.index[j])) * s;
            k += 1;
        }
        for (j, t) in [1.0f64, 2., 4., 8., 16., 32., 64.].iter().enumerate() {
            self.ema[j] += (idx - self.ema[j]) * (1. - (-0.5 / t).exp());
            x[k] = (idx - self.ema[j]) * s;
            k += 1;
        }
        for length in [8, 16, 32, 64] {
            let start = (i + 1).saturating_sub(length);
            let cnt = i - start + 1;
            let av = self.index[start..=i].iter().sum::<f64>() / cnt as f64;
            let mut cov = 0.;
            let mut var = 0.;
            for j in start..=i {
                let t = (j as f64 - start as f64 - (cnt - 1) as f64 / 2.) * 0.5;
                cov += t * (self.index[j] - av);
                var += t * t;
            }
            let slope = cov / var.max(0.01);
            x[k] = slope * s;
            k += 1;
            x[k] = (idx - av - slope * (cnt - 1) as f64 * 0.25) * s;
            k += 1;
        }
        x[k] = self.ep as f64 * s;
        k += 1;
        x[k] = if self.ep != 0 {
            (i - self.epstart) as f64 * 0.5
        } else {
            0.
        };
        k += 1;
        x[k] = if self.ep != 0 {
            (idx - self.epidx) * s
        } else {
            0.
        };
        k += 1;
        x[k] = if self.ep != 0 { self.epgap * s } else { 0. };
        k += 1;
        x[k] = if self.ep != 0 { self.maxgap } else { 0. };
        k += 1;
        x[k] = if self.ep != 0 {
            gap.abs() / self.maxgap.max(1.)
        } else {
            0.
        };
        k += 1;
        x[k] = (self.infnum + 1.) / (self.epnum + 2.);
        k += 1;
        x[k] = self.epnum;
        k += 1;
        x[k] = if self.last_informed { 1. } else { 0. };
        k += 1;
        x[k] = (i - self.prev_tick) as f64 * 0.5;
        k += 1;
        x[k] = s;
        k += 1;
        x[k] = left;
        k += 1;
        for length in [8, 16, 32] {
            let mut cnt = 0f64;
            let mut sums = 0.;
            let mut abss = 0.;
            let mut largest = 0f64;
            for j in ((i + 1).saturating_sub(length)).max(1)..=i {
                let d = self.index[j] - self.index[j - 1];
                if d != 0. {
                    cnt += 1.;
                    sums += d.signum() * s;
                    abss += d.abs();
                    largest = largest.max(d.abs());
                }
            }
            x[k] = sums / cnt.max(1.);
            k += 1;
            x[k] = largest;
            k += 1;
            x[k] = abss;
            k += 1;
        }
        x
    }
}
