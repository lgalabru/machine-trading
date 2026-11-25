//! Reference: ride a confirmed move, with a trailing stop.
//!
//! On mainnet's Phoenix tape (`phoenix-tape.20261125.fc443f1e`) a jump of
//! 1.5% or more went on by half of itself again or more 45% of the time,
//! fifteen minutes on, and gave half of itself back 31% of the time; most
//! small moves (0.5 to 1.5%) didn't give theirs back either. The family
//! plays those odds, so a shock is more often a move to ride than one to
//! fade. The reference reads one thing, the mark each step, against a
//! running mean of the calm before:
//!
//! - **Calm.** A displacement of 1.2% from the calm mean, or a single step
//!   of 1.9%, is a move: above the wobble and most small moves.
//! - **Riding.** The full cap the move's way, held while it holds. Once the
//!   mark gives back 1.5% or 30% of the move from its extreme, whichever is
//!   more, it closes and is calm again: a reversion costs it the stop, an
//!   extension pays it, a stall leaves it about where it was.
//!
//! It never carries a session past a $600 drawdown: past that it closes and
//! stops. It holds to the session's end, which says nothing of the path.
//!
//! Tuned offline on 600 draws of the family (on SPCX, then checked on
//! NVDA's): on 600 NVDA draws, $137 a session, a per-session Sharpe of
//! 0.42, profitable in half of them, deepest drawdown $768. It doesn't
//! know a shock is coming, and enters after the first leg has landed: the
//! informed bot, told who trades on information, is in before it.
//!
//! It builds two ways: a strategy component (`bot.wasm`, the task's oracle
//! solution), and a process (`src/main.rs`), the anchor hidden sets are
//! scored against.

pub mod rider;

use std::collections::VecDeque;

use mtx_kit::prelude::*;
use rider::{Account, toward};

/// A displacement from the calm mean, in percent of the start, that starts
/// a move.
const ENTER_PCT: f64 = 1.2;
/// A single step's move, in percent of the start, that starts one.
const STEP_PCT: f64 = 1.9;
/// The stop: the least retrace from the extreme, in percent of the start,
/// and the share of the move's size it grows to.
const STOP_PCT: f64 = 1.5;
const STOP_SHARE: f64 = 0.3;
/// Past this drawdown, in USD, it closes and stops: 60% of the mandate's
/// $1,000, so the stop's slippage through a gap can't take it past.
const HALT_DRAWDOWN_USD: f64 = 600.0;
/// Steps the calm mean averages over: a minute's half.
const CALM_STEPS: usize = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Calm,
    Riding,
}

/// The strategy.
#[derive(Default)]
pub struct Reference {
    cap: f64,
    start_mark: f64,
    calm: VecDeque<f64>,
    state: State,
    /// The calm mean the move started from, its extreme, both in percent
    /// of the start, and its direction (+1 up, -1 down).
    origin_pct: f64,
    extreme_pct: f64,
    direction: f64,
    last_pct: Option<f64>,
    account: Account,
}

impl Strategy for Reference {
    fn start(&mut self, session: &Session) -> BotResult {
        assert!(session.cap > 0.0 && session.start_mark > 0.0);
        self.cap = session.cap;
        self.start_mark = session.start_mark;
        Ok(())
    }

    fn step(&mut self, market: &Market, orders: &mut Orders) -> BotResult {
        let (mark, held) = (market.mark, market.position.size);
        let halted = self.account.step(mark, held, HALT_DRAWDOWN_USD);
        let level_pct = (mark / self.start_mark - 1.0) * 100.0;
        let step_pct = self.last_pct.map_or(0.0, |last| level_pct - last);
        self.last_pct = Some(level_pct);

        if self.state == State::Calm {
            self.watch(level_pct, step_pct);
        }
        if self.state == State::Riding {
            self.ride(level_pct);
        }
        let target = match (halted, self.state) {
            (true, _) | (false, State::Calm) => 0.0,
            (false, State::Riding) => self.direction * self.cap,
        };
        assert!(target.abs() <= self.cap);
        toward(orders, held, target, mark);
        Ok(())
    }
}

impl Reference {
    /// Calm: keep the mean, and start riding a move past it.
    fn watch(&mut self, level_pct: f64, step_pct: f64) {
        self.calm.push_back(level_pct);
        while self.calm.len() > CALM_STEPS {
            self.calm.pop_front();
        }
        let mean = self.calm.iter().sum::<f64>() / self.calm.len() as f64;
        let displacement = level_pct - mean;
        let direction = if displacement.abs() >= ENTER_PCT {
            displacement.signum()
        } else if step_pct.abs() >= STEP_PCT {
            step_pct.signum()
        } else {
            return;
        };
        self.state = State::Riding;
        (self.origin_pct, self.extreme_pct, self.direction) = (mean, level_pct, direction);
    }

    /// Riding: follow the extreme, and stop out on a retrace from it.
    fn ride(&mut self, level_pct: f64) {
        assert!(self.direction == 1.0 || self.direction == -1.0);
        if self.direction * (level_pct - self.extreme_pct) > 0.0 {
            self.extreme_pct = level_pct;
        }
        let size = (self.extreme_pct - self.origin_pct).abs();
        let retrace = self.direction * (self.extreme_pct - level_pct);
        assert!(retrace >= 0.0, "the extreme is the furthest the move went");
        if retrace >= STOP_PCT.max(STOP_SHARE * size) {
            self.state = State::Calm;
            self.calm.clear();
            self.calm.push_back(level_pct);
        }
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Reference);
