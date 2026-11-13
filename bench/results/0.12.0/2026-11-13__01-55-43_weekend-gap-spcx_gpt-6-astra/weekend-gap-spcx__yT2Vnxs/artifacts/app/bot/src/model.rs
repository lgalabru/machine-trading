//! Online order-flow filter. All observations are prices and already executed orders.
//!
//! A bank of three-state Kalman filters separates permanent informed impact,
//! decaying inventory pressure, and slow book drift. It marginalizes over the
//! informed account, temporary-impact half-life, and impact per share.
//! Returns and price displacements below are fractions of the starting mark.
#[derive(Clone, Copy, Default, Debug)]
pub struct Trade {
    pub t: u64,
    pub actor: usize,
    pub size: f64,
    pub price: f64,
}
#[derive(Clone, Default, Debug)]
pub struct Actor {
    pub n: usize,
    pub buy: usize,
    pub sell: usize,
    pub volume: f64,
    pub first: u64,
    pub last: u64,
    pub gap: f64,
    pub last_price: f64,
    pub last_size: f64,
    pub max_after: f64,
    pub max_gap: f64,
    pub min_gap: f64,
    pub small: bool,
}
impl Actor {
    pub fn direction(&self) -> f64 {
        if self.max_gap > 46. || self.small {
            return 0.;
        }
        if self.buy > 0 && self.sell == 0 {
            1.0
        } else if self.sell > 0 && self.buy == 0 {
            -1.0
        } else {
            0.0
        }
    }
    pub fn pace(&self) -> f64 {
        if self.n > 1 {
            ((self.last - self.first) as f64 / (self.n - 1) as f64).clamp(3., 30.)
        } else {
            15.
        }
    }
    pub fn likelihood(&self) -> f64 {
        if self.n == 0 {
            1.
        } else if self.direction() == 0. {
            0.000001
        } else {
            (2f64.powi((self.n as i32 - 1).min(22)) * 6f64.powi((self.n as i32 - 1).min(10)))
                .min(1e12)
        }
    }
    pub fn active(&self, t: u64) -> f64 {
        if self.n < 2 {
            return 0.;
        }
        let age = (t - self.last) as f64;
        let p = self.pace();
        if age < 1.35 * p {
            1.
        } else {
            (-(age / p - 1.35) * 2.).exp()
        }
    }
}
#[derive(Clone)]
struct Filter {
    who: usize,
    half: f64,
    impact: f64,
    x: [f64; 3],
    v: [[f64; 3]; 3],
    score: f64,
}
impl Filter {
    fn new(who: usize, half: f64, impact: f64) -> Self {
        Self {
            who,
            half,
            impact,
            x: [0.; 3],
            v: [[0., 0., 0.], [0., 0., 0.], [0., 0., 0.0000003]],
            score: 0.,
        }
    }
    fn predict(&mut self, dt: f64, flows: &[f64; 8], squares: &[f64; 8], impact: f64) {
        let a = [
            1.,
            (-dt * std::f64::consts::LN_2 / self.half).exp(),
            (-dt * std::f64::consts::LN_2 / 300.).exp(),
        ];
        for i in 0..3 {
            self.x[i] *= a[i];
            for j in 0..3 {
                self.v[i][j] *= a[i] * a[j];
            }
        }
        for i in 0..8 {
            let j = if i == self.who { 0 } else { 1 };
            self.x[j] += flows[i] * impact;
            self.v[j][j] += squares[i] * (impact * 0.32).powi(2);
        }
        self.v[2][2] += dt * 1.2e-8;
    }
    fn observe(&mut self, p: f64, noise: f64) {
        let e = p - self.x.iter().sum::<f64>();
        let mut cov = [0.; 3];
        for i in 0..3 {
            cov[i] = self.v[i].iter().sum();
        }
        let var = (cov.iter().sum::<f64>() + noise).max(1e-10);
        self.score = 0.999 * self.score - 0.5 * ((e * e / var).min(20.) + var.ln());
        let damping = 1f64.min(4. * var.sqrt() / e.abs().max(1e-10));
        for i in 0..3 {
            self.x[i] += cov[i] / var * e * damping;
            for j in 0..3 {
                self.v[i][j] -= cov[i] * cov[j] / var;
            }
        }
    }
}
pub struct Model {
    pub actors: [Actor; 8],
    filters: Vec<Filter>,
    pending: Vec<Trade>,
    pub impact: f64,
    pub info: [f64; 9],
    pub temporary: f64,
    pub fundamental: f64,
    pub edge: f64,
    pub flow: f64,
    pub decay: f64,
    pub p: f64,
    pub t: u64,
    pub reopen: u64,
    pub warm_p: f64,
    pub warm_t: u64,
    recent: Vec<Trade>,
    pub cap: f64,
}
impl Model {
    pub fn new(reopen: u64) -> Self {
        let mut filters = Vec::new();
        for c in 0..9 {
            for h in [60., 85., 115., 150., 190., 240.] {
                for impact in [0.0008, 0.0010, 0.0012, 0.0014, 0.0016, 0.0018, 0.0020] {
                    filters.push(Filter::new(c, h, impact));
                }
            }
        }
        Self {
            actors: Default::default(),
            filters,
            pending: vec![],
            impact: 0.0014,
            info: [0.; 9],
            temporary: 0.,
            fundamental: 0.,
            edge: 0.,
            flow: 0.,
            decay: 0.,
            p: 0.,
            t: 0,
            reopen,
            warm_p: 0.,
            warm_t: 0,
            recent: vec![],
            cap: 70.,
        }
    }
    pub fn trade(&mut self, tr: Trade) {
        if tr.actor >= 8 {
            return;
        }
        let a = &mut self.actors[tr.actor];
        if a.n == 0 {
            a.first = tr.t;
            a.min_gap = 1000.;
        } else {
            a.gap = (tr.t - a.last) as f64;
            a.max_gap = a.max_gap.max(a.gap);
            a.min_gap = a.min_gap.min(a.gap);
        }
        a.small |= tr.size.abs() < 0.195;
        a.n += 1;
        if tr.size > 0. {
            a.buy += 1
        } else {
            a.sell += 1
        };
        a.volume += tr.size.abs();
        a.last = tr.t;
        a.last_price = tr.price;
        a.last_size = tr.size;
        a.max_after = tr.price * tr.size.signum();
        self.pending.push(tr);
        self.recent.push(tr);
    }
    pub fn update(&mut self, t: u64, p: f64) {
        let dt = t.saturating_sub(self.t) as f64;
        self.t = t;
        self.p = p;
        for a in &mut self.actors {
            if a.n > 0 {
                a.max_after = a.max_after.max(p * a.direction());
            }
        }
        let mut flows = [0.; 8];
        let mut sq = [0.; 8];
        self.pending.retain(|tr| {
            if t >= tr.t + 2 {
                flows[tr.actor] += tr.size;
                sq[tr.actor] += tr.size * tr.size;
                false
            } else {
                true
            }
        });
        for f in &mut self.filters {
            f.predict(dt, &flows, &sq, f.impact);
        }
        if t % 8 < 2 {
            let recent = self
                .recent
                .iter()
                .filter(|tr| t - tr.t < 4)
                .map(|tr| tr.size.abs())
                .sum::<f64>();
            let noise = 1.5e-8 + (recent * self.impact * 0.65).powi(2);
            for f in &mut self.filters {
                f.observe(p, noise);
            }
        }
        // Retain recent flow only while it can affect measurement uncertainty.
        if t - self.warm_t >= 12 && self.recent.iter().all(|tr| t - tr.t >= 4) {
            self.warm_t = t;
            self.warm_p = p;
            self.recent.clear();
        }
        let likes: Vec<f64> = self.actors.iter().map(|a| a.likelihood()).collect();
        let worker: Vec<f64> = self
            .actors
            .iter()
            .zip(&likes)
            .map(|(a, l)| {
                if a.volume > 50.5 || (a.n > 0 && a.volume < 11.8 && t - a.last > 46) {
                    l * 0.0001
                } else {
                    *l
                }
            })
            .collect();
        let mut scores = vec![];
        for f in &self.filters {
            let prior = if f.who == 8 {
                0.4 * (0.4 + 0.6 * worker.iter().sum::<f64>() / 8.)
            } else {
                let inf = if self.actors[f.who].first > self.reopen * 52 / 100 {
                    1e-8
                } else {
                    likes[f.who]
                };
                0.6 / 8.
                    * inf
                    * (0.4
                        + 0.6
                            * worker
                                .iter()
                                .enumerate()
                                .filter(|(i, _)| *i != f.who)
                                .map(|(_, l)| l)
                                .sum::<f64>()
                            / 7.)
            };
            scores.push(f.score + prior.max(1e-30).ln());
        }
        let max = scores.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let z = scores.iter().map(|s| (s - max).exp()).sum::<f64>();
        self.info = [0.; 9];
        self.temporary = 0.;
        self.impact = 0.;
        let mut decay = 0.;
        for (f, s) in self.filters.iter().zip(&scores) {
            let w = (s - max).exp() / z;
            self.info[f.who] += w;
            self.impact += w * f.impact;
            self.temporary += w * f.x[1];
            decay += w * f.x[1] * (1. - (-60. * std::f64::consts::LN_2 / f.half).exp());
        }
        self.fundamental = 0.;
        let mut flow = 0.;
        for (i, a) in self.actors.iter().enumerate() {
            let dir = a.direction();
            if dir == 0. || a.n == 0 {
                continue;
            }
            let active = a.active(t);
            let directional = (1. - 1. / a.likelihood().max(1.)).clamp(0., 1.);
            let vol = (a.volume / a.n as f64).clamp(0.5, 1.5);
            let worker_remaining = (50. - a.volume).max(0.);
            let next_volume = (60. / a.pace() * vol).min(worker_remaining.max(60. * self.info[i]));
            flow += dir * directional * active * next_volume * self.impact;
        }
        for (f, score) in self.filters.iter().zip(&scores) {
            if f.who >= 8 {
                continue;
            }
            let a = &self.actors[f.who];
            let dir = a.direction();
            if dir == 0. || a.n == 0 {
                continue;
            }
            let w = (score - max).exp() / z;
            let active = a.active(t);
            let reached = f.x[0] * dir;
            let bound = (reached + 0.3 * f.impact).clamp(0.005, 0.06667);
            let continuation = (0.12 - bound) / (0.12 / bound).ln();
            self.fundamental += w * dir * (active * continuation + (1. - active) * reached);
        }
        // Unobserved informed traders have a symmetric prior.
        self.flow = flow;
        self.decay = decay;
        self.edge = flow - decay;
    }
    pub fn opening_bounds(&self) -> (f64, f64) {
        // Quiet-weekend range plus 4.5 times the largest reopening surprise SD.
        let mut low: f64 = -0.057;
        let mut high: f64 = 0.057;
        for a in &self.actors {
            let dir = a.direction();
            if dir == 0. || a.n == 0 || a.first > self.reopen * 52 / 100 {
                continue;
            }
            // A still-active directional account can be trading toward the full news range.
            let extent = if self.t.saturating_sub(a.last) <= 46 {
                0.147
            } else {
                (a.volume * 0.003 + 0.003).min(0.12) + 0.027
            };
            if dir > 0. {
                high = high.max(extent);
            } else {
                low = low.min(-extent);
            }
        }
        (low, high)
    }
    pub fn target(&self, pos: f64, dd: f64, start: f64, mark: f64) -> f64 {
        let left = self.reopen.saturating_sub(self.t);
        if self.t >= self.reopen {
            return pos;
        }
        let mut target;
        if left > 16 {
            let edge = self.flow - 1.5 * self.decay;
            target = if edge > 0.0025 {
                self.cap
            } else if edge < -0.0025 {
                -self.cap
            } else {
                pos
            };
        } else {
            let edge = self.fundamental - self.p;
            target = if edge > 0.0015 {
                self.cap
            } else if edge < -0.0015 {
                -self.cap
            } else {
                pos
            };
            // Budget the unknown reopening gap, including a large surprise.
            let (low, high) = self.opening_bounds();
            // Include the mark-to-book cost of changing inventory, and bound execution
            // by the order limit. This also handles flipping a position before reopening.
            let direction = target.signum();
            let adverse = if direction > 0. {
                self.p - low
            } else {
                high - self.p
            };
            let available = 880. - dd + pos * start * (self.p - (mark / start - 1.));
            let risk = |amount: f64| {
                amount * start * adverse.max(0.02)
                    + (direction * amount - pos).abs() * start * 0.0065
            };
            let (mut lower, mut upper) = (0., self.cap);
            for _ in 0..32 {
                let middle = (lower + upper) * 0.5;
                if risk(middle) <= available {
                    lower = middle;
                } else {
                    upper = middle;
                }
            }
            target = direction * target.abs().min(lower);
        }
        if dd > 650. {
            target = 0.;
        }
        target.clamp(-self.cap, self.cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn no_signal_does_not_open_a_position() {
        let mut m = Model::new(500);
        for t in (2..400).step_by(2) {
            m.update(t, 0.);
            assert_eq!(m.target(0., 0., 143., 143.), 0.);
            assert!(m.info.iter().all(|v| v.is_finite()));
        }
    }
    #[test]
    fn two_sided_and_irregular_flow_is_not_a_directional_account() {
        let mut m = Model::new(500);
        m.trade(Trade {
            t: 10,
            actor: 0,
            size: 1.,
            price: 0.,
        });
        m.trade(Trade {
            t: 20,
            actor: 0,
            size: -1.,
            price: 0.,
        });
        assert_eq!(m.actors[0].direction(), 0.);
        m.trade(Trade {
            t: 10,
            actor: 1,
            size: 1.,
            price: 0.,
        });
        m.trade(Trade {
            t: 80,
            actor: 1,
            size: 1.,
            price: 0.,
        });
        assert_eq!(m.actors[1].direction(), 0.);
    }
    #[test]
    fn reopening_budget_includes_inventory_change_and_execution() {
        let mut m = Model::new(500);
        m.t = 498;
        for pos in [-69., -30., 0., 30., 69.] {
            for p in [-0.06, -0.02, 0., 0.02, 0.06] {
                for mark_return in [-0.04, 0., 0.04] {
                    for dd in [0., 200., 500.] {
                        for fair in [-0.1, 0., 0.1] {
                            m.p = p;
                            m.fundamental = fair;
                            let q = m.target(pos, dd, 143., 143. * (1. + mark_return));
                            assert!(q.is_finite() && q.abs() <= 70.);
                            let (lo, hi) = m.opening_bounds();
                            let dir = q.signum();
                            let adverse = if dir > 0. { p - lo } else { hi - p };
                            let available = 880. - dd + pos * 143. * (p - mark_return);
                            let risk = q.abs() * 143. * adverse.max(0.02)
                                + (q - pos).abs() * 143. * 0.0065;
                            if pos.abs() * 143. * 0.0065 <= available {
                                assert!(
                                    risk <= available + 1e-6,
                                    "q={q}, risk={risk}, available={available}"
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn drawdown_guard_flattens() {
        let mut m = Model::new(500);
        m.t = 200;
        m.flow = 0.05;
        assert_eq!(m.target(70., 651., 143., 143.), 0.);
    }
}
