//! The shock strategy, as a pure state machine over the mark: ride a move
//! while it lands, fade it once it stops, sized by what the phase says a
//! fade is worth, inside a drawdown guard. Shared by the bot and the
//! offline backtest.

fn env(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

#[derive(Debug, Clone)]
pub struct Params {
    /// A level change (% of the starting mark) this far from the quiet anchor starts a move.
    pub thr: f64,
    /// A step this far the move's way keeps it alive.
    pub cont: f64,
    /// Quiet steps after which a move is over.
    pub quiet: i64,
    /// Ride a move in progress once it's this large, at this share of the cap.
    pub ride_min: f64,
    pub ride_size: f64,
    /// Fade a held shock of at least this size, by legs so far (1, 2, 3).
    pub fade_min: [f64; 3],
    pub fade_size: f64,
    /// A move larger than this is a shock leg, not a fizzle.
    pub leg_min: f64,
    /// Fizzle fades: smallest move to fade, and sizes by phase.
    pub fizz_min: f64,
    pub fizz_pre: f64,
    pub fizz_hold: f64,
    pub fizz_post: f64,
    /// Steps a fizzle may hold before it's taken for a leg (or a stall).
    pub fizz_wait: i64,
    /// Steps a shock may hold before the aftermath is taken as over.
    pub hold_max: i64,
    /// A giveback of this share of the shock is the revert.
    pub revert_share: f64,
    /// Drawdown guard: budget in USD, and the worst one-step move to survive.
    pub dd_budget: f64,
    pub step_risk: f64,
    /// Cost per unit traded, in basis points (the backtest and the guard's equity).
    pub cost_bps: f64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            thr: env("P_THR", 0.35),
            cont: env("P_CONT", 0.15),
            quiet: env("P_QUIET", 3.0) as i64,
            ride_min: env("P_RIDE_MIN", 0.35),
            ride_size: env("P_RIDE_SIZE", 1.0),
            fade_min: [
                env("P_FADE1", 2.1),
                env("P_FADE2", 1.8),
                env("P_FADE3", 0.9),
            ],
            fade_size: env("P_FADE_SIZE", 1.0),
            leg_min: env("P_LEG_MIN", 2.0),
            fizz_min: env("P_FIZZ_MIN", 0.5),
            fizz_pre: env("P_FIZZ_PRE", 1.0),
            fizz_hold: env("P_FIZZ_HOLD", 1.0),
            fizz_post: env("P_FIZZ_POST", 1.0),
            fizz_wait: env("P_FIZZ_WAIT", 21.0) as i64,
            hold_max: env("P_HOLD_MAX", 90.0) as i64,
            revert_share: env("P_REVERT", 0.45),
            dd_budget: env("P_DD", 700.0),
            step_risk: env("P_STEP_RISK", 0.048),
            cost_bps: env("P_COST", 9.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    Pre,
    Hold,
    Post,
}

#[derive(Debug, Clone, Copy)]
struct Move {
    d: f64,
    start: f64,
    last: i64,
}

#[derive(Debug, Clone, Copy)]
struct Pending {
    d: f64,
    m: f64,
    k: i64,
}

pub struct Strat {
    pub p: Params,
    m0: f64,
    k: i64,
    prev_lvl: f64,
    prev_mark: f64,
    prev_pos: f64,
    anc_sum: f64,
    anc_n: f64,
    mv: Option<Move>,
    pend: Option<Pending>,
    pub phase: Phase,
    shock_d: f64,
    shock_s: f64,
    legs: usize,
    hold_since: i64,
    /// The fade, as a signed share of the cap.
    fade: f64,
    pub eq: f64,
    pub peak: f64,
}

impl Strat {
    pub fn new(p: Params) -> Self {
        Strat {
            p,
            m0: 0.0,
            k: -1,
            prev_lvl: 0.0,
            prev_mark: 0.0,
            prev_pos: 0.0,
            anc_sum: 0.0,
            anc_n: 0.0,
            mv: None,
            pend: None,
            phase: Phase::Pre,
            shock_d: 0.0,
            shock_s: 0.0,
            legs: 0,
            hold_since: 0,
            fade: 0.0,
            eq: 0.0,
            peak: 0.0,
        }
    }

    fn hold_fade(&self) -> f64 {
        let i = self.legs.clamp(1, 3) - 1;
        if self.shock_s >= self.p.fade_min[i] {
            -self.shock_d * self.p.fade_size
        } else {
            0.0
        }
    }

    fn to_post(&mut self) {
        self.phase = Phase::Post;
        self.fade = 0.0;
    }

    fn new_leg(&mut self, d: f64, m: f64, k: i64) {
        if self.phase == Phase::Pre {
            self.phase = Phase::Hold;
            self.shock_d = d;
            self.shock_s = m;
            self.legs = 1;
        } else if self.legs >= 3 {
            // A fourth move the shock's way can only be the extension.
            self.to_post();
            return;
        } else {
            self.legs += 1;
            self.shock_s += m;
        }
        self.hold_since = k;
        self.fade = self.hold_fade();
    }

    fn fizz_size(&self) -> f64 {
        match self.phase {
            Phase::Pre => self.p.fizz_pre,
            Phase::Hold => self.p.fizz_hold,
            Phase::Post => self.p.fizz_post,
        }
    }

    fn on_move(&mut self, d: f64, m: f64, k: i64) {
        if m < self.p.thr {
            return;
        }
        // A move back from an unresolved small move is its giveback.
        if let Some(pend) = self.pend {
            if pend.d != d {
                self.pend = None;
                if m > pend.m + self.p.leg_min {
                    // Far more than a giveback: a shock leg in its own right.
                    self.new_leg_or_revert(d, m - pend.m, k);
                    return;
                }
                self.fade = if self.phase == Phase::Hold {
                    self.hold_fade()
                } else {
                    0.0
                };
                return;
            }
            // Same way again: one larger move.
            self.pend = None;
            self.on_move(d, m + pend.m, k);
            return;
        }
        self.new_leg_or_revert(d, m, k);
    }

    fn new_leg_or_revert(&mut self, d: f64, m: f64, k: i64) {
        match self.phase {
            Phase::Pre => {
                if m > self.p.leg_min {
                    self.new_leg(d, m, k);
                } else {
                    self.set_pending(d, m, k);
                }
            }
            Phase::Hold => {
                if d == self.shock_d {
                    if m > self.p.leg_min {
                        self.new_leg(d, m, k);
                    } else {
                        self.set_pending(d, m, k);
                        // Keep the larger of the fizzle fade and the shock's fade.
                        let hold = self.hold_fade();
                        if hold.abs() > self.fade.abs() {
                            self.fade = hold;
                        }
                    }
                } else if m >= self.p.revert_share * self.shock_s {
                    self.to_post();
                } else {
                    self.set_pending(d, m, k);
                }
            }
            Phase::Post => self.set_pending(d, m, k),
        }
    }

    fn set_pending(&mut self, d: f64, m: f64, k: i64) {
        self.pend = Some(Pending { d, m, k });
        self.fade = if m >= self.p.fizz_min {
            -d * self.fizz_size()
        } else {
            0.0
        };
    }

    /// One step: the mark and the position as read; returns the position to hold.
    pub fn on_step(&mut self, mark: f64, pos: f64, cap: f64) -> f64 {
        self.k += 1;
        let k = self.k;
        if k == 0 {
            self.m0 = mark;
            self.prev_mark = mark;
        }
        let lvl = (mark / self.m0 - 1.0) * 100.0;
        // Equity, on the mark, with a cost for what traded since the last step.
        self.eq += pos * (mark - self.prev_mark)
            - (pos - self.prev_pos).abs() * self.prev_mark * self.p.cost_bps / 10_000.0;
        self.peak = self.peak.max(self.eq);
        let dl = lvl - self.prev_lvl;

        if let Some(mut mv) = self.mv {
            if dl * mv.d >= self.p.cont {
                mv.last = k;
                self.mv = Some(mv);
            } else if dl * mv.d <= -self.p.thr {
                // It turned at once: the move ended at the last level.
                let m = (self.prev_lvl - mv.start) * mv.d;
                self.on_move(mv.d, m, k - 1);
                self.mv = Some(Move {
                    d: -mv.d,
                    start: self.prev_lvl,
                    last: k,
                });
            } else if k - mv.last >= self.p.quiet {
                let m = (lvl - mv.start) * mv.d;
                self.mv = None;
                self.anc_sum = lvl;
                self.anc_n = 1.0;
                self.on_move(mv.d, m, k);
            }
        } else {
            let anchor = if self.anc_n > 0.0 {
                self.anc_sum / self.anc_n
            } else {
                lvl
            };
            let dev = lvl - anchor;
            if dev.abs() >= self.p.thr {
                self.mv = Some(Move {
                    d: dev.signum(),
                    start: anchor,
                    last: k,
                });
            } else {
                self.anc_sum += lvl;
                self.anc_n += 1.0;
            }
        }

        if self.mv.is_none() {
            if let Some(pend) = self.pend {
                if k - pend.k > self.p.fizz_wait {
                    self.pend = None;
                    match self.phase {
                        Phase::Pre => self.new_leg(pend.d, pend.m, pend.k),
                        Phase::Hold => {
                            if pend.d == self.shock_d {
                                self.new_leg(pend.d, pend.m, pend.k);
                            } else {
                                self.to_post();
                            }
                        }
                        Phase::Post => self.fade = 0.0,
                    }
                }
            }
            if self.phase == Phase::Hold && k - self.hold_since > self.p.hold_max {
                self.to_post();
            }
        }

        let mut target = self.fade * cap;
        if let Some(mv) = self.mv {
            let so_far = (lvl - mv.start) * mv.d;
            if so_far >= self.p.ride_min && self.p.ride_size > 0.0 {
                target = mv.d * self.p.ride_size * cap;
            }
        }
        // The guard: what a worst step against the position may still cost.
        let room = (self.p.dd_budget - (self.peak - self.eq)).max(0.0);
        let max_abs = (room / (mark * self.p.step_risk)).floor().min(cap);
        target = target.clamp(-max_abs, max_abs).round();

        self.prev_lvl = lvl;
        self.prev_mark = mark;
        self.prev_pos = pos;
        target
    }
}
