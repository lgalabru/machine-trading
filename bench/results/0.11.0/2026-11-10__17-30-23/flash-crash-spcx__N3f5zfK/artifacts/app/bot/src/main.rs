//! Shock trader for SPCX.
//!
//! Two edges, both measured on scenarios drawn with `mtx scenario generate`:
//! a move that has started tends to keep landing over the next few steps, so
//! ride it from its first jump; and a small move that has stopped is often a
//! fizzle that gives much of itself back within seconds, so fade it for a
//! while. Holding through the long pause after a shock is a coin flip (the
//! next move is as likely another leg as the revert), so the bot is flat
//! there. The position is kept inside what the drawdown limit can absorb in
//! one step.

use mtx_kit::prelude::*;

/// A step move of at least this much (% of the starting mark) is a jump.
const J: f64 = 0.2;
/// A run of jumps ends after this many steps without another.
const G: u64 = 3;
/// A run between these sizes (%) may be a fizzle: fade it for `FZ_T` steps.
const FZ_MIN: f64 = 0.35;
const FZ: f64 = 2.2;
const FZ_T: u64 = 28;
/// The most to trade in one step: a ride is urgent, a fade is not.
const PACE_RIDE: f64 = 140.0;
const PACE_FADE: f64 = 70.0;
/// Drawdown guard: keep `DD_BUF` USD spare after a `WORST`% step against the
/// position, and stop trading for the session past `DD_HALT`.
const DD_BUF: f64 = 300.0;
const WORST: f64 = 5.0;
const DD_HALT: f64 = 600.0;

#[derive(Default)]
struct Shock {
    m0: f64,
    prev: f64,
    run_active: bool,
    run_dir: f64,
    run_mag: f64,
    run_last: u64,
    fz_dir: f64,
    fz_until: u64,
    // Own estimate of the account's value, counting slippage.
    last_pos: f64,
    last_mark: f64,
    eq: f64,
    peak: f64,
    halted: bool,
}

impl Shock {
    fn end_run(&mut self, k: u64) {
        let (r, mag) = (self.run_dir, self.run_mag);
        self.run_active = false;
        let fz = self.fz_dir != 0.0 && k < self.fz_until;
        if fz && r == self.fz_dir {
            // The fizzle gave back: done with it.
            self.fz_dir = 0.0;
        } else if (FZ_MIN..FZ).contains(&mag) {
            self.fz_dir = -r;
            self.fz_until = k + FZ_T;
        }
    }

    /// The position to be at after this step. `price` is the oracle: the mark
    /// follows it, but also drifts with the book, which reads as false jumps.
    fn target(&mut self, k: u64, price: f64, pos: f64, cap: f64, dd: f64) -> f64 {
        if self.m0 == 0.0 {
            self.m0 = price;
            self.prev = price;
        }
        let d = (price - self.prev) / self.m0 * 100.0;
        self.prev = price;
        if d.abs() >= J {
            let sg = d.signum();
            if self.run_active && sg == self.run_dir {
                self.run_mag += d.abs();
            } else {
                if self.run_active {
                    self.end_run(k);
                }
                self.run_active = true;
                self.run_dir = sg;
                self.run_mag = d.abs();
            }
            self.run_last = k;
        } else if self.run_active && k - self.run_last >= G {
            self.end_run(k);
        }
        let fz = self.fz_dir != 0.0 && k < self.fz_until;
        let tgt = if self.run_active {
            self.run_dir * cap
        } else if fz {
            self.fz_dir * cap
        } else {
            0.0
        };
        if dd >= DD_HALT {
            self.halted = true;
        }
        let room = 1000.0 - dd - DD_BUF;
        let mut lim = (room / (WORST / 100.0 * self.m0)).floor().clamp(0.0, cap);
        if self.halted {
            lim = 0.0;
        }
        let tgt = tgt.clamp(-lim, lim);
        let riding = self.run_active && tgt * self.run_dir > 0.0;
        // Getting out is never paced; getting into a fade is.
        let exiting = tgt.abs() < pos.abs() && tgt * pos >= 0.0;
        let pace = if riding || exiting { PACE_RIDE } else { PACE_FADE };
        // Paced toward the target, but inside the guard's limit at once.
        tgt.clamp(pos - pace, pos + pace).clamp(-lim, lim).round()
    }
}

impl Strategy for Shock {
    async fn step(&mut self, market: &Market, orders: &mut Orders<'_>) -> BotResult {
        let pos = market.position.size;
        // Mark-to-market since the last step, on the position held through it,
        // less an estimate of what the last step's trade cost in slippage.
        if self.last_mark != 0.0 {
            self.eq += self.last_pos * (market.mark - self.last_mark);
            let q = (pos - self.last_pos).abs();
            self.eq -= q * 0.15;
        }
        self.last_pos = pos;
        self.last_mark = market.mark;
        self.peak = self.peak.max(self.eq);
        let dd = (self.peak - self.eq).max(market.drawdown);
        if std::path::Path::new("/tmp/bot_debug").exists() {
            eprintln!(
                "S k={} mark={:.2} oracle={:.2} pos={} entry={:.3} dd={:.0}",
                market.k, market.mark, market.oracle, pos, market.position.entry_usd, dd
            );
        }
        let tgt = self.target(market.k, market.oracle, pos, market.cap, dd);
        if market.last {
            return Ok(());
        }
        // Small differences (the guard's limit drifting with the mark) aren't
        // worth a trade, unless they're the last of an exit.
        let diff = (tgt - pos).abs();
        if diff >= 5.0 || (diff >= 1.0 && tgt == 0.0) {
            orders.target_position(tgt);
        }
        Ok(())
    }
}

fn main() -> BotResult {
    mtx_kit::run(Shock::default())
}
