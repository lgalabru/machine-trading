//! Bayesian flow classification and price-impact filtering.
//!
//! Each of 45 Kalman filters hypothesizes an informed account (including none)
//! and a transient-impact half-life. The four latent states are permanent
//! impact, temporary impact, thin-book drift, and impact per share. All prices
//! here are fractional changes from the session's starting mark.
#[derive(Clone, Default, Debug)]
pub struct Flow {
    pub n: usize,
    pub net: f64,
    pub volume: f64,
    pub first: f64,
    pub last: f64,
    pub gaps: f64,
    pub gap2: f64,
    pub last_price: f64,
    pub last_size: f64,
    pub invalid: bool,
    pub role_invalid: bool,
    pub big_limits: usize,
    pub low_limits: usize,
}
impl Flow {
    pub fn add(&mut self, t: f64, q: f64, price: f64, aggression: f64, reopen: f64) {
        if self.n == 0 {
            self.first = t;
        } else {
            let d = t - self.last;
            if d > 48. {
                self.role_invalid = true;
            }
            self.gaps += d;
            self.gap2 += d * d;
        }
        if self.net * q < -0.001 {
            self.role_invalid = true;
        }
        if aggression < 2.4 {
            self.low_limits += 1;
        }
        if self.role_invalid || (self.n == 0 && (t > reopen * 0.51 || t < reopen * 0.015)) {
            self.invalid = true;
        }
        self.n += 1;
        self.net += q;
        self.volume += q.abs();
        self.last = t;
        self.last_price = price;
        self.last_size = q;
        if aggression > 4.1 {
            self.big_limits += 1;
        }
    }
    pub fn pace(&self) -> f64 {
        if self.n > 1 {
            (self.gaps / (self.n - 1) as f64).clamp(3., 30.)
        } else {
            15.
        }
    }
    pub fn active(&self, t: f64) -> f64 {
        let age = t - self.last;
        let pace = self.pace();
        if age < pace * 1.5 + 3. {
            1.
        } else {
            (-(age - pace * 1.5 - 3.) / (pace * 0.5 + 3.)).exp()
        }
    }
    pub fn directional(&self) -> f64 {
        if self.n == 0 || self.role_invalid || self.net.abs() < self.volume - 0.01 {
            return 0.;
        }
        let n = self.n as f64;
        // Same-direction, frequent sequences are unlikely to be eight-account noise.
        let evidence = (n - 1.) * 1.2 + self.big_limits as f64 * 2.5 - self.low_limits as f64 * 3.5
            + if self.n > 1 {
                (20. / self.pace()).ln() * (n - 1.) * 0.5
            } else {
                0.
            };
        1. / (1. + (2.6 - evidence).exp())
    }
}
#[derive(Clone)]
// `who == 8` is the hypothesis that no account is informed.
struct Filter {
    who: usize,
    lambda: f64,
    x: [f64; 4],
    p: [[f64; 4]; 4],
    ll: f64,
}
impl Filter {
    fn new(who: usize, hl: f64) -> Self {
        let mut p = [[0.; 4]; 4];
        p[2][2] = 0.0001f64.powi(2);
        p[3][3] = 0.00035f64.powi(2);
        Self {
            who,
            lambda: std::f64::consts::LN_2 / hl,
            x: [0., 0., 0., 0.0014],
            p,
            ll: 0.,
        }
    }
    fn advance(&mut self, dt: f64, flow: &[f64; 8], q2: &[f64; 8], y: f64, observe: bool) {
        let all: f64 = flow.iter().sum();
        let inf = if self.who < 8 { flow[self.who] } else { 0. };
        let a = (-dt * self.lambda).exp();
        let d = (-dt * std::f64::consts::LN_2 / 300.).exp();
        let f = [
            [1., 0., 0., inf],
            [0., a, 0., all - inf],
            [0., 0., d, 0.],
            [0., 0., 0., 1.],
        ];
        let mut x = [0.; 4];
        let mut fp = [[0.; 4]; 4];
        let mut p = [[0.; 4]; 4];
        for i in 0..4 {
            for j in 0..4 {
                x[i] += f[i][j] * self.x[j];
                for k in 0..4 {
                    fp[i][k] += f[i][j] * self.p[j][k];
                }
            }
        }
        for i in 0..4 {
            for j in 0..4 {
                for k in 0..4 {
                    p[i][j] += fp[i][k] * f[j][k];
                }
            }
        }
        let qi = if self.who < 8 { q2[self.who] } else { 0. };
        let qt: f64 = q2.iter().sum();
        let impact = self.x[3].clamp(0.0007, 0.0022);
        p[0][0] += qi * (impact * 0.28).powi(2);
        p[1][1] += (qt - qi) * (impact * 0.28).powi(2);
        p[2][2] += dt * 0.0003f64.powi(2) / 12.;
        p[3][3] += dt * 0.000001f64.powi(2);
        if observe && y.abs() < 0.0664 {
            let v = y - x[0] - x[1] - x[2];
            let r = 0.00016f64.powi(2);
            let mut ph = [0.; 4];
            for i in 0..4 {
                ph[i] = p[i][0] + p[i][1] + p[i][2];
            }
            let s = (ph[0] + ph[1] + ph[2] + r).max(1e-12);
            // Robust observation likelihood: slight book distortion must not eliminate a model.
            self.ll += -0.5 * (s.ln() + (v * v / s).min(36.));
            for i in 0..4 {
                x[i] += ph[i] / s * v;
                for j in 0..4 {
                    p[i][j] -= ph[i] * ph[j] / s;
                }
            }
        }
        self.x = x;
        self.p = p;
    }
}
#[derive(Clone)]
pub struct Model {
    pub flows: [Flow; 8],
    filters: Vec<Filter>,
    pub probs: [f64; 9],
    pub reopen: f64,
    pub t: f64,
    pub price: f64,
    pub alpha: f64,
    pub transient: f64,
    pub anchor: f64,
    pub fair: f64,
    pub forecast: f64,
    pub signal: f64,
    pub target: f64,
    pub confidence: f64,
    pub pending: Vec<(f64, usize, f64)>,
    pub min_price: f64,
    pub max_price: f64,
    pub horizon: f64,
    pub reopen_start: f64,
    pub reopen_full: f64,
    pub sizing: f64,
    pub hysteresis: f64,
}
impl Model {
    pub fn new(reopen: f64) -> Self {
        let filters = (0..9)
            .flat_map(|who| [65., 90., 125., 170., 230.].map(move |hl| Filter::new(who, hl)))
            .collect();
        Self {
            flows: Default::default(),
            filters,
            probs: [0.; 9],
            reopen,
            t: 0.,
            price: 0.,
            alpha: 0.0014,
            transient: 0.,
            anchor: 0.,
            fair: 0.,
            forecast: 0.,
            signal: 0.,
            target: 0.,
            confidence: 0.,
            pending: Vec::new(),
            min_price: 0.,
            max_price: 0.,
            horizon: 50.,
            reopen_start: 25.,
            reopen_full: 8.,
            sizing: 1.5,
            hysteresis: 1.1,
        }
    }
    pub fn trade(&mut self, t: f64, account: usize, q: f64, aggression: f64) {
        if account >= 8 {
            return;
        }
        self.flows[account].add(t, q, self.price, aggression, self.reopen);
        let effective = t;
        self.pending.push((effective, account, q));
    }
    pub fn update(&mut self, t: f64, y: f64, observe: bool) {
        let dt = (t - self.t).max(0.);
        self.t = t;
        self.price = y;
        if t < self.reopen {
            self.min_price = self.min_price.min(y);
            self.max_price = self.max_price.max(y);
        }
        let mut u = [0.; 8];
        let mut q2 = [0.; 8];
        self.pending.retain(|&(at, i, q)| {
            if at <= t {
                u[i] += q;
                q2[i] += q * q;
                false
            } else {
                true
            }
        });
        // Transactions precede their full book impact; assimilate only settled observations.
        let last_flow = self.flows.iter().map(|f| f.last).fold(0., f64::max);
        for f in &mut self.filters {
            f.advance(dt, &u, &q2, y, observe && t - last_flow >= 4.);
        }
        // Marginalize over at most one worker and one informed account.
        let mut info_lr = [0.; 8];
        let mut worker_lr = [0.; 8];
        for i in 0..8 {
            let flow = &self.flows[i];
            let lr = if flow.n == 0 {
                if t < self.reopen * 0.5 { 1. } else { 0.05 }
            } else if flow.role_invalid {
                0.
            } else {
                let n = flow.n as f64;
                let cv = if n > 2. {
                    (flow.gap2 / (n - 1.) / (flow.pace() * flow.pace()) - 1.)
                        .max(0.)
                        .sqrt()
                } else {
                    0.
                };
                (-1.5 + (n - 1.) * (2. / (0.012 * flow.pace())).ln() + flow.big_limits as f64 * 2.5
                    - flow.low_limits as f64 * 3.5
                    - (cv - 0.55).max(0.) * n * 2.)
                    .min(28.)
                    .exp()
            };
            info_lr[i] = if flow.invalid { 0. } else { lr };
            worker_lr[i] = if flow.volume > 50.1
                || flow.first > self.reopen * 0.61
                || (flow.n > 0 && flow.volume < 11.8 && t - flow.last > 48.)
            {
                0.
            } else {
                lr
            };
        }
        let worker_sum: f64 = worker_lr.iter().sum();
        let mut weights = Vec::with_capacity(self.filters.len());
        for f in &self.filters {
            let prior = if f.who == 8 {
                (0.4 * (0.4 + 0.6 / 8. * worker_sum)).max(1e-100).ln()
            } else {
                ((0.6 / 8.) * info_lr[f.who] * (0.4 + 0.6 / 7. * (worker_sum - worker_lr[f.who])))
                    .max(1e-100)
                    .ln()
            };
            weights.push(f.ll + prior);
        }
        let max = weights.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        for w in &mut weights {
            *w = (*w - max).exp();
        }
        let z: f64 = weights.iter().sum();
        self.probs = [0.; 9];
        self.alpha = 0.;
        self.transient = 0.;
        self.anchor = 0.;
        self.forecast = 0.;
        self.fair = 0.;
        let remaining = (self.reopen - t).max(0.);
        let horizon = remaining.min(self.horizon);
        for (f, w) in self.filters.iter().zip(weights.iter()) {
            let w = w / z;
            self.probs[f.who] += w;
            self.alpha += w * f.x[3];
            self.transient += w * f.x[1];
            self.anchor += w * f.x[0];
            let mut pred = f.x[0]
                + f.x[1] * (-horizon * f.lambda).exp()
                + f.x[2] * (-horizon * std::f64::consts::LN_2 / 300.).exp();
            for i in 0..8 {
                let flow = &self.flows[i];
                if flow.n == 0 {
                    continue;
                }
                let active = flow.active(t);
                let dir = flow.directional();
                let rate = (flow.volume / flow.n as f64) / flow.pace();
                let more = if i == f.who {
                    rate * horizon * active
                } else {
                    let remaining_volume = (50. - flow.volume).clamp(0., 25.);
                    (rate * horizon).min(remaining_volume) * active * dir
                };
                let scale = if i == f.who {
                    1.
                } else {
                    (1. - (-horizon * f.lambda).exp()) / (horizon * f.lambda).max(1e-6)
                };
                pred += flow.net.signum() * more * f.x[3] * scale;
            }
            for &(_, _, q) in &self.pending {
                pred += q * f.x[3];
            }
            self.forecast += w * pred.clamp(-0.06667, 0.06667);
            if f.who < 8 {
                let flow = &self.flows[f.who];
                if flow.n > 0 {
                    let dir = flow.net.signum();
                    let active = flow.active(t);
                    // A completed informed sequence estimates the permanent component,
                    // not the book price distorted by workers and noise.
                    let last = f.x[0];
                    let lo = (dir * f.x[0]).max(0.005).min(0.115);
                    let beyond = (0.12 - lo) / (0.12 / lo).ln();
                    let fair = active * dir * beyond + (1. - active) * last;
                    self.fair += w * fair;
                }
            }
        }
        self.alpha = self.alpha.clamp(0.0007, 0.0022);
        let mix = ((self.reopen_start - remaining) / (self.reopen_start - self.reopen_full))
            .clamp(0., 1.);
        self.signal = (1. - mix) * (self.forecast - y) + mix * (self.fair - y);
        self.confidence = self.probs.iter().take(8).copied().fold(0., f64::max);
    }
    pub fn decide(
        &mut self,
        position: f64,
        halfspread: f64,
        dd: f64,
        mark: f64,
        start: f64,
    ) -> f64 {
        let remaining = self.reopen - self.t;
        if remaining <= 0. {
            self.target = position;
            return position;
        }
        let mut cap: f64 = 69.5;
        if remaining < 24. {
            // Stress the reopen against every still-plausible account direction,
            // including quiet-weekend and surprise allowances. This is a risk
            // estimate, not a guarantee against an unbounded surprise distribution.
            let mut low: f64 = -0.06;
            let mut high: f64 = 0.06;
            for flow in &self.flows {
                if flow.n == 0 {
                    low = low.min(self.min_price - 0.04);
                    high = high.max(self.max_price + 0.04);
                } else if !flow.invalid {
                    if self.t - flow.last <= 48. {
                        if flow.net > 0. {
                            high = 0.15;
                        } else {
                            low = -0.15;
                        }
                    } else {
                        if flow.net > 0. {
                            high = high.max(0.105);
                        } else {
                            low = low.min(-0.105);
                        }
                    }
                }
            }
            let adverse = if self.signal >= 0. {
                mark - start * (1. + low)
            } else {
                start * (1. + high) - mark
            };
            cap = cap.min((850. - dd).max(0.) / (adverse + start * 0.002).max(start * 0.01));
        }
        if dd > 600. {
            cap = cap.min((800. - dd).max(0.) / (start * 0.04));
        }
        let threshold = halfspread + 0.0005;
        let conviction = (self.signal / (threshold * self.sizing)).clamp(-1., 1.);
        let desired = conviction * cap;
        // No trade within the spread-sized inaction region.
        let pos_signal = position / cap.max(1.) * threshold * self.sizing;
        let out = if (self.signal - pos_signal).abs() < threshold * self.hysteresis {
            position.clamp(-cap, cap)
        } else {
            desired
        };
        self.target = out;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_mixed_and_interrupted_directional_sequences() {
        let mut f = Flow::default();
        f.add(20., 1., 0., 4.5, 500.);
        f.add(32., -0.5, 0., 4.5, 500.);
        assert!(f.invalid && f.role_invalid);
        assert_eq!(f.directional(), 0.);
        let mut f = Flow::default();
        f.add(20., 1., 0., 4.5, 500.);
        f.add(70., 1., 0., 4.5, 500.);
        assert!(f.role_invalid);
    }
    #[test]
    fn reopening_retains_position_without_crossing_the_spread() {
        let mut m = Model::new(500.);
        m.update(502., 0.02, true);
        assert_eq!(m.decide(31., 0.001, 0., 143., 143.), 31.);
    }
    #[test]
    fn adverse_reopen_direction_reduces_exposure() {
        let mut m = Model::new(500.);
        m.trade(200., 0, -1., 4.5);
        m.trade(480., 1, -1., 4.5);
        // Use an eligible recent seller: its start was inside the informed window.
        m.flows[0] = Flow::default();
        m.flows[0].add(200., -1., 0., 4.5, 500.);
        m.flows[0].last = 480.;
        m.t = 490.;
        m.signal = 0.03;
        let target = m.decide(0., 0.001, 300., 143., 143.);
        assert!(target > 0. && target < 26.);
    }
    #[test]
    fn bounded_finite_positions_under_mixed_flow_and_price_jumps() {
        let mut m = Model::new(500.);
        let mut position = 0.;
        for t in (2..=650).step_by(2) {
            if t < 500 && t % 6 == 0 {
                m.trade(
                    t as f64,
                    (t / 6) % 8,
                    if t % 12 == 0 { 2.5 } else { -1.7 },
                    3.5,
                );
            }
            let y = if t >= 500 {
                -0.12
            } else {
                0.04 * (t as f64 / 43.).sin()
            };
            m.update(t as f64, y, true);
            position = m.decide(position, 0.0012, 150., 143. * (1. + y), 143.);
            assert!(position.is_finite() && position.abs() <= 69.500001);
        }
    }
}
