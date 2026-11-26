//! Reference: read the tape, and ride the move it calls, with a trailing
//! stop.
//!
//! On mainnet's Phoenix tape (`phoenix-tape.20261125.fc443f1e`) a jump of
//! 1.5% or more went on by half of itself again or more 45% of the time,
//! fifteen minutes on, and gave half of itself back 31% of the time; most
//! small moves (0.5 to 1.5%) didn't give theirs back either; and NVDA's
//! takers were the most informed measured. The family plays those odds, so
//! a shock is more often a move to ride than one to fade, and the takers
//! ahead of it say which way. The reference reads two things each step:
//! the mark, against a running mean of the calm before, and the tape, read
//! off the book.
//!
//! - **The tape.** Phoenix's makers re-centre on the index as the price
//!   moves, so only taking walks the book's mid off the index. The walk
//!   over the last ten steps is the net taking: 8 bp of it is a lean. It
//!   doesn't read it for twelve steps after its own trade, which walked the
//!   book too. It can't tell who took: informed traders, chasers and noise
//!   all count.
//! - **Calm.** A displacement of 1.2% from the calm mean, or a single step
//!   of 1.9%, is a move: above the wobble and most small moves. A lean,
//!   with no move yet, is a move the tape says is coming.
//! - **Leaning.** The full cap the tape's way. A move its way confirms it;
//!   a move the other way, the tape turning, or thirty steps with no move
//!   close it.
//! - **Riding.** The full cap the move's way, held while it holds. Once the
//!   mark gives back 1.5% or 30% of the move from its extreme, whichever is
//!   more, or the tape leans 8 bp against it, it closes and is calm again.
//!
//! It never carries a session past a $600 drawdown: past that it closes and
//! stops. It holds to the session's end, which says nothing of the path.
//!
//! The mark-only rules were tuned offline on 600 draws of the family (on
//! SPCX, then checked on NVDA's); the tape's thresholds were read off 24
//! graded NVDA sessions, where net taking walked the book 10 to 20 bp the
//! move's way in the ten seconds before most jumps, and set once. Without
//! the tape it made 0.15 of the informed bot. It doesn't know who is
//! informed: the informed bot, told their wallets, reads them alone.
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
/// The tape: how far, in bp, net taking walked the book from Phoenix's
/// index over the last [`FLOW_STEPS`] steps, before a move to lean into
/// it, or against a ride to close it.
const LEAN_BPS: f64 = 8.0;
const FLOW_STEPS: usize = 10;
/// A lean the price doesn't follow is closed after this many steps.
const LEAN_STEPS: u32 = 30;
/// After its own trade it doesn't read the tape for this many steps: its
/// own orders walked the book.
const OWN_QUIET_STEPS: u32 = FLOW_STEPS as u32 + 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum State {
    #[default]
    Calm,
    /// In ahead of a move the tape says is coming, not yet confirmed.
    Leaning,
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
    /// The book's premium over the index, in bp, each step (the last
    /// [`FLOW_STEPS`] + 1), the steps since its own last trade, and how long
    /// it has leaned.
    premium_bps: VecDeque<f64>,
    since_own_trade: u32,
    leaned_steps: u32,
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
        let flow_bps = self.read_tape(market);

        if self.state == State::Calm {
            self.watch(level_pct, step_pct, flow_bps);
        }
        if self.state == State::Leaning {
            self.lean(level_pct, step_pct, flow_bps);
        }
        if self.state == State::Riding {
            self.ride(level_pct, flow_bps);
        }
        let target = match (halted, self.state) {
            (true, _) | (false, State::Calm) => 0.0,
            (false, State::Leaning | State::Riding) => self.direction * self.cap,
        };
        assert!(target.abs() <= self.cap);
        if (target - held).abs() >= 1.0 {
            self.since_own_trade = 0;
        }
        toward(orders, held, target, mark);
        Ok(())
    }
}

impl Reference {
    /// The tape, read off the book: how far net taking walked the book's
    /// mid from Phoenix's index over the last [`FLOW_STEPS`] steps, in bp
    /// (positive: buyers took). The makers re-centre on the index as the
    /// price moves, so only taking walks the book off it. `None` while its
    /// own orders are in the window, or the book is one-sided.
    fn read_tape(&mut self, market: &Market) -> Option<f64> {
        self.since_own_trade = self.since_own_trade.saturating_add(1);
        let (Some(bid), Some(ask)) = (market.best_bid, market.best_ask) else {
            self.premium_bps.clear();
            return None;
        };
        if market.index <= 0.0 || ask < bid {
            self.premium_bps.clear();
            return None;
        }
        self.premium_bps
            .push_back(((bid + ask) / 2.0 / market.index - 1.0) * 10_000.0);
        while self.premium_bps.len() > FLOW_STEPS + 1 {
            self.premium_bps.pop_front();
        }
        if self.premium_bps.len() <= FLOW_STEPS || self.since_own_trade <= OWN_QUIET_STEPS {
            return None;
        }
        let first = *self.premium_bps.front().expect("a full window");
        let last = *self.premium_bps.back().expect("a full window");
        Some(last - first)
    }

    /// Calm: keep the mean, and start riding a move past it, or lean into
    /// one the tape says is coming.
    fn watch(&mut self, level_pct: f64, step_pct: f64, flow_bps: Option<f64>) {
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
            if let Some(flow) = flow_bps.filter(|f| f.abs() >= LEAN_BPS) {
                self.state = State::Leaning;
                self.leaned_steps = 0;
                (self.origin_pct, self.extreme_pct, self.direction) =
                    (mean, level_pct, flow.signum());
            }
            return;
        };
        self.state = State::Riding;
        (self.origin_pct, self.extreme_pct, self.direction) = (mean, level_pct, direction);
    }

    /// Leaning: a move the lean's way confirms it; one the other way, the
    /// tape turning, or a move that doesn't come, closes it.
    fn lean(&mut self, level_pct: f64, step_pct: f64, flow_bps: Option<f64>) {
        assert!(self.direction == 1.0 || self.direction == -1.0);
        self.leaned_steps += 1;
        let moved = level_pct - self.origin_pct;
        if self.direction * moved >= ENTER_PCT || self.direction * step_pct >= STEP_PCT {
            self.state = State::Riding;
            self.extreme_pct = level_pct;
            return;
        }
        let against = -self.direction * moved >= ENTER_PCT
            || flow_bps.is_some_and(|f| self.direction * f <= -LEAN_BPS);
        if against || self.leaned_steps >= LEAN_STEPS {
            self.calm_again(level_pct);
        }
    }

    /// Riding: follow the extreme, and stop out on a retrace from it, or
    /// when the tape turns against it.
    fn ride(&mut self, level_pct: f64, flow_bps: Option<f64>) {
        assert!(self.direction == 1.0 || self.direction == -1.0);
        if self.direction * (level_pct - self.extreme_pct) > 0.0 {
            self.extreme_pct = level_pct;
        }
        let size = (self.extreme_pct - self.origin_pct).abs();
        let retrace = self.direction * (self.extreme_pct - level_pct);
        assert!(retrace >= 0.0, "the extreme is the furthest the move went");
        let turned = flow_bps.is_some_and(|f| self.direction * f <= -LEAN_BPS);
        if retrace >= STOP_PCT.max(STOP_SHARE * size) || turned {
            self.calm_again(level_pct);
        }
    }

    fn calm_again(&mut self, level_pct: f64) {
        self.state = State::Calm;
        self.calm.clear();
        self.calm.push_back(level_pct);
    }
}

#[cfg(target_arch = "wasm32")]
mtx_kit::export!(Reference);
