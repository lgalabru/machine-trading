//! Shock fader for SPCX: fades moves away from a slowly tracked anchor once
//! they stop moving, sized so that a further adverse leg can't breach the
//! drawdown limit, with a hard stop when drawdown room gets thin.

use mtx_kit::prelude::*;
use tracing::info;

// Parameters (fractions of price unless noted).
const ENTRY: f64 = 0.008; // fade moves at least this far from the anchor
const FULL: f64 = 0.012; // full size at this deviation
const STILL: f64 = 0.0015; // "move has stopped": last step moved less than this
const FREEZE: f64 = 0.0035; // anchor stops tracking beyond this deviation
const ANCHOR_TAU_STEPS: f64 = 40.0; // anchor EMA time constant, in steps
const EXIT: f64 = 0.0015; // flatten when back within this of the anchor
const LEG_RISK: f64 = 0.045; // adverse jump to survive in a single step
const DD_BUFFER: f64 = 300.0; // USD kept aside for fees/slippage/estimate error
const MIN_TRADE: f64 = 1.0;
const DD_STOP: f64 = 400.0; // flatten when drawdown room falls below this (USD)
const COST_PER_SHARE: f64 = 0.3; // assumed fees+slippage, USD per share traded
const STILL_N: u32 = 3; // steps still before fading
const RIDE_THR: f64 = 0.002; // a step move this big, away from the anchor, is ridden
const RIDE: f64 = 1.0; // fraction of cap to ride with

#[derive(Default)]
struct Fader {
    anchor: Option<f64>,
    prev: Option<f64>,
    target: f64,
    stopped_out: bool,
    still: u32,
    peak: f64,
    cost_since_peak: f64,
}

impl Strategy for Fader {
    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let mark = market.mark;
        if !(mark > 0.0) {
            return Ok(());
        }
        let anchor = *self.anchor.get_or_insert(mark);
        let prev = self.prev.unwrap_or(mark);
        self.prev = Some(mark);
        let dev = mark / anchor - 1.0;
        let step_move = mark / prev - 1.0;

        // Anchor tracks calm markets only.
        if dev.abs() < FREEZE && self.target == 0.0 {
            self.anchor = Some(anchor + (mark - anchor) / ANCHOR_TAU_STEPS);
        }

        let pos = market.position.size;
        let cap = market.cap;
        // Size limit from drawdown room: a LEG_RISK jump must not breach.
        if market.peak_pnl > self.peak {
            self.peak = market.peak_pnl;
            self.cost_since_peak = 0.0;
        }
        let dd_room = market.drawdown_room() - self.cost_since_peak;
        let room = (dd_room - DD_BUFFER).max(0.0);
        let risk_cap = (room / (mark * LEG_RISK)).min(cap).max(0.0);

        self.still = if step_move.abs() < STILL { self.still + 1 } else { 0 };
        let mut target = self.target;
        if RIDE > 0.0 && step_move.abs() >= RIDE_THR && dev.abs() > RIDE_THR {
            if step_move * dev > 0.0 {
                target = step_move.signum() * RIDE * cap;
            }
        } else if dev.abs() >= ENTRY && self.still >= STILL_N && !self.stopped_out {
            let frac = ((dev.abs() - ENTRY) / (FULL - ENTRY)).clamp(0.0, 1.0);
            let size = (0.4 + 0.6 * frac) * cap;
            let want = -dev.signum() * size;
            // Only scale up in the same direction, never shrink on the way.
            if want.signum() != target.signum() || want.abs() > target.abs() {
                target = want;
            }
        }
        if dev.abs() < EXIT {
            target = 0.0;
            self.stopped_out = false;
        }
        // Drawdown guard.
        if dd_room < DD_STOP {
            target = 0.0;
            self.stopped_out = true;
        }
        let target = target.clamp(-risk_cap, risk_cap);
        self.target = target;
        // Near the end, nothing to do differently: positions are marked.
        if (target - pos).abs() >= MIN_TRADE {
            self.cost_since_peak += (target - pos).abs() * COST_PER_SHARE;
            orders.target_position_with(
                target,
                Execution {
                    slippage_bps: 150.0,
                    ..Execution::default()
                },
            );
        }
        info!(
            k = market.k,
            mark,
            oracle = market.oracle,
            anchor,
            dev,
            pos,
            target,
            pnl = market.pnl,
            dd = market.drawdown,
            "step"
        );
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Fader::default())
}
