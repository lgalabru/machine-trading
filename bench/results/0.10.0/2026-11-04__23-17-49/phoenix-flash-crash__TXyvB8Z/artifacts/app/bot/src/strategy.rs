//! The trading logic, free of any chain access: it sees one price per step
//! and answers the position it wants. The bot (`main.rs`) and the offline
//! simulator (`bin/sim.rs`) both drive it, so what is tuned is what trades.
//!
//! The market model it is built around (the task's scenario family):
//! - Phoenix moves its index at most ~3.95% per step: a bigger instant move
//!   lands in `n = ceil(move / 3.95%)` equal parts on consecutive steps, so
//!   the first part of a large move announces the rest.
//! - Moves land at once (~70%) or ramp in uneven increments a few slots
//!   apart over 1-7 s.
//! - Shocks come in legs (another follows with odds ~1/3, up to three), hold
//!   8-35 s, then revert (65%: gives back 50-100%), stall (20%) or extend
//!   (15%: a further 1-5%).
//! - Fizzles (0.4-2%) hold 1.5-10 s and then give everything back.
//!
//! Levels are fractions of the starting mark (0.01 = 1%); time is in steps
//! (one step = two slots = 0.5 s).

#[derive(Debug, Clone)]
pub struct Params {
    pub max_pos: f64,
    /// The per-step cap Phoenix applies to its price.
    pub cap: f64,

    // Split momentum.
    pub jump_lo: f64,
    pub jump_size: f64,

    // Move tracking: a step change of at least `inc` is an increment; a move
    // is a run of same-direction increments at most `gap` steps apart.
    pub inc: f64,
    pub gap: f64,

    // Ramp momentum: ride a move once it has `ramp_n` increments.
    pub ramp_n: f64,
    pub ramp_size: f64,
    /// Ride a move's first increment, by phase (before the shock, during
    /// it, after the aftermath).
    pub r1_pre: f64,
    pub r1_hold: f64,
    pub r1_post: f64,
    /// First-increment momentum multiplier at or above `r1_big_thr`.
    pub r1_big_thr: f64,
    pub r1_big: f64,
    /// Momentum is held while the move's last increment is this recent.
    pub mom_gap: f64,
    /// Ride a move's first increment if it is in [m1_lo, m1_hi].
    pub m1_lo: f64,
    pub m1_hi: f64,
    pub m1_size: f64,

    // Fizzle fade: a settled move of size in [fz_lo, fz_hi], faded until
    // `fz_steps` after it started, with a weight per phase.
    pub fz_lo: f64,
    pub fz_hi: f64,
    pub fz_steps: f64,
    pub fz_pre: f64,
    pub fz_hold: f64,
    pub fz_post: f64,
    /// Before this session step the shock can't have started: every move is a fizzle.
    pub early: f64,
    pub fz_early: f64,
    pub r1_early: f64,
    /// Fizzle weight multiplier below `fz_mid`.
    pub fz_mid: f64,
    pub fz_small: f64,
    /// A fizzle has given back once this share of it is gone.
    pub fz_back: f64,
    /// Fizzle weight for a move after a fizzle has given back in the stretch.
    pub fz_second: f64,
    /// Fizzle weight for the give-back move of a fizzle.
    pub fz_giveback: f64,
    /// A leg that gives back within this many steps of starting was a fizzle.
    pub fz_undo: f64,

    // Shock fade, sized by its expected value per unit risk:
    // EV = a*S - b (fewer than three legs), a3*S - b3 (three legs).
    pub fade_a: f64,
    pub fade_b: f64,
    pub fade_a2: f64,
    pub fade_b2: f64,
    pub fade_a3: f64,
    pub fade_b3: f64,
    pub fade_k: f64,
    /// Enter the fade only this many steps after the last leg.
    pub fade_age: f64,
    /// A shock needs at least this displacement.
    pub shock_min: f64,
    /// A leg is a move of at least this size.
    pub leg_min: f64,
    /// A move toward the start that gives back this share of the shock is
    /// the revert.
    pub revert_done: f64,
    /// The hold after a leg, in steps (8-35 s).
    pub hold_lo: f64,
    pub hold_hi: f64,
    /// Fade only while the aftermath is at least this likely before the end.
    pub p_min: f64,

