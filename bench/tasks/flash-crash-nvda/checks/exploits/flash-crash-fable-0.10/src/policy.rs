//! The strategy: a level-stack state machine over the oracle price.
//!
//! The scenario family has structure the bot can lean on: a shock in legs
//! (held 8-35 s each), then an aftermath (revert, stall or extend), with
//! wobble and short-lived "fizzles" (0.4-2% moves that fully revert within
//! ~10 s) in the quiet stretches. The bot
//!
//! - rides a move while it is still running (ramps come in sub-steps),
//! - fades small completed moves as fizzle candidates until they either
//!   return to the previous level or outlive a fizzle,
//! - in a pause after a leg, holds momentum for small shocks and fades
//!   large ones,
//! - and caps every position by the worst instant move the family can
//!   throw at it, so the drawdown limit can't be hit.
//!
//! Levels are in percent of the starting price; time in steps of 0.5 s.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Pre,
    Pause,
    Post,
}

#[derive(Debug, Clone)]
pub struct Level {
    pub level: f64,
    pub phase: Phase,
    /// Signed shock size so far (% of start).
    pub s: f64,
    pub legs: u32,
    /// Step the last leg landed (its move start).
    pub k_leg: Option<i64>,
    /// Still a fizzle candidate (may return to the level below).
    pub fizz: bool,
    /// Step the move that produced this level started.
    pub k_move: i64,
}

#[derive(Debug, Clone)]
pub struct Params {
    pub move_thr: f64,
    pub quiet_steps: i64,
    pub sub_thr: f64,
    pub ride_min: f64,
    pub ride_max: f64,
    pub ride_hold: i64,
    pub fizzle_max: f64,
    pub fizzle_timeout: f64,
    pub return_tol: f64,
    pub pause_min: f64,
    pub s_mom: f64,
    pub s_fade0: f64,
    pub s_fade1: f64,
    pub worst_leg: f64,
    pub worst_ext: f64,
    pub worst_fizz: f64,
    /// Worst adverse move once the aftermath has started: it can still
    /// extend, by 1-5% of the starting mark, in a single step.
    pub worst_post: f64,
    /// Floor under every worst-case estimate: no phase is ever treated as
    /// safer than this, whatever the state machine believes.
    pub worst_floor: f64,
    /// Drawdown at which the bot stops taking risk for the rest of the
    /// session, well short of the mandate's limit.
    pub halt_dd: f64,
    pub safety: f64,
    pub size_mom: f64,
    pub size_fade: f64,
    pub size_fizz: f64,
    pub size_ride: f64,
    pub end_guard_s: f64,
    pub cap: f64,
    pub dd_limit: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            move_thr: 0.35,
            quiet_steps: 4,
            sub_thr: 0.25,
            ride_min: 0.25,
            ride_max: 3.0,
            ride_hold: 3,
            fizzle_max: 2.0,
            fizzle_timeout: 14.0,
            return_tol: 0.15,
            pause_min: 8.0,
            s_mom: 1.5,
            s_fade0: 7.0,
            s_fade1: 10.0,
            worst_leg: 8.0,
            worst_ext: 5.0,
            worst_fizz: 2.3,
            worst_post: 5.5,
            worst_floor: 5.0,
            halt_dd: 700.0,
            safety: 150.0,
            size_mom: 70.0,
            size_fade: 70.0,
            size_fizz: 70.0,
            size_ride: 70.0,
            end_guard_s: 1.0,
            cap: 70.0,
            dd_limit: 1000.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Flat,
    Halt,
    Ride,
    Fizz,
    PauseMom,
    PauseFade,
}

pub struct Policy {
    pub p: Params,
    n: i64,
    start: Option<f64>,
    stack: Vec<Level>,
    last_change_k: i64,
    ride_dir: i32,
    move_start_k: Option<i64>,
    prev_lv: Option<f64>,
}

impl Policy {
    pub fn new(p: Params, n_steps: i64) -> Self {
        Self {
            p,
            n: n_steps,
            start: None,
            stack: vec![Level {
                level: 0.0,
                phase: Phase::Pre,
                s: 0.0,
                legs: 0,
                k_leg: None,
                fizz: false,
                k_move: 0,
            }],
            last_change_k: -99,
            ride_dir: 0,
            move_start_k: None,
            prev_lv: None,
        }
    }

    fn secs(steps: i64) -> f64 {
        steps as f64 * 0.5
    }

    pub fn cur(&self) -> &Level {
        self.stack.last().expect("stack never empty")
    }

