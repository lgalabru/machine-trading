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
//! scored against.

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
}

impl Strategy for Reference {
    fn start(&mut self, session: &Session) -> BotResult {
        self.cap = session.cap;
        self.start_mark = session.start_mark;
        self.last_mark = session.start_mark;
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
        if self.halted {
            target = 0.0;
        }
        toward(orders, held, target, mark);
        Ok(())
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
