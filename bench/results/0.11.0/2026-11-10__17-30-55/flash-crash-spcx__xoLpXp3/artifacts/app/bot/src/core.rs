//! The decision core: a pure function of the mark path and the account's
//! state to a target position. Shared by the bot and the offline simulator.

#[derive(Debug, Clone)]
pub struct Params {
    /// Displacement from the anchor (fraction) that arms a fade.
    pub enter: f64,
    /// Displacement at which the fade is at full size.
    pub full_at: f64,
    /// Smallest size fraction at `enter`.
    pub min_frac: f64,
    /// Steps the mark must be still before fading.
    pub still_steps: usize,
    /// Largest per-step move (fraction) counted as still.
    pub still_step: f64,
    /// Largest net move over the still window counted as still.
    pub still_net: f64,
    /// Adverse move from the entry mark (fraction) that stops the fade.
    pub stop: f64,
    /// Recovered share of the displacement that takes profit outright.
    pub tp_full: f64,
    /// Recovered share after which a pause in the revert takes profit.
    pub tp_partial: f64,
    /// Steps to hold a fade before giving up (stall).
    pub max_hold: usize,
    /// Steps to stay flat after a stop.
    pub cooldown: usize,
    /// Window (steps) of the anchor's median while flat.
    pub anchor_win: usize,
    /// Adverse move (fraction) the size is budgeted against.
    pub risk_move: f64,
    /// Share of the drawdown room to risk.
    pub risk_share: f64,
    /// Drawdown room (USD) below which the bot stops trading.
    pub min_room: f64,
    /// Round-trip cost estimate (fraction of notional) per unit traded.
    pub cost: f64,
    /// Size multiplier applied after each stop in the session.
    pub restop_frac: f64,
    /// Stops after which the bot halts for the session.
    pub max_stops: usize,
    /// One-step jump (fraction) that triggers a momentum ride; 0 disables.
    pub mom_jump: f64,
    /// Size fraction of the momentum ride.
    pub mom_frac: f64,
    /// Hard cap on size (units), under the mandate's.
    pub max_size: f64,
    /// Per-step move (fraction) in the ride's direction that counts as the move continuing.
    pub mom_cont: f64,
    /// Quiet steps that end a ride.
    pub mom_pause: usize,
    /// Whether to fade displacements at all.
    pub fade: bool,
    /// Net move over 3 steps (fraction) that also triggers a ride; 0 disables.
    pub mom_net: f64,
    /// Don't start a ride toward the anchor that lands within this (fraction) of it.
    pub anchor_zone: f64,
    /// Jumps below this (fraction) ride at half size.
    pub half_below: f64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            enter: 0.012,
            full_at: 0.03,
            min_frac: 0.35,
            still_steps: 4,
            still_step: 0.003,
            still_net: 0.004,
            stop: 0.025,
            tp_full: 0.85,
            tp_partial: 0.4,
            max_hold: 80,
            cooldown: 2,
            anchor_win: 60,
            risk_move: 0.05,
            risk_share: 0.55,
            min_room: 200.0,
            cost: 0.0015,
            restop_frac: 0.5,
            max_stops: 2,
            mom_jump: 0.01,
            mom_frac: 1.0,
            max_size: 55.0,
            mom_cont: 0.003,
            mom_pause: 4,
            fade: false,
            mom_net: 0.0,
            anchor_zone: 0.015,
            half_below: 0.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Flat,
    Fade { entry_mark: f64, anchor: f64, since: usize },
    Cooldown { until: usize },
    /// Riding a move while it runs.
    Momentum { dir: f64, last_move: usize },
}

#[derive(Debug, Clone)]
pub struct Core {
    pub p: Params,
    marks: Vec<f64>,
    anchor: f64,
    mode: Mode,
    /// Estimated account PnL (USD), net of estimated costs, and its peak.
    pub equity: f64,
    pub peak: f64,
    last_pos: f64,
    last_mark: f64,
    pub cap: f64,
    pub max_dd: f64,
    pub halted: bool,
    stops: usize,
}