    pub fn describe(&self) -> String {
        let c = self.cur();
        format!(
            "{:?} lvl={:.2} S={:.2} legs={} fizz={} depth={}",
            c.phase,
            c.level,
            c.s,
            c.legs,
            c.fizz,
            self.stack.len()
        )
    }

    /// Worst instant adverse move (% of start) for a position in `direction`.
    fn worst_move(&self, direction: i32) -> f64 {
        let ent = self.cur();
        let base = if ent.fizz && self.stack.len() >= 2 {
            &self.stack[self.stack.len() - 2]
        } else {
            ent
        };
        let floor = self.p.worst_floor;
        match base.phase {
            Phase::Pre => self.p.worst_leg.max(floor),
            // The aftermath can still extend, and an extension lands in one
            // step. Its direction is not reliably known: assume the worst.
            Phase::Post => self.p.worst_post.max(self.p.worst_fizz).max(floor),
            Phase::Pause => {
                let sd = if base.s > 0.0 { 1 } else { -1 };
                let s = base.s.abs();
                // A pause ends either with a further leg (against a
                // with-shock position) or a full revert (against a
                // counter-shock one). Size for whichever is worse.
                let leg = if base.legs < 3 {
                    self.p.worst_leg
                } else {
                    self.p.worst_ext.max(self.p.worst_fizz)
                };
                if direction == sd {
                    s.max(leg).max(self.p.worst_fizz).max(floor)
                } else {
                    leg.max(self.p.worst_fizz).max(floor)
                }
            }
        }
    }

    pub fn cap_by_risk(&self, eq: f64, peak: f64, price: f64, direction: i32) -> f64 {
        let cushion = self.p.dd_limit - (peak - eq) - self.p.safety;
        if cushion <= 0.0 {
            return 0.0;
        }
        (cushion / (price * self.worst_move(direction) / 100.0))
            .min(self.p.cap)
            .max(0.0)
    }

    /// One step: the oracle `price`, the current position, equity and its
    /// peak. Returns the target position (signed shares) and why.
    pub fn step(&mut self, k: i64, price: f64, pos: f64, eq: f64, peak: f64) -> (f64, Reason) {
        let start = *self.start.get_or_insert(price);
        let lv = (price / start - 1.0) * 100.0;
        let prev = self.prev_lv.unwrap_or(lv);
        self.prev_lv = Some(lv);
        let c = lv - prev;
        if c.abs() >= self.p.sub_thr {
            self.last_change_k = k;
            self.ride_dir = if c.abs() >= self.p.ride_min && c.abs() < self.p.ride_max {
                if c > 0.0 { 1 } else { -1 }
            } else {
                0
            };
        }
        let running = (k - self.last_change_k) < self.p.quiet_steps;
        let riding = (k - self.last_change_k) < self.p.ride_hold && self.ride_dir != 0;

        // an open fizzle candidate that has lived too long becomes a real level
        if self.cur().fizz && Self::secs(k - self.cur().k_move) > self.p.fizzle_timeout {
            self.settle_hard();
        }

        let dev = lv - self.cur().level;
        let (mut target, reason);
        if dev.abs() >= self.p.move_thr {
            if self.move_start_k.is_none() {
                self.move_start_k = Some(k);
            }
            if running {
                if riding {
                    target = self.ride_dir as f64 * self.p.size_ride;
                    reason = Reason::Ride;
                } else {
                    (target, reason) = self.hold_target();
                }
            } else {
                self.new_level(lv, k);
                self.move_start_k = None;
                (target, reason) = self.hold_target();
            }
        } else {
            self.move_start_k = None;
            if riding {
                target = self.ride_dir as f64 * self.p.size_ride;
                reason = Reason::Ride;
            } else {
                (target, reason) = self.hold_target();
            }
        }

        if target != 0.0 {
            let dir = if target > 0.0 { 1 } else { -1 };
            let capr = self.cap_by_risk(eq, peak, price, dir);
            if target.abs() > capr {
                target = capr.copysign(target);
            }
        }
        // A hard stop well short of the mandate's limit: once this much is
        // lost from the peak, the session is over as far as risk goes.
        let mut reason = reason;
        if peak - eq >= self.p.halt_dd {
            target = 0.0;
            reason = Reason::Halt;
        }
        if Self::secs(self.n - 1 - k) < self.p.end_guard_s && target.abs() > pos.abs() + 0.01 {
            target = pos;
        }
        (target, reason)
    }

