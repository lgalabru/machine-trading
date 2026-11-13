//! Causal features: rolling index trends, noise, premium shape, funding,
//! and evidence from episodes that have already ended.

#[derive(Default)]
pub struct Features {
    hist: Vec<(f64, f64)>,
    obs: Vec<f64>,
    slopes: Vec<f64>,
    epstart: f64,
    epx: f64,
    epsign: f64,
    maxp: f64,
    lasttick: f64,
    lastret: f64,
    lastx: f64,
    count: f64,
    informed_count: f64,
    lastdur: f64,
    crowdhalf: f64,
    crowdn: f64,
}
fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}
impl Features {
    pub fn update(
        &mut self,
        x: f64,
        p: f64,
        t: f64,
        period: f64,
        cap: f64,
        left: f64,
    ) -> [f64; 33] {
        if self.hist.is_empty() {
            self.crowdhalf = 15.0;
        }
        self.hist.push((x, p));
        if self.hist.len() > 65 {
            self.hist.remove(0);
        }
        let mut a = [0.0; 33];
        a[0] = p;
        a[1] = p.abs();
        a[2] = p.clamp(-cap, cap) * 7.0 / period;
        a[3] = period;
        let mut mean12 = 0.0;
        let mut slope12 = 0.0;
        for (q, win) in [2, 4, 8, 12, 20, 32].iter().enumerate() {
            let start = self.hist.len().saturating_sub(win * 2 + 1);
            let h = &self.hist[start..];
            let n = h.len() as f64;
            let mx = h.iter().map(|v| v.0).sum::<f64>() / n;
            let mp = h.iter().map(|v| v.1).sum::<f64>() / n;
            let mt = -(n - 1.0) * 0.25;
            let mut cov = 0.0;
            let mut var = 0.0;
            for (j, z) in h.iter().enumerate() {
                let dt = (j as f64 - n + 1.0) * 0.5 - mt;
                cov += dt * (z.0 - mx);
                var += dt * dt;
            }
            let slope = if var > 0.0 { cov / var } else { 0.0 };
            a[4 + q] = x - mx;
            if q >= 1 && q <= 4 {
                a[9 + q] = slope;
            }
            if q == 1 {
                a[16] = p - mp;
            }
            if q == 3 {
                a[17] = p - mp;
                mean12 = mx;
                slope12 = slope;
            }
        }
        if self.obs.is_empty() || (x - self.lastx).abs() > 1e-8 {
            if !self.obs.is_empty() {
                self.lastret = x - self.lastx;
                self.slopes
                    .push(self.lastret / (t - self.lasttick).max(0.5));
                if self.slopes.len() > 12 {
                    self.slopes.remove(0);
                }
            }
            self.obs.push(x);
            if self.obs.len() > 32 {
                self.obs.remove(0);
            }
            self.lastx = x;
            self.lasttick = t;
        }
        if !self.slopes.is_empty() {
            a[14] = median(self.slopes[self.slopes.len().saturating_sub(5)..].to_vec());
            a[15] = median(self.slopes.clone());
        }
        a[18] = 0.0006;
        if self.obs.len() > 7 {
            let mut d: Vec<f64> = self.obs.windows(2).map(|w| w[1] - w[0]).collect();
            let med = median(d.clone());
            for v in &mut d {
                *v = (*v - med).abs();
            }
            a[18] = median(d) * 2.0;
        }
        a[19] = if p > 0.0 { x - mean12 } else { mean12 - x };
        if p.abs() < 0.0007 {
            if self.epsign != 0.0 && self.maxp > 0.002 && t - self.epstart > 8.0 {
                let net = (x - self.epx) * self.epsign;
                let informed = net > self.maxp * 0.65;
                self.count += 1.0;
                if informed {
                    self.informed_count += 1.0;
                } else {
                    self.crowdhalf = (self.crowdhalf * self.crowdn + (t - self.epstart) / 3.0)
                        / (self.crowdn + 1.0);
                    self.crowdn += 1.0;
                }
                self.lastdur = t - self.epstart;
            }
            self.epsign = 0.0;
            self.epstart = -100.0;
            self.maxp = 0.0;
        }
        if p.abs() > 0.001 && (self.epsign == 0.0 || self.epsign * p < 0.0) {
            self.epstart = t;
            self.epsign = if p > 0.0 { 1.0 } else { -1.0 };
            self.epx = x;
            self.maxp = p.abs();
        }
        if self.epsign != 0.0 {
            self.maxp = self.maxp.max(p.abs());
            a[20] = (t - self.epstart).min(100.0);
            a[21] = (x - self.epx) * self.epsign;
            a[22] = self.maxp;
            a[23] = p.abs() / self.maxp;
        }
        a[24] = (t - self.lasttick).min(5.0);
        a[25] = self.lastret;
        a[26] = cap;
        a[27] = left;
        a[28] = p * slope12;
        a[29] = (self.informed_count + 0.5) / (self.count + 1.0);
        a[30] = self.count.min(6.0);
        a[31] = self.lastdur;
        a[32] = self.crowdhalf;
        a
    }
}
