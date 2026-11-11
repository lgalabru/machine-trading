//! Causal shock/recovery state machine. Prices are observed, never forecast from a path.
#[derive(Clone, Copy, Debug)]
pub struct Params {
    pub trigger: f64,
    pub drift: f64,
    pub refresh: f64,
    pub momentum_steps: u64,
    pub momentum_size: f64,
    pub fade_size: f64,
    pub recovery: f64,
    pub expiry: u64,
    pub cooldown: u64,
}
impl Default for Params {
    fn default() -> Self {
        Self {
            trigger: 0.006,
            drift: 0.015,
            refresh: 0.0025,
            momentum_steps: 3,
            momentum_size: 1.0,
            fade_size: 1.0,
            recovery: 1.05,
            expiry: 60,
            cooldown: 0,
        }
    }
}
#[derive(Default, Debug)]
pub struct Policy {
    pub prev: f64,
    pub anchor: f64,
    pub origin: f64,
    pub extreme: f64,
    pub direction: f64,
    pub moved: u64,
    pub active: bool,
    pub cool_until: u64,
}
impl Policy {
    pub fn target(&mut self, k: u64, p: f64, par: &Params) -> f64 {
        if self.prev == 0.0 {
            self.prev = p;
            self.anchor = p;
        }
        let change = p / self.prev - 1.0;
        if !self.active && k >= self.cool_until {
            if change.abs() > par.trigger || (p / self.anchor - 1.0).abs() > par.drift {
                self.origin = if change.abs() > par.trigger {
                    self.prev
                } else {
                    self.anchor
                };
                self.direction = (p - self.origin).signum();
                self.extreme = p;
                self.moved = k;
                self.active = true;
            }
        }
        let mut target = 0.0;
        if self.active {
            if (p - self.extreme) * self.direction > p * par.refresh {
                self.extreme = p;
                self.moved = k;
            }
            let amplitude = (self.extreme - self.origin).abs();
            let retraced = (self.extreme - p) * self.direction;
            let age = k - self.moved;
            if retraced >= amplitude * par.recovery || age >= par.expiry {
                self.active = false;
                self.cool_until = k + par.cooldown;
                self.anchor = p;
            } else if age < par.momentum_steps {
                target = self.direction * par.momentum_size;
            } else {
                target = -self.direction * par.fade_size;
            }
        } else {
            self.anchor += 0.1 * (p - self.anchor);
        }
        self.prev = p;
        target
    }
}
