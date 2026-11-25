//! What the reference and the informed bot share: following the account's
//! value to halt past a drawdown, and moving the position toward a target
//! in market orders a transaction can hold. The informed bot includes this
//! file (`#[path]`), so the two trade with the same mechanics and differ
//! only in what sets the target.

use mtx_kit::prelude::*;

/// Shares one market order carries: one that crosses much of the book
/// doesn't fit a transaction's compute budget.
pub const SLICE: f64 = 10.0;
/// The most the position moves in one step: half the cap, so a full turn
/// takes two steps and one step's mistake costs half as much.
pub const MAX_STEP_TRADE: f64 = 35.0;
/// How far past the mark a market order may fill: wide enough to fill
/// through a fast move, the mandate's cap bounds what it can cost.
const LIMIT_SHARE: f64 = 0.02;

/// The account's value as the strategy follows it, from the mark and the
/// position: a halt once it has fallen `halt_usd` below its peak.
#[derive(Debug, Default)]
pub struct Account {
    value_usd: f64,
    peak_usd: f64,
    last_mark: Option<f64>,
    halted: bool,
}

impl Account {
    /// Follow one step: the position held through the move to `mark`.
    /// Returns whether the strategy is halted.
    pub fn step(&mut self, mark: f64, held: f64, halt_usd: f64) -> bool {
        assert!(mark > 0.0, "a mark is a price");
        assert!(halt_usd > 0.0);
        if let Some(last) = self.last_mark {
            self.value_usd += held * (mark - last);
        }
        self.last_mark = Some(mark);
        self.peak_usd = self.peak_usd.max(self.value_usd);
        assert!(self.peak_usd >= self.value_usd);
        if !self.halted && self.peak_usd - self.value_usd > halt_usd {
            mtx_kit::log("drawdown past the halt; closing and stopping");
            self.halted = true;
        }
        self.halted
    }
}

/// Market orders moving the position from `held` toward `target`, by up to
/// [`MAX_STEP_TRADE`] this step, in slices of [`SLICE`].
pub fn toward(orders: &mut Orders, held: f64, target: f64, mark: f64) {
    assert!(mark > 0.0);
    let mut missing = (target - held).clamp(-MAX_STEP_TRADE, MAX_STEP_TRADE);
    let mut slices = 0;
    while missing.abs() >= 1.0 {
        let size = missing.abs().floor().min(SLICE);
        if missing > 0.0 {
            orders.buy(size, round_cents(mark * (1.0 + LIMIT_SHARE)));
        } else {
            orders.sell(size, round_cents(mark * (1.0 - LIMIT_SHARE)));
        }
        missing -= size.copysign(missing);
        slices += 1;
        assert!(slices <= (MAX_STEP_TRADE / SLICE).ceil() as u32);
    }
}

fn round_cents(usd: f64) -> f64 {
    (usd * 100.0).round() / 100.0
}
