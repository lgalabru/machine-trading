//! The trading logic, free of any chain access so the live bot and the
//! backtester run the very same code.
//!
//! Prices come in as levels: percent away from the session's starting mark.
//! The scenario family has a few regularities this exploits:
//!
//! - **Rides.** Moves land in steps: Phoenix moves its oracle at most 4% of
//!   the starting mark per step, so bigger moves land as equal chunks on
//!   consecutive steps, and about 30% of moves ramp in uneven steps over one
//!   to seven seconds. A step off a quiet price is often followed by more
//!   the same way: ride it until the price has been quiet for a while.
//! - **Fizzles.** A quiet stretch carries at most one fizzle: a 0.4% to 2%
//!   move that gives everything back, to the exact level it left, within
//!   1.5 to 10 seconds. Fade every move that can still be one, and drop the
//!   fade once the move outlives a fizzle or outgrows one.
//! - **Shocks.** After a big enough shock (or its third and last leg), fade
//!   it for the aftermath: about two thirds of aftermaths give back half or
//!   more.
//!
//! The strategy tracks the base the price wobbles around, the shock's legs
//! and whether its aftermath has come, and classifies each move that lands
//! as a fizzle, a leg or the aftermath.

/// Tunable knobs. Levels are in percent of the starting mark, sizes in
/// shares, steps of the session's clock (two slots).
#[derive(Debug, Clone)]
pub struct Params {
    pub max_pos: f64,
    /// Rides: size, the smallest step that starts or extends one, and how
    /// many quiet steps end it.
    pub ride_size: f64,
    pub ride_jump: f64,
    pub ride_hold: u64,
    /// A first step in this range has most likely landed already: it rides
    /// at `ride_mid_scale` of the size until the move goes on.
    pub ride_mid: (f64, f64),
    pub ride_mid_scale: f64,
    /// A move off the base at least this big, once the price has stayed
    /// within `stable_range` for `settle` steps, has landed.
    pub jump: f64,
    pub settle: usize,
    pub stable_range: f64,
    /// Before step `t0` nothing but a fizzle can happen; from `fz_late` on,
    /// a first move is taken for the shock.
    pub t0: u64,
    pub fz_late: u64,
    /// Largest fizzle.
    pub fz_max_size: f64,
    /// A fizzle fade ends once the price is within `fz_exit` of where the
    /// move started; it is dropped after `fz_max_hold` steps, or once the
    /// price is `fz_break` past the largest fizzle.
    pub fz_exit: f64,
    pub fz_max_hold: u64,
    pub fz_break: f64,
    pub fz_size: f64,
    /// Fade a shock once it is at least this big, by legs seen (1, 2, 3).
    pub shock_min: [f64; 3],
    pub shock_size: f64,
    /// Drawdown budget (USD), and the per-step move to budget for: Phoenix
    /// moves its price at most 4% per step.
    pub dd_budget: f64,
    pub dd_move: f64,
    /// Trades smaller than this are skipped.
    pub min_trade: f64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            max_pos: 70.0,
            ride_size: 70.0,
            ride_jump: 0.2,
            ride_hold: 4,
            ride_mid: (1.25, 1.95),
            ride_mid_scale: 0.0,
            jump: 0.45,
            settle: 3,
            stable_range: 0.2,
            t0: 20,
            fz_late: 75,
            fz_max_size: 2.1,
            fz_exit: 0.1,
            fz_max_hold: 24,
            fz_break: 0.3,
            fz_size: 70.0,
            shock_min: [5.0, 3.0, 3.0],
            shock_size: 70.0,
            dd_budget: 750.0,
            dd_move: 0.05,
            min_trade: 2.0,
        }
    }
}

