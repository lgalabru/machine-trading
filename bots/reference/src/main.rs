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

use std::collections::VecDeque;

use anyhow::Result;
use mtx_botkit::{BotEnv, Side, Trader, Turns};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

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

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Calm,
    Moving,
    Holding,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();
    // Answer the grader's hello first: the world waits for it.
    let mut turns = Turns::connect()?;
    let env = BotEnv::from_env()?;
    let trader = Trader::connect(&env)?;
    let cap = env.max_position;
    let start_mark = trader.mark_usd().await?;

    let mut calm: VecDeque<f64> = VecDeque::new();
    let mut state = State::Calm;
    let (mut origin, mut extreme, mut dir, mut quiet) = (0.0, 0.0, 0.0, 0);
    let mut last: Option<f64> = None;
    // The account's value from the start, marked each step (the position
    // held times the mark's move), against its peak: to keep clear of the
    // drawdown limit.
    let (mut value, mut peak_value, mut last_mark) = (0.0_f64, 0.0_f64, start_mark);
    let mut halted = false;
    while turns.next()?.is_some() {
        let (Ok(mark), Ok(position)) = (trader.mark_usd().await, trader.position().await) else {
            continue;
        };
        let level = (mark / start_mark - 1.0) * 100.0;
        let jump = last.map_or(0.0, |l| level - l);
        last = Some(level);
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

        let mut target = 0.0;
        if state == State::Calm {
            calm.push_back(level);
            while calm.len() > CALM_STEPS {
                calm.pop_front();
            }
            let mean = calm.iter().sum::<f64>() / calm.len() as f64;
            let displacement = level - mean;
            if displacement.abs() >= ENTER_PCT || jump.abs() >= STEP_PCT {
                dir = if displacement.abs() >= ENTER_PCT {
                    displacement.signum()
                } else {
                    jump.signum()
                };
                state = State::Moving;
                (origin, extreme, quiet) = (mean, level, 0);
                info!(level, dir, "a move");
            }
        }
        if state == State::Moving {
            if dir * (level - extreme) > 0.0 {
                (extreme, quiet) = (level, 0);
            } else {
                quiet += 1;
            }
            if dir * (extreme - level) >= RETRACE_PCT || quiet >= END_STEPS {
                state = State::Holding;
                info!(level, extreme, "the move stopped");
            }
            target = RIDE * dir * cap;
        }
        if state == State::Holding {
            let size = (extreme - origin).abs();
            if dir * (level - extreme) >= ENTER_PCT {
                (state, extreme, quiet) = (State::Moving, level, 0);
                target = RIDE * dir * cap;
                info!(level, "another leg");
            } else if dir * (level - origin) <= GIVE_BACK * size || size < MIN_FADE_PCT {
                state = State::Calm;
                calm.clear();
                calm.push_back(level);
                target = 0.0;
            } else {
                target = -FADE * dir * cap;
            }
        }
        if halted {
            target = 0.0;
        }
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
        if let Err(e) = trader.place_market(side, size, round_cents(limit)).await {
            warn!(%e, "order failed; retrying next step");
            return;
        }
        missing -= size.copysign(missing);
    }
}

fn round_cents(usd: f64) -> f64 {
    (usd * 100.0).round() / 100.0
}