    // Risk.
    pub max_dd: f64,
    pub dd_margin: f64,
    /// Past this drawdown (USD) the strategy goes flat for the rest of the session.
    pub halt_dd: f64,
    pub min_trade: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            max_pos: 70.0,
            cap: 0.0395,
            jump_lo: 0.0197,
            jump_size: 1.0,
            inc: 0.002,
            gap: 3.0,
            ramp_n: 2.0,
            ramp_size: 1.0,
            r1_pre: 1.0,
            r1_hold: 1.30778,
            r1_post: 1.0,
            r1_big_thr: 0.011575255102040815,
            r1_big: 0.48207,
            mom_gap: 2.0,
            m1_lo: 1.0,
            m1_hi: 1.0,
            m1_size: 0.0,
            fz_lo: 0.0029,
            fz_hi: 0.02181,
            fz_steps: 26.0,
            fz_pre: 1.0,
            fz_hold: 1.0,
            fz_post: 1.0,
            early: 0.0,
            fz_early: 1.0,
            r1_early: 1.0,
            fz_mid: 0.01075,
            fz_small: 1.0,
            fz_back: 0.8,
            fz_second: 1.0,
            fz_giveback: 1.0,
            fz_undo: 26.16267,
            fade_a: 0.0549,
            fade_b: 0.00711,
            fade_a2: 0.0524,
            fade_b2: 0.0064,
            fade_a3: 0.5,
            fade_b3: 0.0036,
            fade_age: 0.0,
            fade_k: 300.0,
            leg_min: 0.0,
            shock_min: 0.00373,
            revert_done: 0.2565,
            hold_lo: 16.0,
            hold_hi: 70.0,
            p_min: 0.35508,
            max_dd: 1000.0,
            halt_dd: 750.0,
            dd_margin: 104.59572,
            min_trade: 3.49496,
        }
    }
}

macro_rules! params_set {
    ($self:ident, $name:ident, $v:ident, [$($f:ident),*]) => {
        match $name { $(stringify!($f) => { $self.$f = $v; true })* _ => false }
    };
}

impl Params {
    /// Set a parameter by name (the simulator's overrides).
    #[allow(dead_code)]
    pub fn set(&mut self, name: &str, v: f64) -> bool {
        params_set!(self, name, v, [max_pos, cap, jump_lo, jump_size, inc, gap, ramp_n, ramp_size,
            m1_lo, m1_hi, m1_size, fz_lo, fz_hi, fz_steps, fz_pre, fz_hold, fz_post, fz_undo, fade_a, fade_b, fade_a2, fade_b2, fade_age, r1_pre, r1_hold, r1_post, fz_mid, fz_small, fz_back, fz_second, fz_giveback, early, fz_early, r1_early, r1_big_thr, r1_big, mom_gap,
            fade_a3, fade_b3, fade_k, shock_min, leg_min, revert_done, hold_lo, hold_hi, p_min, max_dd,
            dd_margin, halt_dd, min_trade])
    }
}

/// What the strategy sees each step.
#[derive(Debug, Clone, Copy)]
pub struct Obs {
    /// The session step (0 at the first step).
    pub step: u64,
    /// Phoenix's index price (the oracle the market makers quote around).
    pub price: f64,
    /// Steps left in the session after this one.
    pub steps_left: u64,
    /// Current position, in shares (positive long).
    pub pos: f64,
    /// Account value change since the start, in USD, at the mark.
    pub pnl: f64,
}

/// A run of same-direction increments.
#[derive(Debug, Clone, Copy)]
struct Move {
    dir: f64,
    from: f64,
    to: f64,
    start: usize,
    last: usize,
    n: u32,
}

