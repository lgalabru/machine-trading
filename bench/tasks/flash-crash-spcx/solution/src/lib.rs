//! Reference: ride each move while it runs, then fade it once it stops.
//!
//! The family's shocks come in legs, among fizzles that give their move
//! back; most moves land at once, the rest ramp in uneven steps, and once a
//! shock has held, most of the time it reverts. The reference reads one
//! thing, the mark each step, against a running mean of the calm before:
//!
//! - **Calm.** A displacement of 1.14% from the calm mean, or a single step
//!   of 1.36%, starts a move.
//! - **Moving.** The full cap with the move while it makes new extremes. Three
//!   steps without one, or a retrace of 1.41% from the extreme, and the move
//!   has stopped.
//! - **Holding.** It fades what the move did: 76% of the cap against it, held
//!   until the mark has given back all but 11% of the move. A new extreme by
//!   another 1.14% is another leg: back to riding it. Moves under 1.03% aren't
//!   worth fading.
//!
//! It never carries a session past a $519 drawdown: past that it closes and
//! stops. Each step it moves the position toward its target by up to 35
//! shares, in market orders of 10 (one order that crosses much of the book
//! doesn't fit a transaction's compute budget).
//!
//! Tuned offline on 400 draws from the 0.9.0 family and checked on 200
//! others ($66 a session, profitable in 64%, no drawdown breach). It doesn't
//! know a fizzle from a leg, a stall from a reversion, or how far a shock
//! will go: the ladder's regime oracle, told those, makes several times more.
//!
//! It builds two ways: a strategy component (`bot.wasm`, the task's oracle
//! solution), and a process (`src/main.rs`), the anchor hidden sets are
//! scored against. The process is also the task's informed bot, scoring
//! v2's 1 (`MTX_RUNG=informed`; see [`Informed`]).

use std::collections::VecDeque;

use mtx_kit::prelude::*;

const ENTER_PCT: f64 = 1.14;
const STEP_PCT: f64 = 1.36;
const END_STEPS: u32 = 3;
const RETRACE_PCT: f64 = 1.41;
const RIDE: f64 = 1.0;
const FADE: f64 = 0.76;
const GIVE_BACK: f64 = 0.11;
const MIN_FADE_PCT: f64 = 1.03;
const HALT_DRAWDOWN_USD: f64 = 519.0;
/// Steps the calm mean averages over.
const CALM_STEPS: usize = 30;
const SLICE: f64 = 10.0;
const MAX_STEP_TRADE: f64 = 35.0;

#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum State {
    #[default]
    Calm,
    Moving,
    Holding,
}

/// The strategy.
#[derive(Default)]
pub struct Reference {
    cap: f64,
    start_mark: f64,
    calm: VecDeque<f64>,
    state: State,
    origin: f64,
    extreme: f64,
    dir: f64,
    quiet: u32,
    last: Option<f64>,
    value: f64,
    peak_value: f64,
    last_mark: f64,
    halted: bool,
    informed: Option<Informed>,
}