impl Core {
    pub fn new(p: Params, cap: f64, max_dd: f64) -> Self {
        Core {
            p,
            marks: Vec::new(),
            anchor: 0.0,
            mode: Mode::Flat,
            equity: 0.0,
            peak: 0.0,
            last_pos: 0.0,
            last_mark: 0.0,
            cap,
            max_dd,
            halted: false,
            stops: 0,
        }
    }

    pub fn anchor(&self) -> f64 {
        self.anchor
    }

    fn median(&self, n: usize) -> f64 {
        let len = self.marks.len();
        let start = len.saturating_sub(n);
        let mut w: Vec<f64> = self.marks[start..].to_vec();
        w.sort_by(|a, b| a.partial_cmp(b).unwrap());
        w[w.len() / 2]
    }

    fn still(&self) -> bool {
        let n = self.marks.len();
        if n < self.p.still_steps + 1 {
            return false;
        }
        let w = &self.marks[n - self.p.still_steps - 1..];
        for i in 1..w.len() {
            if (w[i] / w[i - 1] - 1.0).abs() > self.p.still_step {
                return false;
            }
        }
        (w[w.len() - 1] / w[0] - 1.0).abs() <= self.p.still_net
    }

    /// Account the step's mark move on the position held into it, and the
    /// cost of the trade that got us here.
    fn account(&mut self, mark: f64, position: f64) {
        if self.last_mark > 0.0 {
            self.equity += self.last_pos * (mark - self.last_mark);
            let traded = (position - self.last_pos).abs();
            self.equity -= traded * mark * self.p.cost;
        }
        self.last_mark = mark;
        self.last_pos = position;
        self.peak = self.peak.max(self.equity);
    }

    pub fn drawdown(&self) -> f64 {
        self.peak - self.equity
    }

    /// Size (units) for a ride: the drawdown room allows, under the cap.
    fn ride_size(&self, mark: f64) -> f64 {
        let room = self.max_dd - self.drawdown();
        if room < self.p.min_room {
            return 0.0;
        }
        (room * 0.6 / (0.03 * mark)).min(self.cap)
    }

    /// Size (units) the drawdown room allows.
    fn allowed_size(&self, mark: f64) -> f64 {
        let room = self.max_dd - self.drawdown();
        if room < self.p.min_room {
            return 0.0;
        }
        let scale = self.p.restop_frac.powi(self.stops as i32);
        (room * self.p.risk_share / (self.p.risk_move * mark) * scale).min(self.cap.min(self.p.max_size) * scale)
    }