impl Params {
    /// Set one knob from `name=value`, for the backtester.
    pub fn set(&mut self, name: &str, value: &str) -> Result<(), String> {
        let f = || value.parse::<f64>().map_err(|e| format!("{name}: {e}"));
        let u = || value.parse::<u64>().map_err(|e| format!("{name}: {e}"));
        match name {
            "max_pos" => self.max_pos = f()?,
            "ride_size" => self.ride_size = f()?,
            "ride_jump" => self.ride_jump = f()?,
            "ride_hold" => self.ride_hold = u()?,
            "ride_mid_lo" => self.ride_mid.0 = f()?,
            "ride_mid_hi" => self.ride_mid.1 = f()?,
            "ride_mid_scale" => self.ride_mid_scale = f()?,
            "jump" => self.jump = f()?,
            "settle" => self.settle = u()? as usize,
            "stable_range" => self.stable_range = f()?,
            "t0" => self.t0 = u()?,
            "fz_late" => self.fz_late = u()?,
            "fz_max_size" => self.fz_max_size = f()?,
            "fz_exit" => self.fz_exit = f()?,
            "fz_max_hold" => self.fz_max_hold = u()?,
            "fz_break" => self.fz_break = f()?,
            "fz_size" => self.fz_size = f()?,
            "shock_min1" => self.shock_min[0] = f()?,
            "shock_min2" => self.shock_min[1] = f()?,
            "shock_min3" => self.shock_min[2] = f()?,
            "shock_size" => self.shock_size = f()?,
            "dd_budget" => self.dd_budget = f()?,
            "dd_move" => self.dd_move = f()?,
            "min_trade" => self.min_trade = f()?,
            _ => return Err(format!("unknown parameter {name}")),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Flat,
    /// Fading a possible fizzle that left `from`, landed at `at`, and was
    /// recognized at step `since`.
    Fizzle { from: f64, at: f64, since: u64 },
    /// Fading the shock.
    Shock,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Fizzle,
    Leg,
    Aftermath,
}

pub struct Strategy {
    pub p: Params,
    xs: Vec<f64>,
    /// The level the price wobbles around.
    base: f64,
    /// The shock so far: legs seen, direction, and level.
    legs: u32,
    shock_dir: f64,
    shock: f64,
    /// Whether the aftermath has come.
    after: bool,
    /// Whether the current quiet stretch has had its fizzle.
    fizzle_seen: bool,
    mode: Mode,
    /// The ride (direction, 0 for none), its last step, and its size.
    ride: f64,
    ride_k: u64,
    ride_scale: f64,
    /// Notes on what was decided, for logs.
    pub events: Vec<String>,
    /// Why the position is what it is: `flat`, `ride`, `fizzle` or `shock`,
    /// and a finer tag for rides and fizzles, for attribution.
    pub reason: &'static str,
    pub tag: String,
}

fn range(xs: &[f64]) -> f64 {
    let (lo, hi) = xs
        .iter()
        .fold((f64::MAX, f64::MIN), |(lo, hi), &x| (lo.min(x), hi.max(x)));
    hi - lo
}

impl Strategy {
    pub fn new(p: Params) -> Self {
        Self {
            p,
            xs: Vec::new(),
            base: 0.0,
            legs: 0,
            shock_dir: 0.0,
            shock: 0.0,
            after: false,
            fizzle_seen: false,
            mode: Mode::Flat,
            ride: 0.0,
            ride_k: 0,
            ride_scale: 1.0,
            events: Vec::new(),
            reason: "flat",
            tag: String::new(),
        }
    }

    /// Where a move to `x` happens: 0 early, 1 before the shock, 2 in a hold
    /// toward the start, 3 in a hold away from it, 4 after the aftermath.
    fn context(&self, k: u64, x: f64) -> usize {
        if self.legs == 0 {
            if k < self.p.t0 { 0 } else { 1 }
        } else if self.after {
            4
        } else if (x - self.base) * self.shock_dir < 0.0 {
            2
        } else {
            3
        }
    }

    /// What a move off the base that has just landed at `x` is taken for.
    /// Anything that can still be a fizzle is: fading one costs little when
    /// it isn't.
    fn classify(&self, k: u64, x: f64) -> Kind {
        let p = &self.p;
        let am = (x - self.base).abs();
        let can_fizzle = am <= p.fz_max_size && !self.fizzle_seen;
        if self.legs == 0 {
            if k < p.t0 || (can_fizzle && k < p.fz_late) {
                Kind::Fizzle
            } else {
                Kind::Leg
            }
        } else if self.after {
            // Nothing but a fizzle follows the aftermath.
            if am <= p.fz_max_size {
                Kind::Fizzle
            } else {
                Kind::Aftermath
            }
        } else if can_fizzle {
            Kind::Fizzle
        } else if (x - self.base) * self.shock_dir < 0.0 || self.legs >= 3 {
            Kind::Aftermath
        } else {
            Kind::Leg
        }
    }

    fn apply(&mut self, kind: Kind, k: u64, x: f64) {
        self.events
            .push(format!("k={k} base={:.2} x={x:.2} {kind:?}", self.base));
        match kind {
            Kind::Leg => {
                self.legs += 1;
                self.shock_dir = x.signum();
                self.shock = x;
                self.base = x;
                self.fizzle_seen = false;
                let i = (self.legs.min(3) - 1) as usize;
                self.mode = if self.shock.abs() >= self.p.shock_min[i] {
                    Mode::Shock
                } else {
                    Mode::Flat
                };
            }
            Kind::Aftermath => {
                self.after = true;
                self.base = x;
                self.fizzle_seen = false;
                self.mode = Mode::Flat;
            }
            Kind::Fizzle => unreachable!("fizzles are faded, not applied"),
        }
    }

    /// A move to `x` that was no fizzle: what it is instead.
    fn not_a_fizzle(&mut self, k: u64, x: f64) {
        let away = (x - self.base) * self.shock_dir > 0.0;
        if self.legs == 0 || (!self.after && away && self.legs < 3) {
            self.apply(Kind::Leg, k, x);
        } else {
            self.apply(Kind::Aftermath, k, x);
        }
    }

    /// One step: the level is `x` at step `k`, the position `q`,
    /// the drawdown so far `dd` (USD) and the price `price` (USD). Returns
    /// the position to hold.
    pub fn step(&mut self, k: u64, x: f64, q: f64, dd: f64, price: f64) -> f64 {
        let r = self.xs.last().map_or(0.0, |&prev| x - prev);
        self.xs.push(x);
        let p = self.p.clone();
        let n = self.xs.len();
        let stable = n > p.settle && range(&self.xs[n - p.settle - 1..]) < p.stable_range;
        let mut target: Option<f64> = None;

        // A fizzle being faded: back home, outlived, or outgrown?
        if let Mode::Fizzle { from, at, since } = self.mode {
            let beyond = (x - from).abs() > p.fz_max_size + p.fz_break;
            let further = stable && (x - at) * (at - from) > 0.0 && (x - at).abs() >= p.jump;
            if (x - from).abs() < p.fz_exit {
                self.events.push(format!("k={k} fizzle back to {from:.2}"));
                self.mode = Mode::Flat;
                self.fizzle_seen = true;
                self.base = from;
                target = Some(0.0);
            } else if beyond || further || k - since > p.fz_max_hold {
                // No fizzle: it landed where it first settled, and whatever
                // the price has done since is a move off that.
                self.mode = Mode::Flat;
                self.base = from;
                self.not_a_fizzle(k, at);
            }
        }

        // Rides.
        if !matches!(self.mode, Mode::Fizzle { .. }) {
            if r.abs() >= p.ride_jump {
                if self.ride == 0.0 {
                    self.ride = r.signum();
                    self.ride_k = k;
                    let ctx = self.context(k, x);
                    let b = match r.abs() {
                        a if a < 0.45 => 0,
                        a if a < 1.0 => 1,
                        a if a < 1.95 => 2,
                        _ => 3,
                    };
                    self.tag = format!("ride c{ctx} s{b}");
                    self.ride_scale = if (p.ride_mid.0..p.ride_mid.1).contains(&r.abs()) {
                        p.ride_mid_scale
                    } else {
                        1.0
                    };
                } else if r * self.ride > 0.0 {
                    self.ride_k = k;
                    self.ride_scale = 1.0;
                } else {
                    self.ride = 0.0;
                }
            }
            if self.ride != 0.0 && k - self.ride_k >= p.ride_hold {
                self.ride = 0.0;
            }
        } else {
            self.ride = 0.0;
        }

        // Landings.
        let dev = x - self.base;
        if !matches!(self.mode, Mode::Fizzle { .. })
            && dev.abs() >= p.jump
            && stable
            && target.is_none()
        {
            match self.classify(k, x) {
                Kind::Fizzle => {
                    self.events
                        .push(format!("k={k} base={:.2} x={x:.2} fizzle?", self.base));
                    let ctx = self.context(k, x);
                    let b = match dev.abs() {
                        a if a < 0.8 => 0,
                        a if a < 1.4 => 1,
                        _ => 2,
                    };
                    self.tag = format!("fizzle c{ctx} s{b}");
                    self.mode = Mode::Fizzle {
                        from: self.base,
                        at: x,
                        since: k,
                    };
                    target = Some(-p.fz_size * dev.signum());
                }
                kind => self.apply(kind, k, x),
            }
        }

        self.reason = match self.mode {
            Mode::Shock => "shock",
            Mode::Flat => "flat",
            Mode::Fizzle { .. } => "fizzle",
        };
        let mut target = target.unwrap_or(match self.mode {
            // Fade from a quiet price only: keep what's on while it moves.
            Mode::Shock if stable || q * self.shock_dir < 0.0 => -self.shock_dir * p.shock_size,
            Mode::Shock | Mode::Flat => 0.0,
            Mode::Fizzle { .. } => q,
        });
        if self.ride != 0.0 && !matches!(self.mode, Mode::Fizzle { .. }) {
            target = self.ride * p.ride_size * self.ride_scale;
            self.reason = "ride";
        }
        self.guard(target, q, dd, price)
    }

    /// Risk limits on a target: keep a step's worst move inside the drawdown
    /// budget, stay inside the position cap, and skip tiny trades.
    fn guard(&self, target: f64, q: f64, dd: f64, price: f64) -> f64 {
        let p = &self.p;
        let limit = ((p.dd_budget - dd) / (price * p.dd_move)).max(0.0);
        let target = target.clamp(-limit, limit).clamp(-p.max_pos, p.max_pos);
        if (target - q).abs() < p.min_trade {
            q
        } else {
            target
        }
    }
}
