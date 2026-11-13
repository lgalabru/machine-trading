//! A bank of state-space models: one possible informed account (or none),
//! crossed with impact and transient-impact half-life hypotheses.
#[derive(Clone, Default)]
pub struct Actor {
    pub count: f64,
    pub volume: f64,
    pub absvol: f64,
    pub first: f64,
    pub last: f64,
    pub invalid: bool,
    pub sign: f64,
}
#[derive(Clone)]
struct Particle {
    id: usize,
    beta: f64,
    half: f64,
    x: [f64; 3],
    c: [[f64; 3]; 3],
    lw: f64,
}
pub struct Filter {
    particles: Vec<Particle>,
    pub actors: [Actor; 8],
    pub probs: [f64; 9],
    pub temp: f64,
    pub permanent: f64,
    pub beta: f64,
    pub half: f64,
    pub future: f64,
    pub opening: f64,
    pub last_t: f64,
}
impl Default for Filter {
    fn default() -> Self {
        let mut particles = Vec::new();
        for id in 0..9 {
            for b in 0..7 {
                for h in 0..7 {
                    let mut c = [[0.; 3]; 3];
                    c[2][2] = 0.0003f64.powi(2);
                    particles.push(Particle {
                        id,
                        beta: 0.0008 + b as f64 * 0.0002,
                        half: 60. + h as f64 * 30.,
                        x: [0.; 3],
                        c,
                        lw: if id == 8 {
                            (0.4f64 / 49.).ln()
                        } else {
                            (0.6f64 / 8. / 49.).ln()
                        },
                    });
                }
            }
        }
        Self {
            particles,
            actors: Default::default(),
            probs: [0.; 9],
            temp: 0.,
            permanent: 0.,
            beta: 0.0014,
            half: 150.,
            future: 0.,
            opening: 0.,
            last_t: 0.,
        }
    }
}
impl Filter {
    pub fn trade(&mut self, id: usize, t: f64, q: f64) {
        let a = &mut self.actors[id];
        if a.count == 0. {
            a.first = t;
            a.sign = q.signum();
        }
        if a.sign * q < 0. {
            a.invalid = true;
        }
        a.count += 1.;
        a.volume += q;
        a.absvol += q.abs();
        a.last = t;
    }
    pub fn update(&mut self, t: f64, y: f64, reopen: f64, flows: [f64; 8], qvar: [f64; 8]) {
        let dt = t - self.last_t;
        self.last_t = t;
        for p in &mut self.particles {
            let f = [
                1.,
                (-std::f64::consts::LN_2 * dt / p.half).exp(),
                (-std::f64::consts::LN_2 * dt / 300.).exp(),
            ];
            for i in 0..3 {
                p.x[i] *= f[i];
                for j in 0..3 {
                    p.c[i][j] *= f[i] * f[j];
                }
            }
            p.c[2][2] += 0.00045f64.powi(2) * dt / 12.;
            for i in 0..8 {
                let dest = if p.id == i { 0 } else { 1 };
                p.x[dest] += p.beta * flows[i];
                p.c[dest][dest] += (p.beta * 0.28).powi(2) * qvar[i];
            }
            let pred = p.x.iter().sum::<f64>();
            let mut err = y - pred;
            if y.abs() > 0.0665 && pred * y > y * y {
                err = 0.;
            }
            let pc: [f64; 3] = std::array::from_fn(|i| p.c[i].iter().sum());
            let v = pc.iter().sum::<f64>() + 0.00012f64.powi(2);
            p.lw += -0.5 * (err * err / v + v.ln());
            for i in 0..3 {
                let k = pc[i] / v;
                p.x[i] += k * err;
                for j in 0..3 {
                    p.c[i][j] -= k * pc[j];
                }
            }
            if p.id < 8 {
                let a = &self.actors[p.id];
                if a.invalid || (a.count > 0. && a.first > reopen * 0.52) {
                    p.lw = -1e9;
                }
            }
        }
        let bonus: [f64; 9] = std::array::from_fn(|i| {
            if i == 8 {
                return 0.;
            }
            let a = &self.actors[i];
            let pace = (a.last - a.first) / (a.count - 1.).max(1.);
            if a.count >= 2.
                && !a.invalid
                && pace < 34.
                && a.absvol < 12.
                && t - a.last > (pace * 2.2).max(40.)
            {
                100f64.ln()
            } else {
                0.
            }
        });
        let max = self
            .particles
            .iter()
            .map(|p| p.lw + bonus[p.id])
            .fold(f64::NEG_INFINITY, f64::max);
        let total = self
            .particles
            .iter()
            .map(|p| (p.lw + bonus[p.id] - max).exp())
            .sum::<f64>();
        self.probs = [0.; 9];
        self.temp = 0.;
        self.permanent = 0.;
        self.beta = 0.;
        self.half = 0.;
        self.opening = 0.;
        for p in &self.particles {
            let w = (p.lw + bonus[p.id] - max).exp() / total;
            self.probs[p.id] += w;
            self.temp += w * p.x[1];
            self.permanent += w * p.x[0];
            self.beta += w * p.beta;
            self.half += w * p.half;
            if p.id < 8 {
                let a = &self.actors[p.id];
                if a.count == 0. {
                    continue;
                }
                let pace = (a.last - a.first) / (a.count - 1.).max(1.);
                let age = t - a.last;
                let stopped = a.count >= 2. && age > (pace * 1.8).max(14.);
                let value = if stopped {
                    p.x[0]
                } else {
                    let lo = (a.sign * p.x[0]).clamp(0.005, 0.1199);
                    let tail = (0.12 - lo) / (0.12 / lo).ln();
                    a.sign * (0.6 * tail + 0.4 * lo)
                };
                self.opening += w * value;
            }
        }
        let mut flowrate = 0.;
        for a in &self.actors {
            if a.invalid || a.count < 2. {
                continue;
            }
            let pace = (a.last - a.first) / (a.count - 1.);
            if pace > 36. {
                continue;
            }
            let age = t - a.last;
            let active = (-((age / pace - 1.5).max(0.)) * 3.).exp();
            let role = 1. - 2f64.powf(1. - a.count);
            flowrate += role * active * a.sign * (a.absvol / a.count) / (pace + 1.);
        }
        self.future = self.beta * flowrate * 110.
            - self.temp * (1. - (-std::f64::consts::LN_2 * 110. / self.half).exp());
    }
}
