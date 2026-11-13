//! Permutation-invariant account and price-history features.
//!
//! Actors are ranked by repeated directional trading and limit aggressiveness,
//! never by their public key. Several impact half-lives distinguish temporary
//! pressure from permanent information. Feature order matches training/features.py.

const LN2: f64 = std::f64::consts::LN_2;
const H: [f64; 4] = [12., 32., 80., 160.];
const AH: [f64; 3] = [60., 120., 240.];
#[derive(Default, Clone)]
pub struct Actor {
    pub key: String,
    pub n: usize,
    pub net: f64,
    pub qty: f64,
    pub countnet: i32,
    pub first: f64,
    pub last: f64,
    pub gapsum: f64,
    pub gap2: f64,
    pub limitsum: f64,
    pub maxlimit: f64,
    pub minlimit: f64,
    pub high: usize,
    pub low: usize,
    pub decay: [f64; 3],
    pub lastsize: f64,
}
impl Actor {
    pub fn new(key: String, t: f64) -> Self {
        Self {
            key,
            first: t,
            last: t,
            minlimit: 10.,
            ..Self::default()
        }
    }
    pub fn add(&mut self, t: f64, q: f64, ratio: f64) {
        if self.n > 0 {
            let g = t - self.last;
            self.gapsum += g;
            self.gap2 += g * g;
        }
        self.n += 1;
        self.net += q;
        self.qty += q.abs();
        self.countnet += if q > 0. { 1 } else { -1 };
        self.last = t;
        self.limitsum += ratio;
        self.maxlimit = self.maxlimit.max(ratio);
        self.minlimit = self.minlimit.min(ratio);
        self.high += usize::from(ratio > 4.15);
        self.low += usize::from(ratio < 2.7);
        for x in &mut self.decay {
            *x += q;
        }
        self.lastsize = q;
    }
    pub fn score(&self) -> f64 {
        if self.countnet.unsigned_abs() as usize == self.n && self.low == 0 {
            self.n as f64 + 3. * self.high as f64 + (self.maxlimit - 3.).max(0.)
        } else {
            -100. + self.n as f64 * 0.01
        }
    }
    fn features(&self, t: f64) -> Vec<f64> {
        let n = self.n as f64;
        let g = self.gapsum / (n - 1.).max(1.);
        let cv = (self.gap2 / (n - 1.).max(1.) - g * g).max(0.).sqrt() / g.max(1.);
        vec![
            n,
            self.net,
            self.qty,
            self.countnet as f64 / n,
            self.first,
            t - self.last,
            g,
            cv,
            self.limitsum / n,
            self.maxlimit,
            self.minlimit,
            self.high as f64,
            self.low as f64,
            self.decay[0],
            self.decay[1],
            self.decay[2],
            self.lastsize,
            if self.countnet > 0 { 1. } else { -1. },
        ]
    }
}
#[derive(Default)]
pub struct State {
    pub reopen: f64,
    pub spread: f64,
    pub t: f64,
    pub y: Option<f64>,
    pub ema: [f64; 3],
    pub slope: [f64; 3],
    pub flow: [f64; 4],
    pub absflow: [f64; 4],
    pub actors: Vec<Actor>,
    pub history: Vec<(f64, f64)>,
    pub lastflow: f64,
    pub pending: Vec<(f64, f64, f64)>,
    pub beta: f64,
    pub betaw: f64,
    pub slope_sum: [f64; 3],
    pub slope_time: [f64; 3],
}
impl State {
    pub fn new(reopen: f64, spread: f64) -> Self {
        Self {
            reopen,
            spread,
            lastflow: -100.,
            beta: 0.14,
            betaw: 3.,
            ..Self::default()
        }
    }
    pub fn update(&mut self, t: f64, y: f64, flows: &[(String, f64, f64)]) {
        let dt = t - self.t;
        self.t = t;
        for a in &mut self.actors {
            for (x, h) in a.decay.iter_mut().zip(AH) {
                *x *= (-LN2 * dt / h).exp();
            }
        }
        for i in 0..4 {
            let d = (-LN2 * dt / H[i]).exp();
            self.flow[i] *= d;
            self.absflow[i] *= d;
        }
        if self.y.is_none() {
            self.y = Some(y);
            self.ema = [y; 3];
        }
        let old = self.y.unwrap_or(y);
        let dy = y - old;
        for (i, h) in [24., 60., 120.].iter().enumerate() {
            let d = (-LN2 * dt / h).exp();
            self.slope_sum[i] *= d;
            self.slope_time[i] *= d;
        }
        if flows.is_empty() && t - self.lastflow > 4. {
            for i in 0..3 {
                self.slope_sum[i] += dy;
                self.slope_time[i] += dt;
            }
        }
        if flows.is_empty() && t - self.lastflow > 6. {
            for (i, h) in [24., 60., 120.].iter().enumerate() {
                let a = 1. - (-LN2 * dt / h).exp();
                self.slope[i] += (dy / dt.max(1.) - self.slope[i]) * a;
            }
        }
        for (i, h) in [12., 32., 80.].iter().enumerate() {
            let a = 1. - (-LN2 * dt / h).exp();
            self.ema[i] += (y - self.ema[i]) * a;
        }
        if !flows.is_empty() {
            self.lastflow = t;
            let q: f64 = flows.iter().map(|x| x.1).sum();
            let qa: f64 = flows.iter().map(|x| x.1.abs()).sum();
            if q.abs() > 0.3 && q.abs() > qa * 0.9 {
                self.pending.push((t, old, q));
            }
        }
        while self.pending.first().map(|x| t - x.0 >= 4.).unwrap_or(false) {
            let (tt, old, q) = self.pending.remove(0);
            if self.lastflow == tt {
                let b = ((y - old) / q).clamp(0.03, 0.30);
                let w = q.abs().min(2.);
                self.beta = (self.beta * self.betaw + b * w) / (self.betaw + w);
                self.betaw = (40.0f64).min(self.betaw + w);
            }
        }
        for (key, q, ratio) in flows {
            let i = if let Some(i) = self.actors.iter().position(|a| &a.key == key) {
                i
            } else {
                self.actors.push(Actor::new(key.clone(), t));
                self.actors.len() - 1
            };
            self.actors[i].add(t, *q, *ratio);
            for i in 0..4 {
                self.flow[i] += q;
                self.absflow[i] += q.abs();
            }
        }
        self.history.push((t, y));
        self.y = Some(y);
    }
    pub fn features(&self) -> Vec<f64> {
        let t = self.t;
        let y = self.y.unwrap_or(0.);
        let mut lag = Vec::new();
        for h in [32., 80., 160.] {
            let v = self
                .history
                .iter()
                .rev()
                .find(|x| x.0 <= t - h)
                .or(self.history.first())
                .map(|x| x.1)
                .unwrap_or(y);
            lag.push(y - v);
        }
        let mut actors: Vec<&Actor> = self.actors.iter().collect();
        actors.sort_by(|a, b| b.score().total_cmp(&a.score()));
        let net: f64 = self.actors.iter().map(|a| a.net).sum();
        let qty: f64 = self.actors.iter().map(|a| a.qty).sum();
        let sn: f64 = self
            .actors
            .iter()
            .filter(|a| a.score() >= 3.)
            .map(|a| a.net)
            .sum();
        let sq: f64 = self
            .actors
            .iter()
            .filter(|a| a.score() >= 3.)
            .map(|a| a.qty)
            .sum();
        let mut f = vec![t, self.reopen - t, t / self.reopen, self.spread, y];
        for e in self.ema {
            f.push(y - e);
        }
        f.extend(lag);
        f.extend(self.slope);
        for h in AH {
            f.push(y + self.slope[1] * h / LN2);
        }
        f.push(self.beta);
        f.extend(self.flow);
        f.extend(self.absflow);
        f.extend([net, qty, sn, sq]);
        for i in 0..3 {
            if i < actors.len() {
                f.extend(actors[i].features(t));
            } else {
                f.extend([0.; 18]);
            }
        }
        let mut td = [0.; 3];
        for a in &self.actors {
            for i in 0..3 {
                td[i] += a.decay[i];
            }
        }
        for i in 0..4 {
            for j in 0..3 {
                let mut pred = td[j];
                if i < 3 && i < actors.len() {
                    pred += actors[i].net - actors[i].decay[j];
                }
                f.push(y - self.beta * pred);
            }
        }
        let mut balanced = [0.; 3];
        for i in 0..3 {
            balanced[i] = self.slope_sum[i] / self.slope_time[i].max(1.);
        }
        f.extend(balanced);
        for h in AH {
            f.push(y + balanced[1] * h / LN2);
        }
        f.extend([self.betaw, self.slope_time[1]]);
        f.into_iter().map(|v| v as f32 as f64).collect()
    }
}