#[derive(Debug, Clone)]
pub struct Strategy {
    pub p: Params,
    anchor: f64,
    levels: Vec<f64>,
    /// Session step of `levels[0]`.
    step0: u64,
    // Split momentum: the part being ridden and parts seen.
    jump: f64,
    jump_parts: u32,
    plateau: f64,
    cur: Option<Move>,
    /// The last settled move, if it may still be a fizzle.
    fizzle: Option<Move>,
    /// Direction of a fizzle that has given back in this quiet stretch (a
    /// stretch carries at most one).
    fizzled: Option<f64>,
    /// Weight of the current fizzle candidate (lower for a second one).
    fz_weight: f64,
    // Shock state.
    shock_dir: f64,
    shock: f64,
    legs: u32,
    /// Step the last leg settled at.
    leg_end: usize,
    reverted: bool,
    /// Shock state before the last leg, and that leg: a leg that gives
    /// everything back within a fizzle's lifetime was a fizzle.
    before_leg: Option<(f64, f64, u32, usize, Move)>,
    peak_pnl: f64,
    last_pnl: f64,
    halted: bool,
    pub last_reason: &'static str,
    /// Settled moves (step, phase, size, direction vs shock, increments, duration), when debugging.
    pub debug: bool,
    pub events: Vec<(usize, u8, f64, f64, u32, usize)>,
}

impl Strategy {
    pub fn new(p: Params, anchor: f64) -> Self {
        Self {
            p,
            anchor,
            levels: Vec::new(),
            step0: 0,
            jump: 0.0,
            jump_parts: 0,
            plateau: 0.0,
            cur: None,
            fizzle: None,
            fizzled: None,
            fz_weight: 1.0,
            shock_dir: 0.0,
            shock: 0.0,
            legs: 0,
            leg_end: 0,
            reverted: false,
            before_leg: None,
            peak_pnl: 0.0,
            last_pnl: 0.0,
            halted: false,
            last_reason: "",
            debug: false,
            events: Vec::new(),
        }
    }

    /// (shock direction, displacement in its direction, legs, steps since the last leg, reverted)
    #[allow(dead_code)]
    pub fn shock_state(&self) -> (f64, f64, u32, usize, bool) {
        let k = self.levels.len().saturating_sub(1);
        let d = self.levels.last().copied().unwrap_or(0.0);
        (self.shock_dir, d * self.shock_dir, self.legs, k.saturating_sub(self.leg_end), self.reverted)
    }

    /// How far the account's value is below its peak, in USD.
    pub fn drawdown(&self) -> f64 {
        self.peak_pnl - self.last_pnl
    }

    /// Split momentum: Some(direction) while another equal part may follow.
    fn split(&mut self, m: f64) -> Option<f64> {
        let p = &self.p;
        if self.jump != 0.0 {
            let same = m.signum() == self.jump.signum()
                && (m.abs() - self.jump.abs()).abs() <= 0.08 * self.jump.abs() + 0.0008;
            if same {
                self.jump_parts += 1;
                // A move of n parts has parts larger than cap*(n-1)/n.
                let n = self.jump_parts as f64 + 1.0;
                if self.jump.abs() > p.cap * (n - 1.0) / n {
                    return Some(self.jump.signum());
                }
            }
            self.jump = 0.0;
            self.jump_parts = 0;
        }
        if m.abs() >= p.jump_lo && m.abs() <= p.cap + 0.002 {
            self.jump = m;
            self.jump_parts = 1;
            return Some(m.signum());
        }
        None
    }

    /// 0 before the shock (a leg still young enough to be a fizzle counts as
    /// before), 1 during it, 2 after the aftermath.
    fn phase(&self, k: usize, move_start: usize) -> u8 {
        if ((move_start as u64 + self.step0) as f64) < self.p.early {
            3
        } else if self.reverted {
            2
        } else if self.legs == 0
            || (self.legs == 1 && self.before_leg.is_some_and(|b| (k - b.4.start) as f64 <= self.p.fz_undo))
        {
            0
        } else {
            1
        }
    }

