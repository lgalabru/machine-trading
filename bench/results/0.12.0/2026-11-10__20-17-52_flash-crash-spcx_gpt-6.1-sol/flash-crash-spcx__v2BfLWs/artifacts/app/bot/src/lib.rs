//! Event detection and causal observations for the price forecasters.
#[derive(Clone, Copy, Debug)]
pub struct Params {
    pub trigger: f64,
    pub noise: f64,
    pub settle: usize,
    pub finish: f64,
    pub expire: usize,
    pub fade: f64,
    pub momentum: f64,
    pub jump: f64,
    pub window: usize,
    pub min_fade: f64,
    pub mode: usize,
}
impl Default for Params {
    fn default() -> Self {
        Self {
            trigger: 0.005,
            noise: 0.002,
            settle: 3,
            finish: 0.4,
            expire: 100,
            fade: 1.0,
            momentum: 1.0,
            jump: 0.03,
            window: 12,
            min_fade: 0.005,
            mode: 0,
        }
    }
}
#[derive(Default)]
pub struct Signal {
    pub history: Vec<f64>,
    pub active: bool,
    pub dir: f64,
    pub origin: f64,
    pub extreme: f64,
    pub progress: f64,
    pub last_move: usize,
    pub born: usize,
    pub moves: usize,
    pub last_jump: usize,
    pub jump_size: f64,
    pub completed: bool,
}
impl Signal {
    pub fn step(&mut self, p: f64, cfg: Params) -> f64 {
        let k = self.history.len();
        let prev = self.history.last().copied().unwrap_or(p);
        let delta = p / prev - 1.0;
        self.history.push(p);
        if !self.active {
            let from = k.saturating_sub(cfg.window);
            let mut low = p;
            let mut high = p;
            for &v in &self.history[from..=k] {
                low = low.min(v);
                high = high.max(v);
            }
            let up = p / low - 1.0;
            let down = high / p - 1.0;
            if up.max(down) >= cfg.trigger {
                self.active = true;
                self.dir = if up > down { 1.0 } else { -1.0 };
                self.origin = if up > down { low } else { high };
                self.extreme = p;
                self.progress = p;
                self.last_move = k;
                self.born = k;
                self.moves = 1;
                self.jump_size = delta.abs();
                self.last_jump = k;
                self.completed = false;
            }
        } else {
            if self.dir * (p - self.extreme) > 0.0 {
                self.extreme = p;
            }
            if self.dir * (p - self.progress) / self.progress > cfg.noise {
                self.last_move = k;
                self.progress = p;
                self.moves += 1;
            }
            if delta * self.dir > cfg.noise {
                self.last_jump = k;
                self.jump_size = delta.abs();
            }
        }
        if !self.active {
            return 0.0;
        }
        let amplitude = (self.extreme - self.origin).abs() / self.origin;
        let retrace = self.dir * (self.extreme - p) / self.origin;
        let age = k - self.last_move;
        // A rebound remains part of the original event, until its price settles.
        if retrace > amplitude * cfg.finish {
            self.completed = true;
        }
        let from = k.saturating_sub(cfg.settle);
        let recent = (p / self.history[from] - 1.0).abs();
        if (self.completed && recent < cfg.noise && age > cfg.settle) || k - self.born > cfg.expire
        {
            self.active = false;
            // Forget the completed event so it cannot immediately trigger itself again.
            let n = self.history.len();
            let start = n.saturating_sub(cfg.window + 1);
            for v in &mut self.history[start..] {
                *v = p;
            }
            return 0.0;
        }
        if cfg.mode == 1 {
            return -self.dir * cfg.fade;
        }
        let running = age < cfg.settle && !self.completed;
        let momentum_ok = self.moves >= 2 || self.jump_size > cfg.jump;
        if running && momentum_ok {
            return self.dir * cfg.momentum;
        }
        if running && cfg.mode == 0 {
            return 0.0;
        }
        if amplitude < cfg.min_fade {
            return 0.0;
        }
        -self.dir * cfg.fade
    }
}

