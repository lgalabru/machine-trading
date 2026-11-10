//! The shock strategy, as a pure function of the level path: ride a move
//! while it runs, fade small moves (fizzles) briefly, fade a standing shock
//! until its aftermath. Levels are percent from the session's first price.

#[derive(Debug, Clone)]
pub struct Params {
    /// A step change at least this big (percent) is part of a move.
    pub delta: f64,
    /// Parts at most this many steps apart belong to one move.
    pub gap: i64,
    /// Ride a move for this many steps after its last part.
    pub h_mom: i64,
    /// Ride only moves at least this big so far.
    pub pmin_mom: f64,
    pub mom_w: f64,
    /// Fade a finished move of this size range as a fizzle, this long.
    pub fiz_min: f64,
    pub fiz_max: f64,
    pub t_fiz: i64,
    pub fiz_r: f64,
    pub fiz_w: f64,
    /// Fade fizzles that go against a standing shock too (1) or not (0).
    pub fiz_counter: f64,
    /// Fade a standing shock of at least this size, by legs seen (1, 2, 3+).
    pub s_min1: f64,
    pub s_min2: f64,
    pub s_min3: f64,
    pub shock_w: f64,
    /// A level this far from the start is a shock, not a fizzle.
    pub s_arm: f64,
    /// A move this big counts as a leg.
    pub leg_min: f64,
    /// The shock is over once it gave back this share of its peak.
    pub rho: f64,
    /// ... or this many steps passed since its last leg.
    pub t_shock: i64,
    /// ... or this many legs ran.
    pub max_legs: f64,
    /// The drawdown governor: stop for good past this share of the limit,
    /// and before it keep `dd_buf` plus a worst step of `dd_worst` in hand.
    pub halt: f64,
    pub dd_buf: f64,
    pub dd_worst: f64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            delta: 0.2,
            gap: 3,
            h_mom: 3,
            pmin_mom: 0.3,
            mom_w: 1.0,
            fiz_min: 0.6,
            fiz_max: 2.1,
            t_fiz: 24,
            fiz_r: 1.0,
            fiz_w: 1.0,
            fiz_counter: 1.0,
            s_min1: 3.5,
            s_min2: 2.0,
            s_min3: 1.0,
            shock_w: 1.0,
            s_arm: 2.2,
            leg_min: 0.8,
            rho: 0.35,
            t_shock: 90,
            max_legs: 5.0,
            halt: 0.7,
            dd_buf: 1.5,
            dd_worst: 6.0,
        }
    }
}

impl Params {
    pub fn set(&mut self, name: &str, v: f64) -> bool {
        match name {
            "delta" => self.delta = v,
            "gap" => self.gap = v as i64,
            "h_mom" => self.h_mom = v as i64,
            "pmin_mom" => self.pmin_mom = v,
            "mom_w" => self.mom_w = v,
            "fiz_min" => self.fiz_min = v,
            "fiz_max" => self.fiz_max = v,
            "t_fiz" => self.t_fiz = v as i64,
            "fiz_r" => self.fiz_r = v,
            "fiz_w" => self.fiz_w = v,
            "fiz_counter" => self.fiz_counter = v,
            "s_min1" => self.s_min1 = v,
            "s_min2" => self.s_min2 = v,
            "s_min3" => self.s_min3 = v,
            "shock_w" => self.shock_w = v,
            "s_arm" => self.s_arm = v,
            "leg_min" => self.leg_min = v,
            "rho" => self.rho = v,
            "t_shock" => self.t_shock = v as i64,
            "max_legs" => self.max_legs = v,
            "halt" => self.halt = v,
            "dd_buf" => self.dd_buf = v,
            "dd_worst" => self.dd_worst = v,
            _ => return false,
        }
        true
    }
}

#[derive(Debug, Clone)]
pub struct Strat {
    pub p: Params,
    k: i64,
    prev: Option<f64>,
    // The move in progress (or the last one).
    mv_dir: f64,
    mv_anchor: f64,
    mv_last: i64,
    mv_open: bool,
    // The fizzle fade.
    fz_on: bool,
    fz_dir: f64,
    fz_size: f64,
    fz_end: f64,
    fz_until: i64,
    // The shock.
    peak: f64,
    legs: u32,
    last_leg: i64,
    done: bool,
    halted: bool,
}

