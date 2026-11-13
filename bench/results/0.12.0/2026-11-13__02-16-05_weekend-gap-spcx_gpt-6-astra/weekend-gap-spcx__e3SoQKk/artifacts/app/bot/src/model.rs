//! Online decomposition of lasting impact, decaying inventory pressure and drift.
const HALVES: [f64; 5] = [65., 95., 135., 185., 240.];
const IMPACTS: [f64; 8] = [0.0008, 0.001, 0.0012, 0.0014, 0.0016, 0.0018, 0.002, 0.0022];
#[derive(Clone)]
struct Model {
    id: usize,
    half: f64,
    impact: f64,
    f: [f64; 3],
    x: [f64; 3],
    p: [[f64; 3]; 3],
    ll: f64,
}
#[derive(Clone, Default)]
pub struct Flow {
    pub count: u32,
    pub buys: u32,
    pub sells: u32,
    pub volume: f64,
    pub first: f64,
    pub last: f64,
    pub min_gap: f64,
    pub max_gap: f64,
    pub aggression: f64,
    pub min_aggression: f64,
}
impl Flow {
    pub fn add(&mut self, slot: f64, size: f64, aggression: f64) {
        if self.count == 0 {
            self.first = slot;
            self.min_gap = 10000.;
            self.min_aggression = 10.;
        } else {
            let gap = slot - self.last;
            self.min_gap = self.min_gap.min(gap);
            self.max_gap = self.max_gap.max(gap);
        }
        self.last = slot;
        self.count += 1;
        self.buys += u32::from(size > 0.);
        self.sells += u32::from(size < 0.);
        self.volume += size.abs();
        if aggression.is_finite() {
            self.aggression = self.aggression.max(aggression);
            self.min_aggression = self.min_aggression.min(aggression);
        }
    }
}
#[derive(Default)]
pub struct Signal {
    pub open: f64,
    pub rate: f64,
    pub temporary: f64,
    pub drift: f64,
    pub half: f64,
    pub probs: [f64; 9],
    pub impact: f64,
    pub permanent: f64,
}
pub struct Filter {
    models: Vec<Model>,
    pub flows: [Flow; 8],
    previous_flow: f64,
}
impl Default for Filter {
    fn default() -> Self {
        let mut models = Vec::new();
        for id in 0..9 {
            for half in HALVES {
                for impact in IMPACTS {
                    let mut p = [[0.; 3]; 3];
                    p[2][2] = 0.00025_f64.powi(2);
                    models
                        .push(Model {
                            id,
                            half,
                            impact,
                            f: [
                                1.,
                                (-std::f64::consts::LN_2 * 2. / half).exp(),
                                (-std::f64::consts::LN_2 * 2. / 300.).exp(),
                            ],
                            x: [0.; 3],
                            p,
                            ll: 0.,
                        });
                }
            }
        }
        Self {
            models,
            flows: std::array::from_fn(|_| Flow::default()),
            previous_flow: 0.,
        }
    }
}
impl Filter {
    pub fn step(&mut self, k: usize, price: f64, flow: [f64; 8]) -> Signal {
        let activity: f64 = flow.iter().map(|x| x.abs()).sum();
        let observe = k >= 4 && k % 3 == 2 && activity + self.previous_flow < 1e-9;
        self.previous_flow = activity;
        let mut logs = Vec::with_capacity(self.models.len());
        let mut maxlog = f64::NEG_INFINITY;
        for m in &mut self.models {
            for i in 0..3 {
                m.x[i] *= m.f[i];
                for j in 0..3 {
                    m.p[i][j] *= m.f[i] * m.f[j];
                }
            }
            m.p[2][2] += 0.0001_f64.powi(2) * 2.;
            for (c, q) in flow.iter().enumerate() {
                let i = if c == m.id { 0 } else { 1 };
                m.x[i] += q * m.impact;
                m.p[i][i] += (q * m.impact * 0.2).powi(2);
            }
            if observe {
                let u: [f64; 3] = std::array::from_fn(|i| m.p[i].iter().sum());
                let v = (u.iter().sum::<f64>() + 0.00015_f64.powi(2)).max(1e-16);
                let e = price - m.x.iter().sum::<f64>();
                m.ll -= 0.5 * (v.ln() + e * e / v);
                for i in 0..3 {
                    m.x[i] += u[i] / v * e;
                    for j in 0..3 {
                        m.p[i][j] -= u[i] * u[j] / v;
                    }
                }
            }
            let mut lp = if m.id == 8 { 0.4_f64.ln() } else { (0.6_f64 / 8.).ln() };
            if m.id < 8 && self.flows[m.id].buys > 0 && self.flows[m.id].sells > 0 {
                lp -= 30.;
            }
            let l = m.ll + lp;
            logs.push(l);
            maxlog = maxlog.max(l);
        }
        let weights: Vec<f64> = logs.iter().map(|l| (l - maxlog).exp()).collect();
        let sum: f64 = weights.iter().sum();
        let mut s = Signal::default();
        let mut conditional = [0.; 9];
        for (m, w) in self.models.iter().zip(weights.iter()) {
            let w = w / sum;
            s.probs[m.id] += w;
            conditional[m.id] += w * m.x[0];
            s.permanent += w * m.x[0];
            s.temporary += w * m.x[1];
            s.drift += w * m.x[2];
            s.impact += w * m.impact;
            s.half += w * m.half;
        }
        for (c, p) in conditional.iter_mut().enumerate() {
            *p /= s.probs[c].max(1e-30);
        }
        let slot = (k + 1) as f64 * 2.;
        let mut rates = [0.; 8];
        let mut opens = [0.; 8];
        for c in 0..8 {
            let f = &self.flows[c];
            let n = f.count as f64;
            if f.count == 0 || f.buys * f.sells > 0 {
                s.probs[c] = 0.;
                continue;
            }
            let sign = if f.buys > 0 { 1. } else { -1. };
            let age = slot - f.last;
            let mean = (f.last - f.first) / (n - 1.).max(1.);
            let pace = if n > 1. { mean.clamp(3., 30.) } else { 18. };
            let mut odds = 0.04 * 2_f64.powf(n.min(12.));
            if n > 1. {
                odds *= (20. / mean.max(5.)).max(0.05).powf((n - 1.).min(5.));
            }
            if f.max_gap > 3. * f.min_gap + 4. {
                odds *= 0.05;
            }
            if f.aggression > 4.1 {
                odds *= 10000.;
            }
            if f.min_aggression < 2.6 {
                odds *= 0.001;
                s.probs[c] *= 0.001;
            }
            let inst = odds / (1. + odds);
            let maxpace = if n > 1. { (2. * f.min_gap).min(30.) } else { 30. };
            let maxwait = 1.5 * maxpace + 2.;
            let survival = ((maxwait - age) / (maxwait * 0.4)).clamp(0., 1.);
            rates[c] = sign * f.volume / n / pace * inst * survival;
            let bound = (sign * conditional[c] - 0.0005).max(0.005);
            let far = if bound < 0.119 {
                (0.12 - bound) / (0.12 / bound).ln()
            } else {
                bound + 0.005
            };
            opens[c] = survival * sign * far + (1. - survival) * conditional[c];
            if age > maxwait && f.volume < 11.8 {
                s.probs[c] *= 1. + 30. * inst;
            }
            if n < 2. && f.aggression <= 4.1 {
                s.probs[c] *= 0.25;
            }
            s.probs[c] *= 1. + 10. * inst;
        }
        let total = s.probs.iter().sum::<f64>().max(1e-30);
        for p in &mut s.probs {
            *p /= total;
        }
        for c in 0..8 {
            s.open += s.probs[c] * opens[c];
            let survival = ((50. - self.flows[c].volume) / 30.).clamp(0., 1.);
            s.rate += rates[c] * (s.probs[c] + (1. - s.probs[c]) * survival) * s.impact;
        }
        s
    }
}
