//! The trading logic, pure: fed one price per step, it returns a target
//! position. Shared by the live bot (`main.rs`) and the offline simulator
//! (`bin/sim.rs`).
//!
//! Model of the market (from the task): the price sits at a level with
//! small wobbles (< 0.3%), then moves in "events": a fizzle (0.4-2%, fully
//! reverts within 10 s), a shock leg (0.6-8%, holds 8-35 s, up to three
//! legs, each followed by another with odds of a third), or an aftermath
//! (revert of 50-100%, stall, or extend by 1-5%). Moves land at once or
//! ramp in sub-steps 3-10 slots apart over 1-7 s.
//!
//! Policy, by expected value under that model:
//! - while a move is running (a sub-step landed in the last few steps),
//!   ride it: ramps continue;
//! - a fresh small move may be a fizzle: fade it lightly until it has held
//!   too long to be one;
//! - a small shock is more likely to continue (another leg, or an extend)
//!   than to give back much: ride it; a big shock is faded;
//! - a big move against the shock after it held is the aftermath's revert:
//!   the level it reaches becomes the new anchor and only fizzles remain;
//! - every position is capped so one more worst-case move cannot breach the
//!   drawdown limit.

#[derive(Debug, Clone)]
pub struct Config {
    pub max_position: f64,
    pub max_drawdown: f64,
    /// One-step price change (fraction) that counts as a move sub-step.
    pub move_threshold: f64,
    /// A move is "running" for this many steps after its last sub-step.
    pub run_steps: u64,
    /// Sub-steps this close (in steps) belong to the same move.
    pub gap_steps: u64,
    /// Displacement from the anchor below which nothing is done.
    pub d_lo: f64,
    /// Shock displacement above which the shock is faded rather than ridden.
    pub d_cross: f64,
    /// Steps after a move's start during which it may still be a fizzle.
    pub fizzle_steps: u64,
    /// Largest move (fraction) that can be a fizzle.
    pub fizzle_max: f64,
    /// Position weight (0..1 of the cap) fading a fizzle candidate.
    pub w_fizzle: f64,
    /// Position weight riding a small shock during its hold.
    pub w_small: f64,
    /// Position weight fading a big shock.
    pub w_big: f64,
    /// Position weight riding a running move.
    pub w_run: f64,
    /// A move whose only sub-step so far is bigger than this (fraction)
    /// landed at once: it is not ridden.
    pub ride_max_first: f64,
    /// The chain lands a jump above ~4% in equal pieces on consecutive
    /// steps: a first sub-step at least this big is one such piece, and
    /// the next follows for sure.
    pub split_min: f64,
    /// Position weight fading moves after the aftermath (fizzles only).
    pub w_post: f64,
    /// Worst instant move against a fade (a further leg).
    pub worst_fade: f64,
    /// Floor on the worst instant move against a ride.
    pub worst_ride_floor: f64,
    /// Drawdown budget kept in reserve for exit costs and wobble, USD.
    pub cushion: f64,
    /// Steps a level must hold before it becomes the new anchor regardless.
    pub reanchor_steps: u64,
    /// Band (fraction) within which the price counts as "at a level".
    pub level_band: f64,
    /// An opposite move giving back at least this fraction of the shock
    /// is its revert.
    pub revert_frac: f64,
    /// Smallest position change worth sending, in shares.
    pub min_trade: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_position: 70.0,
            max_drawdown: 1000.0,
            move_threshold: 0.002,
            run_steps: 4,
            gap_steps: 5,
            d_lo: 0.003,
            d_cross: 0.06,
            fizzle_steps: 24,
            fizzle_max: 0.021,
            w_fizzle: 1.0,
            w_small: 0.0,
            w_big: 1.0,
            w_run: 1.0,
            ride_max_first: 0.015,
            split_min: 0.02,
            w_post: 1.0,
            worst_fade: 0.09,
            worst_ride_floor: 0.02,
            cushion: 80.0,
            reanchor_steps: 72,
            level_band: 0.003,
            revert_frac: 0.5,
            min_trade: 3.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Idle,
    Run,
    Fizzle,
    RideShock,
    FadeShock,
    Post,
}