impl Strategy for Reference {
    fn start(&mut self, session: &Session) -> BotResult {
        self.cap = session.cap;
        self.start_mark = session.start_mark;
        self.last_mark = session.start_mark;
        if session.var("MTX_RUNG") == Some("informed") {
            let regime = session
                .var("MTX_REGIME")
                .ok_or("informed needs MTX_REGIME")?;
            self.informed = Some(Informed::from_regime(regime)?);
        }
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let (mark, held) = (market.mark, market.position.size);
        let level = (mark / self.start_mark - 1.0) * 100.0;
        let jump = self.last.map_or(0.0, |l| level - l);
        self.last = Some(level);
        self.value += held * (mark - self.last_mark);
        self.last_mark = mark;
        self.peak_value = self.peak_value.max(self.value);
        if !self.halted && self.peak_value - self.value > HALT_DRAWDOWN_USD {
            mtx_kit::log("drawdown past the halt; closing and stopping");
            self.halted = true;
        }

        let cap = self.cap;
        let was_calm = self.state == State::Calm;
        let mut target = 0.0;
        if self.state == State::Calm {
            self.calm.push_back(level);
            while self.calm.len() > CALM_STEPS {
                self.calm.pop_front();
            }
            let mean = self.calm.iter().sum::<f64>() / self.calm.len() as f64;
            let displacement = level - mean;
            if displacement.abs() >= ENTER_PCT || jump.abs() >= STEP_PCT {
                self.dir = if displacement.abs() >= ENTER_PCT {
                    displacement.signum()
                } else {
                    jump.signum()
                };
                self.state = State::Moving;
                (self.origin, self.extreme, self.quiet) = (mean, level, 0);
            }
        }
        if self.state == State::Moving {
            if self.dir * (level - self.extreme) > 0.0 {
                (self.extreme, self.quiet) = (level, 0);
            } else {
                self.quiet += 1;
            }
            if self.dir * (self.extreme - level) >= RETRACE_PCT || self.quiet >= END_STEPS {
                self.state = State::Holding;
            }
            target = RIDE * self.dir * cap;
        }
        if self.state == State::Holding {
            let size = (self.extreme - self.origin).abs();
            if self.dir * (level - self.extreme) >= ENTER_PCT {
                (self.state, self.extreme, self.quiet) = (State::Moving, level, 0);
                target = RIDE * self.dir * cap;
            } else if self.dir * (level - self.origin) <= GIVE_BACK * size || size < MIN_FADE_PCT {
                self.state = State::Calm;
                self.calm.clear();
                self.calm.push_back(level);
                target = 0.0;
            } else {
                target = -FADE * self.dir * cap;
            }
        }
        if let Some(informed) = &mut self.informed {
            let started = was_calm && self.state != State::Calm;
            let (share, reset) = informed.step(level, started.then_some((self.dir, self.origin)));
            if reset {
                self.state = State::Calm;
                self.calm.clear();
                self.calm.push_back(level);
            }
            target = share.map_or(target, |share| share * cap);
        }
        if self.halted {
            target = 0.0;
        }
        toward(orders, held, target, mark);
        Ok(())
    }
}

/// Steps without a new extreme that end a leg: a ramp's steps are at most 6
/// slots (3 steps) apart, a pause between legs at least 30.
const LEG_QUIET_STEPS: u32 = 6;
/// How far past the extreme, in percent of the open, a move must go to be
/// a new leg (or the extension) rather than wobble.
const NEW_LEG_PCT: f64 = 0.5;
/// A move the shock's way that gives back more than this share of itself
/// before its legs are done was a fizzle, not the shock.
const FIZZLE_GIVE_BACK: f64 = 0.5;
/// The least a reversion recovers, as a share of the shock (the family's).
const LEAST_RECOVERY: f64 = 0.5;

/// What the informed bot knows of a session: the regime the family drew
/// for it (`MTX_REGIME`), and not its path. The shock's direction, how
/// many legs it comes in, and whether its aftermath reverts, stalls or
/// extends: whose forced flow it is and how much of it there is to come,
/// which a desk that saw every account's positions and stops could know.
/// Not how far each leg goes, when it comes, where the aftermath ends, or
/// whether the session ends first: that is the session's randomness. The
/// grader hands it the draw's whole shape; it reads only these three.
///
/// It trades as the reference does until a move the shock's way starts,
/// then holds the full cap with the shock through its legs and pauses,
/// counting them. At the extreme, it takes the full cap against a shock
/// that reverts, until the mark has recovered at least what every reversion
/// does and stopped; with one that extends, it keeps the cap with it until
/// the extension stops; a stall it leaves alone. After that, only fizzles
/// are left: the reference's trading again. A move the shock's way that
/// gives most of itself back before its legs are done was a fizzle, and it
/// goes back to waiting.
pub struct Informed {
    shock: f64,
    legs: u32,
    aftermath: Aftermath,
    phase: Phase,
    origin: f64,
    extreme: f64,
    quiet: u32,
    legs_done: u32,
    /// The aftermath's farthest level its way.
    far: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Aftermath {
    Revert,
    Stall,
    Extend,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// Before the shock: the reference's trading.
    Waiting,
    /// In a leg the shock's way, or a pause between legs.
    Shock,
    /// Past the last leg, trading the aftermath.
    Aftermath,
    /// The aftermath done: the reference's trading.
    After,
}

impl Informed {
    /// The regime's direction, legs and aftermath. The shape is flat JSON
    /// the grader writes, so it is read without a JSON parser: the solution
    /// builds with the starter's dependencies alone.
    fn from_regime(regime: &str) -> BotResult<Self> {
        let field = |name: &str| -> Option<&str> {
            let at = regime.find(&format!("\"{name}\":"))? + name.len() + 3;
            let rest = regime[at..].trim_start();
            let end = rest.find([',', '}']).unwrap_or(rest.len());
            Some(rest[..end].trim().trim_matches('"'))
        };
        let shock = match field("direction") {
            Some("crash") => -1.0,
            Some("squeeze") => 1.0,
            _ => return Err("MTX_REGIME: no direction".into()),
        };
        let aftermath = match field("aftermath") {
            Some("revert") => Aftermath::Revert,
            Some("stall") => Aftermath::Stall,
            Some("extend") => Aftermath::Extend,
            _ => return Err("MTX_REGIME: no aftermath".into()),
        };
        let legs = field("legs")
            .and_then(|l| l.parse().ok())
            .ok_or("MTX_REGIME: no legs")?;
        Ok(Self {
            shock,
            legs,
            aftermath,
            phase: Phase::Waiting,
            origin: 0.0,
            extreme: 0.0,
            quiet: 0,
            legs_done: 0,
            far: 0.0,
        })
    }