impl Strat {
    pub fn new(p: Params) -> Self {
        Strat {
            p,
            k: -1,
            prev: None,
            mv_dir: 0.0,
            mv_anchor: 0.0,
            mv_last: -1000,
            mv_open: false,
            fz_on: false,
            fz_dir: 0.0,
            fz_size: 0.0,
            fz_end: 0.0,
            fz_until: 0,
            peak: 0.0,
            legs: 0,
            last_leg: 0,
            done: false,
            halted: false,
        }
    }

    /// The share of the cap the account may hold, given its drawdown so far
    /// (`dd`) and the limit, both in percent of the cap's notional: a
    /// worst single step against a full position must still fit.
    pub fn allowed(&mut self, dd: f64, limit: f64) -> f64 {
        if dd >= self.p.halt * limit {
            self.halted = true;
        }
        if self.halted {
            return 0.0;
        }
        // In quarters, so that a wobbling drawdown doesn't churn the position.
        let share = ((limit - dd - self.p.dd_buf) / self.p.dd_worst).clamp(0.0, 1.0);
        (share * 4.0).floor() / 4.0
    }

    fn shock_min(&self) -> f64 {
        match self.legs {
            0 | 1 => self.p.s_min1,
            2 => self.p.s_min2,
            _ => self.p.s_min3,
        }
    }

    /// One step: the level now (percent from the start). Returns the target
    /// position as a share of the cap, in [-1, 1].
    pub fn step(&mut self, x: f64) -> f64 {
        self.k += 1;
        let k = self.k;
        let p = self.p.clone();
        let prev = self.prev.unwrap_or(x);
        self.prev = Some(x);
        let d = x - prev;
        if d.abs() >= p.delta {
            let dir = d.signum();
            if self.mv_open && self.mv_dir == dir && k - self.mv_last <= p.gap {
                self.mv_last = k;
            } else {
                if self.mv_open {
                    self.finish_move(prev);
                }
                self.mv_open = true;
                self.mv_dir = dir;
                self.mv_anchor = prev;
                self.mv_last = k;
            }
        }
        if self.mv_open && k - self.mv_last >= p.h_mom {
            self.finish_move(x);
        }
        // The shock's state.
        if x.abs() > self.peak.abs() {
            self.peak = x;
        }
        if !self.done && self.peak.abs() >= p.s_arm {
            let back = (self.peak - x) * self.peak.signum();
            if !self.mv_open && back >= p.rho * self.peak.abs() {
                self.done = true;
            }
            if self.legs as f64 >= p.max_legs {
                self.done = true;
            }
            if self.legs > 0 && k - self.last_leg > p.t_shock {
                self.done = true;
            }
        }
        // The fizzle fade's exits.
        if self.fz_on {
            let back = (self.fz_end - x) * self.fz_dir;
            if k >= self.fz_until || back >= p.fiz_r * self.fz_size {
                self.fz_on = false;
            }
        }
        if self.mv_open {
            let size = (x - self.mv_anchor).abs();
            if size >= p.pmin_mom {
                return self.mv_dir * p.mom_w;
            }
        }
        if self.fz_on {
            return -self.fz_dir * p.fiz_w;
        }
        if !self.done && x.abs() >= self.shock_min() && x.signum() == self.peak.signum() {
            return -x.signum() * p.shock_w;
        }
        0.0
    }

    fn finish_move(&mut self, x: f64) {
        self.mv_open = false;
        let m = x - self.mv_anchor;
        let size = m.abs();
        let dir = self.mv_dir;
        // A retrace of a fizzle being faded: the fade is done.
        if self.fz_on && dir == -self.fz_dir {
            self.fz_on = false;
            return;
        }
        let shock_dir = if self.peak.abs() >= self.p.s_arm {
            self.peak.signum()
        } else {
            0.0
        };
        let against_shock = !self.done && shock_dir != 0.0 && dir == -shock_dir;
        if shock_dir == dir && x.abs() >= self.p.s_arm && size >= self.p.leg_min && !self.done {
            self.legs += 1;
            self.last_leg = self.k;
        }
        if size >= self.p.fiz_min && size <= self.p.fiz_max {
            if against_shock && self.p.fiz_counter < 0.5 {
                return;
            }
            self.fz_on = true;
            self.fz_dir = dir;
            self.fz_size = size;
            self.fz_end = x;
            self.fz_until = self.k + self.p.t_fiz;
        }
    }
}