#[derive(Debug, Clone)]
pub struct Strategy {
    pub cfg: Config,
    /// Reference level: the pre-shock level, or the post-aftermath level.
    pub anchor: f64,
    /// Level tracking for the time-based re-anchor.
    level: f64,
    level_since: u64,
    pub peak_value: f64,
    last_price: f64,
    /// The current move: direction, the level it left, when it started,
    /// when its last sub-step landed, how many sub-steps, and the first's size.
    move_dir: f64,
    sub_steps: u32,
    first_sub_step: f64,
    /// The move was a split jump whose pieces have all landed.
    landed: bool,
    /// The current move was counted as a shock leg.
    move_counted: bool,
    move_from: f64,
    move_start: u64,
    last_sub_step: Option<u64>,
    /// Shock bookkeeping.
    shock_dir: f64,
    pub legs: u32,
    pub shock_done: bool,
    pub mode: Mode,
}

impl Strategy {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            anchor: 0.0,
            level: 0.0,
            level_since: 0,
            peak_value: f64::NEG_INFINITY,
            last_price: 0.0,
            move_dir: 0.0,
            sub_steps: 0,
            first_sub_step: 0.0,
            landed: false,
            move_counted: false,
            move_from: 0.0,
            move_start: 0,
            last_sub_step: None,
            shock_dir: 0.0,
            legs: 0,
            shock_done: false,
            mode: Mode::Idle,
        }
    }

    /// Largest absolute position the drawdown budget allows against an
    /// instant adverse move of `worst` (fraction).
    pub fn cap(&self, price: f64, value: f64, worst: f64) -> f64 {
        let dd = (self.peak_value - value).max(0.0);
        let budget = self.cfg.max_drawdown - dd - self.cfg.cushion;
        (budget / (price * worst.max(0.005))).clamp(0.0, self.cfg.max_position)
    }

    pub fn running(&self, k: u64) -> bool {
        !self.landed && matches!(self.last_sub_step, Some(s) if k - s < self.cfg.run_steps)
    }

    /// A running move worth riding: a ramp (several sub-steps) or a first
    /// sub-step small enough to be the start of one.
    fn rideable(&self) -> bool {
        self.sub_steps > 1
            || self.first_sub_step <= self.cfg.ride_max_first
            || self.first_sub_step >= self.cfg.split_min
    }

    fn reanchor(&mut self, price: f64, k: u64) {
        self.anchor = price;
        self.level = price;
        self.level_since = k;
        self.last_sub_step = None;
        self.move_dir = 0.0;
        self.landed = false;
    }

    /// One step: the price (oracle), current position, account value.
    /// Returns the target position in shares.
    pub fn on_step(&mut self, k: u64, price: f64, position: f64, value: f64) -> f64 {
        if value > self.peak_value {
            self.peak_value = value;
        }
        if self.anchor == 0.0 {
            self.anchor = price;
            self.level = price;
            self.level_since = k;
            self.last_price = price;
            return 0.0;
        }
        let prev = self.last_price;
        let change = price / prev - 1.0;
        self.last_price = price;

        // Sub-step detection and move bookkeeping.
        if change.abs() >= self.cfg.move_threshold {
            let dir = change.signum();
            let continues = dir == self.move_dir
                && matches!(self.last_sub_step, Some(s) if k - s <= self.cfg.gap_steps);
            if !continues {
                // A new move starts from the level before this step.
                self.move_dir = dir;
                self.move_from = prev;
                self.move_start = k;
                self.sub_steps = 0;
                self.first_sub_step = change.abs();
                self.landed = false;
                self.move_counted = false;
                let d_before = prev / self.anchor - 1.0;
                if !self.shock_done {
                    if self.shock_dir == 0.0 && d_before.abs() < self.cfg.d_lo {
                        // First move away from the anchor: a leg or a fizzle.
                        self.shock_dir = dir;
                        self.legs = 1;
                        self.move_counted = true;
                    } else if dir == self.shock_dir {
                        self.legs += 1;
                        self.move_counted = true;
                    }
                }
            }
            self.last_sub_step = Some(k);
            self.sub_steps += 1;
            if self.sub_steps == 2
                && self.first_sub_step >= self.cfg.split_min
                && (change.abs() - self.first_sub_step).abs() <= 0.1 * self.first_sub_step
            {
                self.landed = true;
            }
        }

        let d = price / self.anchor - 1.0;

        // A counted move that returns to the level it left within the
        // fizzle window was a fizzle, not a leg.
        if self.move_counted
            && !self.shock_done
            && ((price - self.move_from) / self.anchor).abs() < self.cfg.d_lo
            && k - self.move_start <= self.cfg.fizzle_steps + self.cfg.gap_steps
        {
            self.move_counted = false;
            self.legs = self.legs.saturating_sub(1);
            if self.legs == 0 {
                self.shock_dir = 0.0;
            }
        }

        // The aftermath's revert: a move against the shock giving back at
        // least `revert_frac` of it, once the shock had held (not running
        // the shock itself). Re-anchor at the level it reaches.
        if self.shock_dir != 0.0 && !self.shock_done && !self.running(k) {
            let from_d = self.move_from / self.anchor - 1.0;
            let given_back = (from_d - d) * self.shock_dir;
            if self.move_dir == -self.shock_dir
                && from_d.abs() > self.cfg.fizzle_max
                && given_back >= self.cfg.revert_frac * from_d.abs()
            {
                self.shock_done = true;
                self.reanchor(price, k);
            }
        }

        // Level tracking: time-based re-anchor as a fallback.
        if ((price - self.level) / self.level).abs() > self.cfg.level_band {
            self.level = price;
            self.level_since = k;
        }
        if k - self.level_since >= self.cfg.reanchor_steps
            && ((self.level - self.anchor) / self.anchor).abs() > self.cfg.level_band
        {
            self.reanchor(price, k);
            self.shock_done = true;
        }

        let d = price / self.anchor - 1.0;
        let since_move = k.saturating_sub(self.move_start);
        let move_size = ((price - self.move_from) / self.anchor).abs();

        let (mode, target) = if self.running(k) && self.rideable() {
            let cap = self.cap(price, value, d.abs().max(self.cfg.worst_ride_floor));
            (Mode::Run, self.move_dir * self.cfg.w_run * cap)
        } else if self.running(k) {
            (Mode::Idle, 0.0)
        } else if d.abs() < self.cfg.d_lo || self.move_dir == 0.0 {
            (Mode::Idle, 0.0)
        } else if self.shock_done {
            if since_move <= self.cfg.fizzle_steps {
                let cap = self.cap(price, value, self.cfg.worst_fade);
                (Mode::Post, -d.signum() * self.cfg.w_post * cap)
            } else {
                (Mode::Idle, 0.0)
            }
        } else if since_move <= self.cfg.fizzle_steps && move_size <= self.cfg.fizzle_max {
            let cap = self.cap(price, value, self.cfg.worst_fade);
            (Mode::Fizzle, -d.signum() * self.cfg.w_fizzle * cap)
        } else if self.legs >= 3 || d.abs() >= self.cfg.d_cross {
            let cap = self.cap(price, value, self.cfg.worst_fade);
            (Mode::FadeShock, -d.signum() * self.cfg.w_big * cap)
        } else {
            let cap = self.cap(price, value, d.abs().max(self.cfg.worst_ride_floor));
            (Mode::RideShock, d.signum() * self.cfg.w_small * cap)
        };
        self.mode = mode;
        let target = target.clamp(-self.cfg.max_position, self.cfg.max_position);
        // Avoid churn: only move when the change is worth it, unless the
        // position exceeds what the budget allows for an adverse 8% move.
        let hard_cap = self.cap(price, value, self.cfg.worst_fade);
        if (target - position).abs() < self.cfg.min_trade && position.abs() <= hard_cap + 1.0 {
            return position;
        }
        target
    }
}