/// Causal observations used for forecasting; all price features are percentages.
#[derive(Default)]
pub struct Features {
    prices: Vec<f64>,
    pub direction: f64,
    jump_k: usize,
    jump: f64,
    previous_jump: f64,
    previous_gap: f64,
    event: Signal,
    last_change: usize,
    last_small: usize,
    first_large: Option<usize>,
    last_large: usize,
    large_return: f64,
    large_count: usize,
    leg_count: usize,
    largest_jump: f64,
    largest_k: usize,
    variation: f64,
}
impl Features {
    pub fn step(&mut self, p: f64, left: usize) -> [f32; 64] {
        let k = self.prices.len();
        let prev = self.prices.last().copied().unwrap_or(p);
        let change = p / prev - 1.0;
        if self.direction == 0.0 {
            self.direction = 1.0;
        }
        if p != prev {
            self.last_change = k;
        }
        if change.abs() > 0.0005 {
            self.last_small = k;
        }
        if change.abs() > 0.005 {
            self.first_large.get_or_insert(k);
            self.last_large = k;
            self.large_return = change;
            self.large_count += 1;
        }
        if change.abs() > self.largest_jump {
            self.largest_jump = change.abs();
            self.largest_k = k;
        }
        if self.first_large.is_some() {
            self.variation += change.abs();
        }
        if change.abs() > 0.002 {
            if k - self.jump_k >= 6 {
                self.leg_count += 1;
            }
            self.previous_jump = self.jump;
            self.previous_gap = (k - self.jump_k) as f64;
            self.jump = change;
            self.jump_k = k;
            self.direction = change.signum();
        }
        self.prices.push(p);
        self.event.step(p, Params::default());
        let d = self.direction;
        let mut out = [0.0; 64];
        let mut n = 0;
        let mut push = |v: f64| {
            out[n] = v as f32;
            n += 1;
        };
        push(k as f64 * 0.5);
        push(left as f64 * 0.5);
        push(100.0 * d * (p / self.prices[0] - 1.0));
        push((k - self.jump_k) as f64 * 0.5);
        push(100.0 * d * self.jump);
        push(100.0 * d * self.previous_jump);
        push(self.previous_gap * 0.5);
        push(if self.event.active { 1.0 } else { 0.0 });
        push(self.event.dir * d);
        let origin = if self.event.origin > 0.0 {
            self.event.origin
        } else {
            p
        };
        push(100.0 * d * (self.event.extreme / origin - 1.0));
        push(100.0 * d * (p / origin - 1.0));
        push(k.saturating_sub(self.event.last_move) as f64 * 0.5);
        push(k.saturating_sub(self.event.born) as f64 * 0.5);
        push(if self.event.completed { 1.0 } else { 0.0 });
        for h in [1, 2, 3, 4, 6, 8, 12, 16, 24, 32, 48, 64, 96, 128, 192, 256] {
            push(100.0 * d * (p / self.prices[k.saturating_sub(h)] - 1.0));
        }
        for h in [4, 8, 16, 32, 64, 96, 128, 256] {
            let slice = &self.prices[k.saturating_sub(h)..=k];
            let low = slice.iter().copied().fold(p, f64::min);
            let high = slice.iter().copied().fold(p, f64::max);
            let (favourable, adverse) = if d > 0.0 { (high, low) } else { (low, high) };
            push(100.0 * d * (p / favourable - 1.0));
            push(100.0 * d * (p / adverse - 1.0));
        }
        for h in [4, 8, 16, 32, 64, 96, 128, 256] {
            let slice = &self.prices[k.saturating_sub(h)..=k];
            let variation = slice
                .windows(2)
                .map(|w| (w[1] / w[0] - 1.0).abs())
                .sum::<f64>();
            push(100.0 * variation);
        }
        push((k - self.last_change) as f64 * 0.5);
        push((k - self.last_small) as f64 * 0.5);
        push(self.first_large.map_or(-1.0, |j| (k - j) as f64 * 0.5));
        push((k - self.last_large) as f64 * 0.5);
        push(self.large_return * d * 100.0);
        push(self.large_count as f64);
        push(self.leg_count as f64);
        push(self.largest_jump * 100.0);
        push((k - self.largest_k) as f64 * 0.5);
        push(self.variation * 100.0);
        out
    }
}

pub mod forecast;

pub mod risk;
