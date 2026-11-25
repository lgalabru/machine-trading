//! What the reference and the informed bot share: when they take their
//! position into the reopen, how they size it from the gap they expect, and
//! how they trade toward it. The informed bot includes this file
//! (`#[path]`), so the two differ only in what they expect.

use mtx_kit::{log, prelude::*};

/// How much of the weekend's move the reopen keeps, and the spread of the
/// open around that, in percent: the instruction's numbers, from 21
/// weekends of Phoenix SPCX candles (2026-06-12 to 2026-11-20).
pub const REOPEN_KEEP: f64 = 0.78;
pub const OPEN_RESIDUAL_SD_PCT: f64 = 0.50;

/// The position is decided from this many slots before the reopen: after
/// the makers' last re-price (an hour, compressed, before the reopen: at
/// least 8 slots), with two steps left to get there at 35 SPCX a step.
pub const ENTRY_SLOTS_BEFORE_REOPEN: u64 = 8;
/// The last slot before the reopen it still changes its position at.
pub const LOCK_SLOTS_BEFORE_REOPEN: u64 = 4;

/// The expected gap, in percent, that takes the full cap, and the least it
/// trades for: under about 0.08% the entry's costs (the half-spread,
/// Phoenix's 3.5 bp taker fee and the depth a 70-SPCX take walks, about
/// 5 bp) eat the gap it expects. The position is held to the end, where
/// the mark values it: there is no exit to pay for.
pub const EXPECTED_GAP_FULL_PCT: f64 = 0.25;
pub const EXPECTED_GAP_MIN_PCT: f64 = 0.08;

/// Past this drawdown, in USD, it closes and stops: under the 1,000 limit,
/// with room for the close.
pub const HALT_DRAWDOWN_USD: f64 = 600.0;

/// Market orders of at most this many shares (one order that crosses much
/// of the book doesn't fit a transaction's compute budget), at most this
/// many shares a turn, at most this far past the makers' mid, in percent.
const SLICE: f64 = 10.0;
const MAX_STEP_TRADE: f64 = 35.0;
const SLIPPAGE_PCT: f64 = 1.0;

/// Where a step stands against the reopen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// The weekend, before the position is decided.
    Weekend,
    /// The last slots before the reopen: the position is decided, and may
    /// still change.
    Entry,
    /// From the lock on: the position is held to the end.
    Hold,
}

/// The session's clock against the reopen.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    pub start_slot: u64,
    pub reopen_slot: u64,
}

impl Clock {
    pub fn from_session(session: &Session) -> Result<Self, String> {
        let reopen_slot: u64 = session
            .var("MTX_REOPEN_SLOT")
            .ok_or("the session names no MTX_REOPEN_SLOT")?
            .parse()
            .map_err(|_| "MTX_REOPEN_SLOT isn't a slot")?;
        if reopen_slot <= session.start_slot + ENTRY_SLOTS_BEFORE_REOPEN {
            return Err(format!("the reopen at {reopen_slot} is before the entry"));
        }
        Ok(Self {
            start_slot: session.start_slot,
            reopen_slot,
        })
    }

    pub fn phase(&self, slot: u64) -> Phase {
        assert!(slot >= self.start_slot);
        if slot + LOCK_SLOTS_BEFORE_REOPEN >= self.reopen_slot {
            Phase::Hold
        } else if slot + ENTRY_SLOTS_BEFORE_REOPEN >= self.reopen_slot {
            Phase::Entry
        } else {
            Phase::Weekend
        }
    }
}

/// The position for an expected gap, in percent: the cap's share of it
/// that the gap's size over [`EXPECTED_GAP_FULL_PCT`] is, and nothing under
/// [`EXPECTED_GAP_MIN_PCT`].
pub fn position_for(expected_gap_pct: f64, cap: f64) -> f64 {
    assert!(expected_gap_pct.is_finite() && cap > 0.0);
    if expected_gap_pct.abs() < EXPECTED_GAP_MIN_PCT {
        return 0.0;
    }
    let position = cap * (expected_gap_pct / EXPECTED_GAP_FULL_PCT).clamp(-1.0, 1.0);
    assert!(position.abs() <= cap);
    position
}

/// The gap the reopen is expected to open, in percent of the starting
/// mark, from where the weekend has taken the makers' price.
pub fn expected_gap_pct(weekend_pct: f64) -> f64 {
    (REOPEN_KEEP - 1.0) * weekend_pct
}

/// Its account's value at the mark, its peak and whether it has halted.
#[derive(Debug, Default)]
pub struct Account {
    last_mark: Option<f64>,
    value_usd: f64,
    peak_usd: f64,
    pub halted: bool,
}

impl Account {
    /// Mark the position to `mark`; halt past `halt_usd` below the peak.
    pub fn step(&mut self, mark: f64, held: f64) -> bool {
        if let Some(last) = self.last_mark {
            self.value_usd += held * (mark - last);
        }
        self.last_mark = Some(mark);
        self.peak_usd = self.peak_usd.max(self.value_usd);
        if !self.halted && self.peak_usd - self.value_usd > HALT_DRAWDOWN_USD {
            log("drawdown past the halt; flat from here");
            self.halted = true;
        }
        self.halted
    }
}

/// Market orders toward `target`, in slices, at most [`MAX_STEP_TRADE`] a
/// turn.
pub fn trade_toward(orders: &mut Orders, position: f64, target: f64, mid: f64) {
    assert!(mid > 0.0 && target.is_finite());
    let mut left = (target - position).clamp(-MAX_STEP_TRADE, MAX_STEP_TRADE);
    let mut slices = 0;
    while left.abs() >= 0.5 {
        let size = left.abs().min(SLICE);
        if left > 0.0 {
            orders.buy(size, mid * (1.0 + SLIPPAGE_PCT / 100.0));
        } else {
            orders.sell(size, mid * (1.0 - SLIPPAGE_PCT / 100.0));
        }
        left -= size * left.signum();
        slices += 1;
        assert!(slices <= (MAX_STEP_TRADE / SLICE).ceil() as u32 + 1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_expected_gap_fades_a_fifth_of_the_weekend() {
        assert!((expected_gap_pct(1.0) + 0.22).abs() < 1e-9);
        assert!((expected_gap_pct(-2.0) - 0.44).abs() < 1e-9);
        assert_eq!(position_for(0.05, 70.0), 0.0);
        assert_eq!(position_for(-0.5, 70.0), -70.0);
        assert!((position_for(0.125, 70.0) - 35.0).abs() < 1e-9);
    }

    #[test]
    fn the_clock_decides_late_and_locks_before_the_reopen() {
        let clock = Clock {
            start_slot: 100,
            reopen_slot: 600,
        };
        assert_eq!(clock.phase(100), Phase::Weekend);
        assert_eq!(clock.phase(591), Phase::Weekend);
        assert_eq!(clock.phase(592), Phase::Entry);
        assert_eq!(clock.phase(596), Phase::Hold);
        assert_eq!(clock.phase(700), Phase::Hold);
    }
}