    /// A move settled: update the shock bookkeeping.
    fn settle(&mut self, mv: Move, k: usize) {
        let size = (mv.to - mv.from).abs();
        let p = &self.p;
        let away = self.shock_dir == 0.0 || mv.dir == self.shock_dir;
        if let Some((dir, shock, legs, leg_end, leg)) = self.before_leg {
            let leg_size = (leg.to - leg.from).abs();
            if mv.dir == -leg.dir && (k - leg.start) as f64 <= p.fz_undo && size >= 0.7 * leg_size {
                // The "leg" gave everything back: it was a fizzle.
                self.shock_dir = dir;
                self.shock = shock;
                self.legs = legs;
                self.leg_end = leg_end;
                self.before_leg = None;
                self.fizzle = None;
                self.fizzled = Some(leg.dir);
                return;
            }
        }
        let mut weight = 1.0;
        if let Some(fd) = self.fizzled {
            if mv.dir == -fd {
                // The fizzle's own give-back.
                weight = p.fz_giveback;
            } else {
                weight = p.fz_second;
                self.fizzled = None;
            }
        }
        if self.reverted {
            // After the aftermath only fizzles remain.
        } else if ((mv.start as u64 + self.step0) as f64) < p.early {
            // Too early for the shock: a fizzle.
        } else if away && mv.to.abs() >= p.shock_min && size >= p.leg_min && mv.to.signum() == mv.dir {
            self.before_leg = Some((self.shock_dir, self.shock, self.legs, self.leg_end, mv));
            if self.shock_dir == 0.0 {
                self.shock_dir = mv.dir;
            }
            self.legs += 1;
            self.shock = mv.to.abs();
            self.leg_end = k;
        } else if !away && self.shock > 0.0 && size >= p.revert_done * self.shock {
            self.reverted = true;
        }
        if self.debug {
            let phase = if self.shock_dir == 0.0 { 0 } else if self.reverted { 2 } else { 1 };
            let rel = if self.shock_dir == 0.0 { 0.0 } else { mv.dir * self.shock_dir };
            self.events.push((k, phase, mv.to - mv.from, rel, mv.n, mv.last - mv.start));
        }
        self.fizzle = if size >= self.p.fz_lo && size <= self.p.fz_hi { Some(mv) } else { None };
        self.fz_weight = weight;
    }