    /// The position this step, as a share of the cap (`None`: the
    /// reference's), and whether the reference should start afresh from
    /// calm. `started` is the reference's move starting this step: its
    /// direction and the calm mean it left.
    fn step(&mut self, level: f64, started: Option<(f64, f64)>) -> (Option<f64>, bool) {
        let shock = self.shock;
        match self.phase {
            Phase::Waiting => match started {
                Some((dir, origin)) if dir == shock => {
                    (self.phase, self.origin, self.extreme) = (Phase::Shock, origin, level);
                    (self.quiet, self.legs_done) = (0, 0);
                    (Some(shock), false)
                }
                _ => (None, false),
            },
            Phase::Shock => {
                if shock * (level - self.extreme) > 0.0 {
                    // A new extreme: the leg runs on; in a pause, past
                    // wobble, the next leg has started.
                    let pausing = self.quiet >= LEG_QUIET_STEPS;
                    if !pausing || shock * (level - self.extreme) >= NEW_LEG_PCT {
                        (self.extreme, self.quiet) = (level, 0);
                    } else {
                        self.quiet += 1;
                    }
                } else {
                    self.quiet += 1;
                }
                let size = shock * (self.extreme - self.origin);
                let first =
                    self.legs_done == 0 || (self.legs_done == 1 && self.quiet >= LEG_QUIET_STEPS);
                if first && shock * (level - self.origin) < (1.0 - FIZZLE_GIVE_BACK) * size {
                    self.phase = Phase::Waiting;
                    return (Some(0.0), true);
                }
                if self.quiet == LEG_QUIET_STEPS {
                    self.legs_done += 1;
                    if self.legs_done >= self.legs {
                        if self.aftermath == Aftermath::Stall {
                            self.phase = Phase::After;
                            return (Some(0.0), true);
                        }
                        (self.phase, self.far, self.quiet) = (Phase::Aftermath, self.extreme, 0);
                    }
                }
                (Some(shock), false)
            }
            Phase::Aftermath => {
                let way = if self.aftermath == Aftermath::Revert {
                    -shock
                } else {
                    shock
                };
                if way * (level - self.far) > 0.0 {
                    (self.far, self.quiet) = (level, 0);
                } else {
                    self.quiet += 1;
                }
                let moved = way * (self.far - self.extreme);
                let landed = if self.aftermath == Aftermath::Revert {
                    moved >= LEAST_RECOVERY * (self.extreme - self.origin).abs()
                } else {
                    moved >= NEW_LEG_PCT
                };
                if landed && self.quiet >= LEG_QUIET_STEPS {
                    self.phase = Phase::After;
                    return (Some(0.0), true);
                }
                (Some(way), false)
            }
            Phase::After => (None, false),
        }
    }
}

/// Market orders moving the position from `held` toward `target`, by up to
/// [`MAX_STEP_TRADE`] this step, in slices of [`SLICE`].
fn toward(orders: &mut Orders, held: f64, target: f64, mark: f64) {
    let mut missing = (target - held).clamp(-MAX_STEP_TRADE, MAX_STEP_TRADE);
    while missing.abs() >= 1.0 {
        let size = missing.abs().floor().min(SLICE);
        if missing > 0.0 {
            orders.buy(size, round_cents(mark * 1.02));
        } else {
            orders.sell(size, round_cents(mark * 0.98));
        }
        missing -= size.copysign(missing);
    }
}

fn round_cents(usd: f64) -> f64 {
    (usd * 100.0).round() / 100.0
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Reference);
