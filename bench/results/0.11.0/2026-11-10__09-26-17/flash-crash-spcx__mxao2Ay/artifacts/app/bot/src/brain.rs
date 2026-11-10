//! The strategy's decisions, as a pure function of the marks it has seen, so
//! the bot and the offline simulator (`src/bin/sim.rs`) share them.

#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// A step move bigger than this (fraction) counts as the market moving.
    pub mv_thr: f64,
    /// Consecutive moving steps before riding a move.
    pub ride_after: u32,
    /// Fraction of the cap to ride with.
    pub ride_frac: f64,
    /// Smallest deviation from the anchor worth riding.
    pub ride_min: f64,
    /// Quiet steps before fading.
    pub q_fade: u32,
    /// Deviation at which fading starts, and at which it's full size.
    pub fade_min: f64,
    pub fade_full: f64,
    /// Exit the fade once the deviation is below this share of its peak.
    pub exit_frac: f64,
    /// Anchor EMA weight per step while quiet.
    pub anchor_alpha: f64,
    /// Worst move (fraction) a position must survive within the drawdown room.
    pub worst_move: f64,
    /// Drawdown kept in reserve.
    pub dd_buffer: f64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            mv_thr: 0.002,
            ride_after: 1,
            ride_frac: 1.0,
            ride_min: 0.0024,
            q_fade: 4,
            fade_min: 0.0048,
            fade_full: 0.0089,
            exit_frac: 0.84,
            anchor_alpha: 0.05,
            worst_move: 0.08,
            dd_buffer: 105.0,
        }
    }
}

pub struct Brain {
    pub p: Params,
    anchor: Option<f64>,
    last: Option<f64>,
    moving_run: u32,
    quiet: u32,
    in_shock: bool,
    peak_dev: f64,
    last_dir: f64,
    target: f64,
}

impl Brain {
    pub fn new(p: Params) -> Self {
        Brain {
            p,
            anchor: None,
            last: None,
            moving_run: 0,
            quiet: 0,
            in_shock: false,
            peak_dev: 0.0,
            last_dir: 0.0,
            target: 0.0,
        }
    }

    /// The position to hold after seeing `mark`, given the cap and the
    /// drawdown room left.
    pub fn decide(&mut self, mark: f64, cap: f64, dd_room: f64) -> f64 {
        let p = self.p;
        let anchor = *self.anchor.get_or_insert(mark);
        let last = self.last.replace(mark).unwrap_or(mark);
        let r = mark / last - 1.0;
        if r.abs() > p.mv_thr {
            if r.signum() == self.last_dir {
                self.moving_run += 1;
            } else {
                self.moving_run = 1;
            }
            self.last_dir = r.signum();
            self.quiet = 0;
        } else {
            self.quiet += 1;
            self.moving_run = 0;
        }
        let dev = mark / anchor - 1.0;
        if !self.in_shock {
            if dev.abs() > p.fade_min.min(p.ride_min) {
                self.in_shock = true;
                self.peak_dev = dev;
            } else if self.quiet > 0 {
                let a = self.anchor.as_mut().unwrap();
                *a += p.anchor_alpha * (mark - *a);
            }
        }
        let mut target = 0.0;
        if self.in_shock {
            if dev * self.peak_dev > 0.0 && dev.abs() > self.peak_dev.abs() {
                self.peak_dev = dev;
            }
            let pk = self.peak_dev;
            if self.moving_run >= p.ride_after && dev.abs() > p.ride_min {
                // Ride the move while it runs.
                target = self.last_dir * cap * p.ride_frac;
            } else if self.quiet >= p.q_fade {
                if dev * pk <= 0.0 || dev.abs() < p.exit_frac * pk.abs() {
                    // Reverted: done with this shock, start over from here.
                    self.in_shock = false;
                    self.anchor = Some(mark);
                    self.peak_dev = 0.0;
                } else if pk.abs() > p.fade_min {
                    let s = ((pk.abs() - p.fade_min) / (p.fade_full - p.fade_min).max(1e-9))
                        .clamp(0.0, 1.0);
                    target = -pk.signum() * cap * s.max(0.3);
                }
            } else {
                // Moving, but not yet a ride, or not yet quiet: keep what we hold.
                target = self.target;
            }
        }
        let risk_cap = ((dd_room - p.dd_buffer).max(0.0) / (p.worst_move * mark)).min(cap);
        target = target.clamp(-risk_cap, risk_cap);
        self.target = target;
        target
    }
}
