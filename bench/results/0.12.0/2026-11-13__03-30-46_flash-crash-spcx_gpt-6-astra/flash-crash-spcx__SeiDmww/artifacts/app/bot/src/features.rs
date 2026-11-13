//! Causal features in percentage points and market seconds. A step is 0.5 s.
//! Event history separates ongoing legs from pauses and partial reversals.

#[derive(Default)]
struct Event {
    sign: f64,
    amount: f64,
    first: usize,
    last: usize,
    base: f64,
}

pub struct Features {
    prices: Vec<f64>,
    high: f64,
    low: f64,
    hiat: usize,
    loat: usize,
    up: [i64; 4],
    dn: [i64; 4],
    events: Vec<Event>,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            prices: Vec::new(), high: 0.0, low: 0.0, hiat: 0, loat: 0,
            up: [-200; 4], dn: [-200; 4], events: Vec::new(),
        }
    }
}

impl Features {
    pub fn next(&mut self, p: f64, left: u64) -> Vec<f64> {
        let k = self.prices.len();
        let prev = self.prices.last().copied().unwrap_or(0.0);
        let dp = p - prev;
        self.prices.push(p);

        for (j, threshold) in [0.15, 0.3, 0.6, 1.2].iter().enumerate() {
            if dp > *threshold { self.up[j] = k as i64; }
            if dp < -*threshold { self.dn[j] = k as i64; }
        }
        if p > self.high { self.high = p; self.hiat = k; }
        if p < self.low { self.low = p; self.loat = k; }

        if dp.abs() > 0.22 {
            let sign = dp.signum();
            let extend = self.events.last()
                .map(|e| e.sign == sign && k - e.last <= 16)
                .unwrap_or(false);
            if extend {
                if let Some(event) = self.events.last_mut() {
                    event.amount += dp;
                    event.last = k;
                }
            } else {
                self.events.push(Event { sign, amount: dp, first: k, last: k, base: prev });
            }
        }

        let mut x = vec![
            k as f64 / 2.0, left as f64 / 2.0, p, self.high, self.low,
            (k - self.hiat) as f64 / 2.0, (k - self.loat) as f64 / 2.0,
        ];
        for lag in [1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128] {
            x.push(p - if k >= lag { self.prices[k - lag] } else { 0.0 });
        }
        for window in [8, 24, 64, 160] {
            let history = &self.prices[(k + 1).saturating_sub(window)..];
            x.push(p - history.iter().copied().fold(f64::INFINITY, f64::min));
            x.push(p - history.iter().copied().fold(f64::NEG_INFINITY, f64::max));
        }
        for j in 0..4 {
            x.push((k as i64 - self.up[j]).min(200) as f64 / 2.0);
            x.push((k as i64 - self.dn[j]).min(200) as f64 / 2.0);
        }
        for j in 0..4 {
            if j < self.events.len() {
                let event = &self.events[self.events.len() - 1 - j];
                x.extend([
                    event.amount, (k - event.first).min(200) as f64 / 2.0,
                    (k - event.last).min(200) as f64 / 2.0, p - event.base,
                ]);
            } else {
                x.extend([0.0, 100.0, 100.0, 0.0]);
            }
        }
        x
    }
}

/// Mirror negative excursions so both shock directions share one forecast.
pub fn canonical(x: &[f64]) -> (Vec<f64>, f64) {
    let mut a = x.to_vec();
    let sign = if a[2] < 0.0 { -1.0 } else { 1.0 };
    if sign < 0.0 {
        a[2] = -a[2];
        a[3] = -x[4];
        a[4] = -x[3];
        a.swap(5, 6);
        for j in 7..21 { a[j] = -a[j]; }
        for j in (21..29).step_by(2) { a[j] = -x[j + 1]; a[j + 1] = -x[j]; }
        for j in (29..37).step_by(2) { a.swap(j, j + 1); }
        for j in (37..53).step_by(4) { a[j] = -a[j]; a[j + 3] = -a[j + 3]; }
    }
    (a, sign)
}