    fn new_level(&mut self, lv: f64, k: i64) {
        let k_move = self.move_start_k.unwrap_or(k);
        let n = self.stack.len();
        let cur = self.stack[n - 1].clone();
        let m = (lv - cur.level).abs();
        // 1. fizzle return: back to the level below the current one
        if n >= 2 {
            let below = &self.stack[n - 2];
            let exc = (cur.level - below.level).abs();
            if (lv - below.level).abs() <= self.p.return_tol
                && exc <= self.p.fizzle_max + 0.3
                && Self::secs(k - cur.k_move) <= self.p.fizzle_timeout + 1.5
                && cur.fizz
            {
                self.stack.pop();
                return;
            }
        }
        // 2. otherwise push a new level; decide whether it is a fizzle candidate
        let since_leg = cur.k_leg.map(|kl| Self::secs(k_move - kl));
        let surely_fizz = m <= self.p.fizzle_max + 0.3
            && match cur.phase {
                Phase::Pre => Self::secs(k_move) < 10.0,
                Phase::Pause => since_leg.map_or(false, |s| s < self.p.pause_min - 0.5),
                Phase::Post => true,
            };
        let maybe_fizz = m <= self.p.fizzle_max;
        let ent = Level {
            level: lv,
            phase: cur.phase,
            s: cur.s,
            legs: cur.legs,
            k_leg: cur.k_leg,
            fizz: surely_fizz || maybe_fizz,
            k_move,
        };
        self.stack.push(ent);
        if !(surely_fizz || maybe_fizz) {
            self.settle_hard();
        }
    }

    /// The top of the stack is a real new level (not a fizzle): classify it.
    fn settle_hard(&mut self) {
        let n = self.stack.len();
        self.stack[n - 1].fizz = false;
        if n < 2 {
            return;
        }
        let below = self.stack[n - 2].clone();
        let ent = &mut self.stack[n - 1];
        let d = if ent.level > below.level { 1 } else { -1 };
        let m = (ent.level - below.level).abs();
        match below.phase {
            Phase::Pre => {
                ent.phase = Phase::Pause;
                ent.s = ent.level - below.level;
                ent.legs = 1;
                ent.k_leg = Some(ent.k_move);
            }
            Phase::Pause => {
                let sd = if below.s > 0.0 { 1 } else { -1 };
                if d == sd {
                    if below.legs >= 3 {
                        if m >= 1.0 {
                            ent.phase = Phase::Post;
                        }
                    } else {
                        ent.s = below.s + (ent.level - below.level);
                        ent.legs = below.legs + 1;
                        ent.k_leg = Some(ent.k_move);
                    }
                } else {
                    ent.phase = Phase::Post;
                }
            }
            Phase::Post => {
                if m > self.p.fizzle_max + 0.3 {
                    ent.phase = Phase::Pause;
                    ent.s = ent.level - below.level;
                    ent.legs = 1;
                    ent.k_leg = Some(ent.k_move);
                }
            }
        }
        if self.stack.len() > 3 {
            self.stack.remove(0);
        }
    }

    fn hold_target(&self) -> (f64, Reason) {
        let n = self.stack.len();
        let cur = &self.stack[n - 1];
        if cur.fizz && n >= 2 {
            let below = &self.stack[n - 2];
            let d = if cur.level > below.level { 1.0 } else { -1.0 };
            if below.phase == Phase::Pause {
                let sd = if below.s > 0.0 { 1.0 } else { -1.0 };
                let s = below.s.abs();
                if d != sd && s >= self.p.s_fade0 {
                    return self.pause_target(below);
                }
            }
            return (-d * self.p.size_fizz, Reason::Fizz);
        }
        if cur.phase == Phase::Pause {
            return self.pause_target(cur);
        }
        (0.0, Reason::Flat)
    }

    fn pause_target(&self, ent: &Level) -> (f64, Reason) {
        let s = ent.s.abs();
        let d = if ent.s > 0.0 { 1.0 } else { -1.0 };
        if s < self.p.s_mom {
            return (d * self.p.size_mom, Reason::PauseMom);
        }
        if s >= self.p.s_fade0 {
            let f = if self.p.s_fade1 > self.p.s_fade0 {
                ((s - self.p.s_fade0) / (self.p.s_fade1 - self.p.s_fade0)).min(1.0)
            } else {
                1.0
            };
            let f = f.max(0.5);
            return (-d * self.p.size_fade * f, Reason::PauseFade);
        }
        (0.0, Reason::Flat)
    }
}