    /// The position to hold after this step.
    pub fn step(&mut self, o: Obs) -> f64 {
        let k = self.levels.len();
        if k == 0 {
            self.step0 = o.step;
        }
        let d = o.price / self.anchor - 1.0;
        let m = if k > 0 { (1.0 + d) / (1.0 + self.levels[k - 1]) - 1.0 } else { 0.0 };
        self.levels.push(d);
        let p = self.p.clone();

        let split = self.split(m);

        // --- Move tracking ---------------------------------------------------
        if m.abs() >= p.inc {
            match self.cur.as_mut() {
                Some(c) if c.dir == m.signum() => {
                    c.to = d;
                    c.last = k;
                    c.n += 1;
                }
                _ => {
                    if let Some(c) = self.cur.take() {
                        self.settle(c, k);
                    }
                    let from = if k > 0 { self.levels[k - 1] } else { d };
                    self.cur = Some(Move { dir: m.signum(), from, to: d, start: k, last: k, n: 1 });
                }
            }
        } else if let Some(c) = self.cur {
            if (k - c.last) as f64 > p.gap {
                self.cur = None;
                let mut c = c;
                c.to = d;
                self.settle(c, k);
                self.plateau = d;
            }
        } else {
            self.plateau += 0.2 * (d - self.plateau);
        }
        // A fizzle candidate is done once it has given back or aged out.
        if let Some(f) = self.fizzle {
            let back = (f.to - d) * f.dir;
            if back >= p.fz_back * (f.to - f.from).abs() {
                self.fizzle = None;
                self.fizzled = Some(f.dir);
            } else if (k - f.start) as f64 > p.fz_steps {
                self.fizzle = None;
            }
        }

        // --- Compose ---------------------------------------------------------
        let mut target = 0.0;
        self.last_reason = "flat";
        let moving = self.cur.filter(|c| (k - c.last) as f64 <= p.mom_gap);
        let start = moving.map(|c| c.start).or(self.fizzle.map(|f| f.start)).unwrap_or(k);
        let phase = self.phase(k, start);
        if let Some(dir) = split {
            target = dir * p.jump_size * p.max_pos;
            self.last_reason = "split";
        } else if let Some(c) = moving.filter(|c| c.n >= 2 && c.last + 1 >= k) {
            target = c.dir * p.ramp_size * p.max_pos;
            self.last_reason = "ramp";
        } else if let Some(c) = moving.filter(|c| c.n == 1 && c.last + 1 >= k) {
            let w = [p.r1_pre, p.r1_hold, p.r1_post, p.r1_early][phase as usize];
            let big = if (c.to - c.from).abs() >= p.r1_big_thr { p.r1_big } else { 1.0 };
            target = c.dir * w * big * p.max_pos;
            self.last_reason = "ramp";
        } else if let Some(c) = moving.filter(|c| {
            c.n == 1 && c.last == k && (c.to - c.from).abs() >= p.m1_lo && (c.to - c.from).abs() <= p.m1_hi
        }) {
            target = c.dir * p.m1_size * p.max_pos;
            self.last_reason = "first";
        } else if moving.is_some() && o.pos != 0.0 {
            // Mid-move: keep riding what we hold if it agrees with the move.
            let c = moving.unwrap();
            if o.pos.signum() == c.dir {
                target = o.pos;
                self.last_reason = "hold";
            }
        }
        if self.last_reason == "flat" {
            // Shock fade.
            if self.shock_dir != 0.0 && !self.reverted {
                let s = (d * self.shock_dir).max(0.0);
                let ev = match self.legs { 1 => p.fade_a * s - p.fade_b, 2 => p.fade_a2 * s - p.fade_b2, _ => p.fade_a3 * s - p.fade_b3 };
                // Odds the aftermath lands before the end, given the hold so far.
                let age = (k - self.leg_end) as f64;
                let lo = age.max(p.hold_lo);
                let pa = if age > p.hold_hi { 0.0 } else { ((age + o.steps_left as f64).min(p.hold_hi) - lo).max(0.0) / (p.hold_hi - lo).max(1.0) };
                if ev > 0.0 && pa >= p.p_min && age >= p.fade_age {
                    target = -self.shock_dir * (ev * p.fade_k).min(1.0) * p.max_pos;
                    self.last_reason = "fade";
                }
            }
            // Fizzle fade.
            if let Some(f) = self.fizzle {
                let w = self.fz_weight * [p.fz_pre, p.fz_hold, p.fz_post, p.fz_early][phase as usize]
                    * if (f.to - f.from).abs() < p.fz_mid { p.fz_small } else { 1.0 };
                let t = -f.dir * w * p.max_pos;
                if w > 0.0 && (target == 0.0 || t.signum() == target.signum()) {
                    target = if t.abs() > target.abs() { t } else { target };
                    self.last_reason = "fizzle";
                }
            }
        }

        // --- Risk ------------------------------------------------------------
        self.peak_pnl = self.peak_pnl.max(o.pnl);
        self.last_pnl = o.pnl;
        let room = (p.max_dd - p.dd_margin - self.drawdown()).max(0.0);
        let mut lim = (room / (p.cap * o.price)).min(p.max_pos);
        if self.drawdown() > p.halt_dd {
            self.halted = true;
        }
        if self.halted {
            lim = 0.0;
            self.last_reason = "halt";
        }
        target = target.clamp(-lim, lim);
        if (target - o.pos).abs() < p.min_trade && target != 0.0 {
            target = o.pos;
        }
        target
    }
}
