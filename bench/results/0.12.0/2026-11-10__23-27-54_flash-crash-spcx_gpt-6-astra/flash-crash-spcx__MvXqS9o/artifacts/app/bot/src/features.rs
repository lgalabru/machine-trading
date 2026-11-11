//! Causal price features. Every value uses only observations already received.
const LAGS: [usize; 13] = [1, 2, 3, 4, 6, 8, 12, 16, 24, 40, 60, 80, 120];
const WINS: [usize; 5] = [8, 20, 60, 120, 400];
const THR: [f64; 4] = [0.10, 0.25, 0.5, 1.0];

pub struct History {
    pub prices: Vec<f64>,
    last_amp: [f64; 4],
    last_time: [i64; 4],
    event_amp: f64,
    event_start: i64,
    event_last: i64,
    event_base: f64,
    event_dir: f64,
    event_peak: f64,
    prev_amp: f64,
    prev_time: i64,
    prev2_amp: f64,
    prev2_time: i64,
    ema: [f64; 3],
    biggest: f64,
    big_time: i64,
    last_change: f64,
    change_at: i64,
    orientation: f64,
    count: f64,
}
impl Default for History {
    fn default() -> Self {
        Self {
            prices: Vec::with_capacity(400),
            last_amp: [0.; 4],
            last_time: [-400; 4],
            event_amp: 0.,
            event_start: 0,
            event_last: -400,
            event_base: 0.,
            event_dir: 0.,
            event_peak: 0.,
            prev_amp: 0.,
            prev_time: -400,
            prev2_amp: 0.,
            prev2_time: -400,
            ema: [0.; 3],
            biggest: 0.,
            big_time: -400,
            last_change: 0.,
            change_at: -400,
            orientation: 1.,
            count: 0.,
        }
    }
}
impl History {
    pub fn update(&mut self, p: f64, steps_left: u64) -> ([f32; 73], f64) {
        let k = self.prices.len() as i64;
        let previous = self.prices.last().copied().unwrap_or(p);
        if k == 0 {
            self.event_base = p;
            self.event_peak = p;
            self.ema = [p; 3];
        }
        self.prices.push(p);
        let r = p - previous;
        if r.abs() > 0.001 {
            self.last_change = r;
            self.change_at = k;
        }
        for (j, &threshold) in THR.iter().enumerate() {
            if r.abs() >= threshold {
                self.last_amp[j] = r;
                self.last_time[j] = k;
            }
        }
        if r.abs() > self.biggest.abs() {
            self.biggest = r;
            self.big_time = k;
        }
        if r.abs() >= 0.25 {
            let sign = r.signum();
            if self.event_dir != sign || k - self.event_last > 12 {
                self.prev2_amp = self.prev_amp;
                self.prev2_time = self.prev_time;
                self.prev_amp = self.event_amp;
                self.prev_time = self.event_last;
                self.event_start = k;
                self.event_base = previous;
                self.event_dir = sign;
                self.event_peak = p;
                self.count += 1.;
            }
            self.event_last = k;
            self.orientation = sign;
        }
        if self.event_dir * (p - self.event_peak) > 0. {
            self.event_peak = p;
        }
        self.event_amp = self.event_peak - self.event_base;
        let o = self.orientation;
        let mut x = [0f32; 73];
        let mut f = 0;
        let mut push = |v: f64| {
            x[f] = v as f32;
            f += 1;
        };
        push(k as f64 * 0.5);
        push(steps_left as f64 * 0.5);
        push((p - self.prices[0]) * o);
        for (j, decay) in [0.25, 0.07, 0.02].into_iter().enumerate() {
            self.ema[j] += decay * (p - self.ema[j]);
            push((p - self.ema[j]) * o);
        }
        for lag in LAGS {
            push((p - self.prices[(k as usize).saturating_sub(lag)]) * o);
        }
        for w in WINS {
            let lo = (k as usize + 1).saturating_sub(w);
            let mut low = p * o;
            let mut high = low;
            let mut ilt = k;
            let mut iht = k;
            let mut tv = 0.;
            for i in lo..=k as usize {
                let q = self.prices[i] * o;
                if q <= low {
                    low = q;
                    ilt = i as i64;
                }
                if q >= high {
                    high = q;
                    iht = i as i64;
                }
                if i > lo {
                    tv += (self.prices[i] - self.prices[i - 1]).abs();
                }
            }
            push(p * o - low);
            push(high - p * o);
            push((k - ilt) as f64 * 0.5);
            push((k - iht) as f64 * 0.5);
            push(tv);
        }
        for j in 0..4 {
            push(self.last_amp[j] * o);
            push((k - self.last_time[j]).min(400) as f64 * 0.5);
        }
        push(self.event_amp * o);
        push((k - self.event_last).min(400) as f64 * 0.5);
        push((self.event_last - self.event_start) as f64 * 0.5);
        push((p - self.event_peak) * o);
        push(self.prev_amp * o);
        push((k - self.prev_time).min(400) as f64 * 0.5);
        push(self.prev2_amp * o);
        push((k - self.prev2_time).min(400) as f64 * 0.5);
        push(self.biggest * o);
        push((k - self.big_time) as f64 * 0.5);
        push(self.last_change * o);
        push((k - self.change_at) as f64 * 0.5);
        push(self.count);
        push((p - self.event_base) * o);
        (x, o)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn crashes_and_squeezes_have_the_same_oriented_features() {
        let mut up = History::default();
        let mut down = History::default();
        let path = [
            0.0, 0.02, -0.03, 0.08, 1.3, 2.6, 2.6, 2.5, 2.6, 2.7, 1.1, 1.0,
        ];
        for (k, &p) in path.iter().enumerate() {
            let (a, oa) = up.update(p, (100 - k) as u64);
            let (b, ob) = down.update(-p, (100 - k) as u64);
            if k >= 4 {
                assert_eq!(oa, -ob);
                for j in 0..73 {
                    assert!((a[j] - b[j]).abs() < 1e-5, "feature {j}");
                }
            }
        }
    }
}
