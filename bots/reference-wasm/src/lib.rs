//! The flash-crash reference (`bots/reference`) as a wasm strategy: ride each
//! move while it runs, then fade it once it stops. The same rules and
//! constants; the grader reads the mark and the position and sends the
//! orders, instead of the bot.

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

#[derive(Default)]
struct Reference {
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
            mtx_kit::component::log("drawdown past the halt; closing and stopping");
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

mtx_kit::export!(Reference);
