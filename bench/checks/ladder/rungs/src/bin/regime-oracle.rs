//! Rung `regime-oracle` (flash-crash): the reference's trading, told the
//! session's regime, not its path. The grader hands it the draw's shape as
//! `MTX_REGIME` (ladder runs only, `mtx grade --tell-regime`): the shock's
//! extreme and where the aftermath ends.
//!
//! It trades as the reference does (ride a move while it runs, fade it once
//! it stops, among fizzles it can't tell apart), and uses the shape where
//! the reference has to guess:
//!
//! - **Before the extreme**, a move the shock's way is the shock: it holds
//!   the full cap with it, through pauses, until the extreme. A move the
//!   other way is a fizzle, traded as the reference trades it.
//! - **At the extreme**, it takes the full cap toward where the aftermath
//!   ends, if that's worth the round trip, and holds until it gets there.
//! - **After**, only fizzles are left: the reference's trading again.
//!
//! Until 0.11.0 it only took the extreme-to-end trade and never traded the
//! rest of the session, so it made about what the reference made, and an
//! exploit bot trading every part better beat it without reading the
//! generator. Built on the reference, its lead is what knowing the regime
//! adds to good trading: the ceiling the exploit bots must stay under.

use std::collections::VecDeque;

use anyhow::Result;
use mtx_botkit::{BotEnv, Side, Trader, Turns};
use mtx_ladder_rungs::init_logs;
use serde::Deserialize;
use tracing::{info, warn};

// The reference's parameters (bench/tasks/flash-crash-spcx/solution).
const ENTER_PCT: f64 = 1.14;
const STEP_PCT: f64 = 1.36;
const END_STEPS: u32 = 3;
const RETRACE_PCT: f64 = 1.41;
const FADE: f64 = 0.76;
const GIVE_BACK: f64 = 0.11;
const MIN_FADE_PCT: f64 = 1.03;
const HALT_DRAWDOWN_USD: f64 = 519.0;
const CALM_STEPS: usize = 30;
const SLICE: f64 = 10.0;
const MAX_STEP_TRADE: f64 = 35.0;

/// How close to a level, in percent of the open, counts as there.
const NEAR_PCT: f64 = 0.5;
/// Smallest move from the extreme to the end worth the round trip.
const WORTH_PCT: f64 = 1.0;

#[derive(Deserialize)]
struct Shape {
    extreme_pct: f64,
    end_pct: f64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Calm,
    Moving,
    Holding,
}

/// The reference's state machine, as it is in its solution.
struct Reference {
    calm: VecDeque<f64>,
    state: State,
    origin: f64,
    extreme: f64,
    dir: f64,
    quiet: u32,
    last: Option<f64>,
}

impl Reference {
    fn new() -> Self {
        Self {
            calm: VecDeque::new(),
            state: State::Calm,
            origin: 0.0,
            extreme: 0.0,
            dir: 0.0,
            quiet: 0,
            last: None,
        }
    }

    fn reset(&mut self, level: f64) {
        self.state = State::Calm;
        self.calm.clear();
        self.calm.push_back(level);
    }

    /// The position the reference would want, as a share of the cap.
    fn step(&mut self, level: f64) -> f64 {
        let jump = self.last.map_or(0.0, |l| level - l);
        self.last = Some(level);
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
            target = self.dir;
        }
        if self.state == State::Holding {
            let size = (self.extreme - self.origin).abs();
            if self.dir * (level - self.extreme) >= ENTER_PCT {
                (self.state, self.extreme, self.quiet) = (State::Moving, level, 0);
                target = self.dir;
            } else if self.dir * (level - self.origin) <= GIVE_BACK * size || size < MIN_FADE_PCT {
                self.reset(level);
                target = 0.0;
            } else {
                target = -FADE * self.dir;
            }
        }
        target
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Phase {
    /// Before the extreme.
    Shock,
    /// At the extreme, trading toward the end.
    Aftermath,
    /// The end reached, or not worth trading toward: fizzles only.
    After,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_logs();
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let shape: Shape = serde_json::from_str(&std::env::var("MTX_REGIME")?)?;
    let trader = Trader::connect(&env)?;
    let cap = env.max_position;
    let start_mark = trader.mark_usd().await?;
    let shock = shape.extreme_pct.signum();
    let toward_end = shape.end_pct - shape.extreme_pct;
    info!(shape.extreme_pct, shape.end_pct, "told the regime");

    let mut reference = Reference::new();
    let mut phase = Phase::Shock;
    let (mut value, mut peak_value, mut last_mark) = (0.0_f64, 0.0_f64, start_mark);
    let mut halted = false;
    while turns.next()?.is_some() {
        let (Ok(mark), Ok(position)) = (trader.mark_usd().await, trader.position().await) else {
            continue;
        };
        let level = (mark / start_mark - 1.0) * 100.0;
        value += position.size * (mark - last_mark);
        last_mark = mark;
        peak_value = peak_value.max(value);
        if !halted && peak_value - value > HALT_DRAWDOWN_USD {
            warn!(
                value,
                peak_value, "drawdown past the halt; closing and stopping"
            );
            halted = true;
        }

        let wanted = reference.step(level);
        if phase == Phase::Shock && shock * level >= shape.extreme_pct.abs() - NEAR_PCT {
            phase = if toward_end.abs() >= WORTH_PCT {
                Phase::Aftermath
            } else {
                Phase::After
            };
            info!(level, ?phase, "at the extreme");
        }
        if phase == Phase::Aftermath && toward_end.signum() * (level - shape.end_pct) >= -NEAR_PCT {
            phase = Phase::After;
            reference.reset(level);
            info!(level, "at the end");
        }
        let share = match phase {
            // The shock's way, it's the shock: hold it to the extreme.
            Phase::Shock if reference.state != State::Calm && reference.dir == shock => shock,
            Phase::Shock => wanted,
            Phase::Aftermath => toward_end.signum(),
            Phase::After => wanted,
        };
        let target = if halted { 0.0 } else { share * cap };
        toward(&trader, position.size, target, mark).await;
    }
    Ok(())
}

/// Move the position from `held` toward `target`, by up to
/// [`MAX_STEP_TRADE`] this step, in market orders of [`SLICE`].
async fn toward(trader: &Trader, held: f64, target: f64, mark: f64) {
    let mut missing = (target - held).clamp(-MAX_STEP_TRADE, MAX_STEP_TRADE);
    while missing.abs() >= 1.0 {
        let size = missing.abs().floor().min(SLICE);
        let (side, limit) = if missing > 0.0 {
            (Side::Bid, mark * 1.02)
        } else {
            (Side::Ask, mark * 0.98)
        };
        if let Err(e) = trader
            .place_market(side, size, (limit * 100.0).round() / 100.0)
            .await
        {
            warn!(%e, "order failed; retrying next step");
            return;
        }
        missing -= size.copysign(missing);
    }
}
