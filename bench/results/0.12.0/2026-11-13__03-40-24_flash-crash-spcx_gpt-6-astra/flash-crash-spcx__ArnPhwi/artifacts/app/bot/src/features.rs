//! Causal features: only observations already delivered by the session are used.
const LAGS: [usize; 13] = [1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96];
const TAUS: [f64; 4] = [4., 12., 32., 80.];
const WINDOWS: [usize; 4] = [8, 24, 64, 128];
#[derive(Default)]
struct Segment {
    start: usize,
    end: usize,
    base: f64,
    dir: f64,
    finish: f64,
}
#[derive(Default)]
pub struct History {
    history: Vec<f64>,
    emas: [f64; 4],
    events: Vec<(usize, f64)>,
    pub sign: f64,
    max: f64,
    min: f64,
    maxt: usize,
    mint: usize,
    big: f64,
    bigt: usize,
    bigbase: f64,
    segments: Vec<Segment>,
    strong: f64,
    strongt: usize,
    first: Option<usize>,
    firstdir: f64,
    steps: u64,
}
impl History {
    pub fn new(steps: u64) -> Self {
        Self {
            steps,
            sign: 1.,
            ..Self::default()
        }
    }
    pub fn observe(&mut self, p: f64) -> [f64; 70] {
        let k = self.history.len();
        let prev = self.history.last().copied().unwrap_or(0.0);
        let r = (p - prev) * 10000.;
        if r.abs() > 25. {
            self.sign = r.signum();
            self.events.insert(0, (k, r));
            self.events.truncate(5);
        }
        if r.abs() > self.big {
            self.big = r.abs();
            self.bigt = k;
            self.bigbase = if k == 0 { p } else { prev };
        }
        if p > self.max {
            self.max = p;
            self.maxt = k;
        }
        if p < self.min {
            self.min = p;
            self.mint = k;
        }
        self.history.push(p);
        let mut x = [0.; 70];
        x[0] = k as f64;
        x[1] = self.steps as f64 - k as f64;
        x[2] = p * 10000. * self.sign;
        let mut j = 3;
        for lag in LAGS {
            let old = if k >= lag { self.history[k - lag] } else { 0. };
            x[j] = (p - old) * 10000. * self.sign;
            j += 1;
        }
        for i in 0..4 {
            self.emas[i] += (p - self.emas[i]) / TAUS[i];
            x[j] = (p - self.emas[i]) * 10000. * self.sign;
            j += 1;
        }
        for w in WINDOWS {
            let from = (k + 1).saturating_sub(w);
            let mut hi = if k + 1 < w { 0. } else { p };
            let mut lo = hi;
            for &v in &self.history[from..] {
                hi = hi.max(v);
                lo = lo.min(v);
            }
            x[j] = (p - hi) * 10000. * self.sign;
            x[j + 1] = (p - lo) * 10000. * self.sign;
            j += 2;
        }
        for i in 0..5 {
            if let Some(&(t, r)) = self.events.get(i) {
                x[j] = r * self.sign;
                x[j + 1] = (k - t) as f64;
            } else {
                x[j + 1] = 500.;
            }
            j += 2;
        }
        x[j] = self.big;
        x[j + 1] = (k - self.bigt) as f64;
        x[j + 2] = (p - self.bigbase) * 10000. * self.sign;
        x[j + 3] = (p - self.max) * 10000. * self.sign;
        x[j + 4] = (p - self.min) * 10000. * self.sign;
        x[j + 5] = (k - if self.sign > 0. { self.maxt } else { self.mint }) as f64;
        // Canonicalize extrema in the direction of the most recent move.
        if self.sign < 0. {
            for idx in [20, 22, 24, 26, 41] {
                x.swap(idx, idx + 1);
            }
        }
        if r.abs() > 25. {
            let fresh = self
                .segments
                .first()
                .map(|s| s.dir != r.signum() || k - s.end > 14)
                .unwrap_or(true);
            if fresh {
                self.segments.insert(
                    0,
                    Segment {
                        start: k,
                        end: k,
                        base: if k == 0 { p } else { prev },
                        dir: r.signum(),
                        finish: p,
                    },
                );
                self.segments.truncate(4);
            } else if let Some(s) = self.segments.first_mut() {
                s.end = k;
                s.finish = p;
            }
        }
        for i in 0..4 {
            let j = 44 + i * 5;
            if let Some(s) = self.segments.get(i) {
                x[j] = (s.finish - s.base) * 10000. * self.sign;
                x[j + 1] = (k - s.start) as f64;
                x[j + 2] = (k - s.end) as f64;
                x[j + 3] = (p - s.base) * 10000. * self.sign;
                x[j + 4] = (p - s.finish) * 10000. * self.sign;
            } else {
                x[j + 1] = 500.;
                x[j + 2] = 500.;
            }
        }
        if p.abs() > self.strong.abs() {
            self.strong = p;
            self.strongt = k;
        }
        if self.first.is_none() && p.abs() > 0.005 {
            self.first = Some(k);
            self.firstdir = p.signum();
        }
        x[64] = self.strong * 10000. * self.sign;
        x[65] = (k - self.strongt) as f64;
        x[66] = if self.strong.abs() > 1e-6 {
            p / self.strong
        } else {
            0.
        };
        x[67] = self.first.map(|f| (k - f) as f64).unwrap_or(500.);
        x[68] = self.firstdir * self.sign;
        if self.segments.len() > 1 {
            let a = &self.segments[0];
            let b = &self.segments[1];
            let den = b.finish - b.base;
            if den.abs() > 1e-6 {
                x[69] = (a.finish - a.base) / den;
            }
        }
        x
    }
}
include!("features_golden.rs");