    /// One step: the mark now and the position held into it. Returns the
    /// target position for the step.
    pub fn step(&mut self, k: usize, mark: f64, position: f64) -> f64 {
        self.account(mark, position);
        self.marks.push(mark);
        if self.anchor == 0.0 {
            self.anchor = mark;
        }
        let p = self.p.clone();
        let flat = position.abs() < 0.5;

        // Drawdown halt: never trade again this session.
        if self.drawdown() > self.max_dd * 0.8 {
            self.halted = true;
        }
        if self.halted {
            self.mode = Mode::Flat;
            return 0.0;
        }

        let n = self.marks.len();
        let jump = if n >= 2 { self.marks[n - 1] / self.marks[n - 2] - 1.0 } else { 0.0 };

        // A jump anywhere: ride it, whatever we were doing.
        let net3 = if n >= 4 { self.marks[n - 1] / self.marks[n - 4] - 1.0 } else { 0.0 };
        let riding = matches!(self.mode, Mode::Momentum { .. });
        let trigger = (p.mom_jump > 0.0 && jump.abs() >= p.mom_jump)
            || (p.mom_net > 0.0 && !riding && net3.abs() >= p.mom_net && jump * net3 > 0.0);
        // Re-anchor when the market has sat still for a long while.
        if !riding && position.abs() < 0.5 && n >= p.anchor_win {
            let w = &self.marks[n - p.anchor_win..];
            let (lo, hi) = w.iter().fold((f64::MAX, f64::MIN), |(l, h), &x| (l.min(x), h.max(x)));
            if hi / lo - 1.0 < 0.01 {
                self.anchor = self.median(p.anchor_win);
            }
        }
        let disp_now = mark / self.anchor - 1.0;
        let toward_anchor = jump * disp_now < 0.0 || disp_now.abs() < 1e-9;
        let skip = p.anchor_zone > 0.0 && toward_anchor && disp_now.abs() < p.anchor_zone;
        if trigger && !skip {
            let mut size = (p.mom_frac * self.ride_size(mark)).floor();
            if p.half_below > 0.0 && jump.abs() < p.half_below {
                size = (size / 2.0).floor();
            }
            if size >= 1.0 {
                let dir = if jump.abs() >= p.mom_jump { jump.signum() } else { net3.signum() };
                if let Mode::Fade { entry_mark, anchor, .. } = self.mode {
                    let fade_dir = if entry_mark > anchor { -1.0 } else { 1.0 };
                    if fade_dir != dir {
                        self.stops += 1;
                        if self.stops >= p.max_stops {
                            self.halted = true;
                        }
                    }
                }
                self.mode = Mode::Momentum { dir, last_move: k };
                return dir * size;
            }
        }

        match self.mode {
            Mode::Momentum { dir, last_move } => {
                if jump * dir >= p.mom_cont {
                    self.mode = Mode::Momentum { dir, last_move: k };
                    return position;
                }
                if jump * dir <= -p.mom_cont || k - last_move >= p.mom_pause {
                    self.mode = Mode::Flat;
                    // fall through to the fade logic below, flat
                } else {
                    return position;
                }
            }
            Mode::Cooldown { until } => {
                if k >= until {
                    self.mode = Mode::Flat;
                }
                return 0.0;
            }
            Mode::Fade {
                entry_mark,
                anchor,
                since,
            } => {
                let dir = if entry_mark > anchor { -1.0 } else { 1.0 }; // position sign
                let disp0 = entry_mark / anchor - 1.0;
                let disp = mark / anchor - 1.0;
                let adverse = -dir * (mark / entry_mark - 1.0);
                if adverse > p.stop {
                    self.stops += 1;
                    if self.stops >= p.max_stops {
                        self.halted = true;
                    }
                    self.mode = Mode::Cooldown { until: k + p.cooldown };
                    return 0.0;
                }
                let recovered = 1.0 - disp / disp0;
                let recent = if n >= 3 {
                    dir * (self.marks[n - 1] / self.marks[n - 3] - 1.0)
                } else {
                    0.0
                };
                let done = recovered >= p.tp_full
                    || (recovered >= p.tp_partial && recent < 0.0015)
                    || k - since > p.max_hold;
                if done {
                    // The stretch goes on around where it settled.
                    self.anchor = mark;
                    self.mode = Mode::Cooldown { until: k + p.cooldown };
                    return 0.0;
                }
                return position;
            }
            Mode::Flat => {}
        }
        if !p.fade {
            return 0.0;
        }
        // Flat: track the anchor while quiet, look for a displacement.
        let disp = mark / self.anchor - 1.0;
        if flat && disp.abs() < p.enter {
            self.anchor = self.median(p.anchor_win);
        }
        let disp = mark / self.anchor - 1.0;
        if disp.abs() >= p.enter && self.still() {
            let frac = ((disp.abs() - p.enter) / (p.full_at - p.enter) * (1.0 - p.min_frac)
                + p.min_frac)
                .clamp(p.min_frac, 1.0);
            let size = (frac * self.allowed_size(mark)).floor();
            if size >= 1.0 {
                self.mode = Mode::Fade {
                    entry_mark: mark,
                    anchor: self.anchor,
                    since: k,
                };
                return -disp.signum() * size;
            }
        }
        0.0
    }
}
